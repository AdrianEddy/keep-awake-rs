//! macOS inhibition via IOKit power assertions.
//!
//! The two assertion types are the ones `caffeinate` takes for its `-d` and `-i`
//! flags — they suppress the *idle* display and system sleep timers while the
//! assertion is alive:
//!
//! * `kIOPMAssertionTypePreventUserIdleDisplaySleep` for a display inhibition
//!   (which also keeps the system awake, since the system cannot idle-sleep while
//!   the display is required);
//! * `kIOPMAssertionTypePreventUserIdleSystemSleep` for a system inhibition.
//!
//! Deliberately *not* `kIOPMAssertionTypePreventSystemSleep`: that is the stronger
//! server-style assertion which also blocks sleep the user asks for, and which the
//! system honours essentially only on AC power.

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};

/// `IOReturn` is `kern_return_t`, i.e. a C `int`, on every Apple platform.
pub type IOReturn = i32;
pub type IOPMAssertionLevel = u32;
pub type IOPMAssertionId = u32;

/// `kIOReturnSuccess`.
const K_IORETURN_SUCCESS: IOReturn = 0;
// const K_IOPMASSERTION_LEVEL_OFF : IOPMAssertionLevel = 0;
const K_IOPMASSERTION_LEVEL_ON: IOPMAssertionLevel = 255;

/// `kIOPMAssertionTypePreventUserIdleDisplaySleep`.
const ASSERTION_PREVENT_USER_IDLE_DISPLAY_SLEEP: &str = "PreventUserIdleDisplaySleep";
/// `kIOPMAssertionTypePreventUserIdleSystemSleep`.
const ASSERTION_PREVENT_USER_IDLE_SYSTEM_SLEEP: &str = "PreventUserIdleSystemSleep";

unsafe extern "C" {
    pub fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        assertion_level: IOPMAssertionLevel,
        assertion_name: CFStringRef,
        assertion_id: &mut IOPMAssertionId,
    ) -> IOReturn;

    pub fn IOPMAssertionRelease(assertion_id: IOPMAssertionId) -> IOReturn;
}

/// A live assertion. Dropping it releases.
pub struct Holder {
    id: IOPMAssertionId,
}

pub fn inhibit_display(name: &str, reason: &str) -> Result<Holder, Box<dyn std::error::Error>> {
    inhibit(name, reason, ASSERTION_PREVENT_USER_IDLE_DISPLAY_SLEEP)
}

pub fn inhibit_system(name: &str, reason: &str) -> Result<Holder, Box<dyn std::error::Error>> {
    inhibit(name, reason, ASSERTION_PREVENT_USER_IDLE_SYSTEM_SLEEP)
}

fn inhibit(
    name: &str,
    reason: &str,
    assertion_type: &'static str,
) -> Result<Holder, Box<dyn std::error::Error>> {
    let assertion_type_cf = CFString::from_static_string(assertion_type);
    // The assertion name is the only attribution slot IOKit offers, and it is what
    // `pmset -g assertions` prints, so it carries both halves.
    let assertion_name = CFString::new(&format!("{name}: {reason}"));
    let mut id: IOPMAssertionId = 0;
    let ret = unsafe {
        IOPMAssertionCreateWithName(
            assertion_type_cf.as_concrete_TypeRef(),
            K_IOPMASSERTION_LEVEL_ON,
            assertion_name.as_concrete_TypeRef(),
            &mut id,
        )
    };
    if ret != K_IORETURN_SUCCESS {
        return Err(
            format!("IOPMAssertionCreateWithName({assertion_type}) failed: {ret:#010x}").into(),
        );
    }
    Ok(Holder { id })
}

impl Drop for Holder {
    fn drop(&mut self) {
        let ret = unsafe { IOPMAssertionRelease(self.id) };
        if ret != K_IORETURN_SUCCESS {
            log::error!("IOPMAssertionRelease({}) failed: {ret:#010x}", self.id);
        }
    }
}
