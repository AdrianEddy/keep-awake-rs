//! Windows inhibition via **power requests**.
//!
//! Each [`Holder`] owns exactly one power request handle (`PowerCreateRequest`),
//! the request types it successfully set on it, and the attribution string the
//! request was created with; dropping the holder clears those types, closes the
//! handle and then frees the string.
//!
//! This is deliberately *not* `SetThreadExecutionState`, whose contract does not
//! fit an RAII holder at all:
//!
//! * its state is **absolute**, so acquiring a system inhibition while a display
//!   inhibition is live silently clears `ES_DISPLAY_REQUIRED`, and releasing either
//!   one clears both;
//! * its state is **per thread**, so create and release have to happen on the same
//!   long-lived thread;
//! * and the documented way to clear it is `ES_CONTINUOUS` alone — passing `0` is an
//!   invalid flags value, so the call simply fails and the inhibition stays until the
//!   process exits.
//!
//! Power requests have none of those problems: they are per handle, kernel
//! reference-counted, process-scoped (any thread may set or clear them), and they
//! show up in `powercfg /requests` with the `REASON_CONTEXT` attribution below.

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Power::{
    POWER_REQUEST_TYPE, PowerClearRequest, PowerCreateRequest, PowerRequestDisplayRequired,
    PowerRequestSystemRequired, PowerSetRequest,
};
use windows::Win32::System::SystemServices::POWER_REQUEST_CONTEXT_VERSION;
use windows::Win32::System::Threading::{
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
};
use windows::core::PWSTR;

/// A display inhibition keeps the screen lit **and** the system awake. Both types
/// are required, not belt and braces: `PowerSetRequest`'s own documentation says a
/// `PowerRequestSystemRequired` "must be taken in addition to a
/// `PowerRequestDisplayRequired` to ensure the display stays on and the system does
/// not enter sleep for the duration of the request".
///
/// One request object can carry both, because the kernel counts requests per type
/// per object.
const DISPLAY_REQUESTS: &[POWER_REQUEST_TYPE] =
    &[PowerRequestDisplayRequired, PowerRequestSystemRequired];

const SYSTEM_REQUESTS: &[POWER_REQUEST_TYPE] = &[PowerRequestSystemRequired];

/// A live inhibition. Dropping it releases.
pub struct Holder {
    request: HANDLE,
    /// The request types that were successfully set on `request`. Cleared in
    /// reverse order on drop; empty while a partially-failed acquire unwinds.
    set: &'static [POWER_REQUEST_TYPE],
    /// The buffer `REASON_CONTEXT.SimpleReasonString` was made to point at.
    ///
    /// Nothing in the documentation of `REASON_CONTEXT` or `PowerCreateRequest` says
    /// the string is copied, and `powercfg /requests` prints it for as long as the
    /// request is alive, so it is kept alive for exactly that long. Struct fields
    /// drop after `Drop::drop` returns, i.e. after the handle is closed.
    #[allow(dead_code)]
    attribution: Vec<u16>,
}

// A power request handle is an ordinary kernel handle and every power request API is
// process-scoped, so a holder may be created on one thread and dropped on another.
unsafe impl Send for Holder {}
unsafe impl Sync for Holder {}

pub fn inhibit_display(name: &str, reason: &str) -> Result<Holder, Box<dyn std::error::Error>> {
    inhibit(name, reason, DISPLAY_REQUESTS)
}

pub fn inhibit_system(name: &str, reason: &str) -> Result<Holder, Box<dyn std::error::Error>> {
    inhibit(name, reason, SYSTEM_REQUESTS)
}

fn inhibit(
    name: &str,
    reason: &str,
    requests: &'static [POWER_REQUEST_TYPE],
) -> Result<Holder, Box<dyn std::error::Error>> {
    // NUL-terminated UTF-16, because `SimpleReasonString` is a `PWSTR`. The buffer
    // outlives the call: it moves into the holder, and moving a `Vec` does not move
    // its heap allocation, so the pointer stays valid.
    let mut attribution: Vec<u16> = format!("{name}: {reason}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let context = REASON_CONTEXT {
        Version: POWER_REQUEST_CONTEXT_VERSION,
        Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
        Reason: REASON_CONTEXT_0 {
            SimpleReasonString: PWSTR(attribution.as_mut_ptr()),
        },
    };

    // `?` here, and `.into()` in the loop below, convert `windows::core::Error` into
    // `Box<dyn Error>` through its `std::error::Error` impl, which the `windows` crate
    // only has under its default `std` feature. Turning default features off would
    // break both.
    let request = unsafe { PowerCreateRequest(&context) }?;

    // From here on the handle belongs to `holder`, so every exit path — including the
    // partial-failure one below — clears what was set and closes it.
    let mut holder = Holder {
        request,
        set: &[],
        attribution,
    };
    for (already_set, request_type) in requests.iter().enumerate() {
        if let Err(e) = unsafe { PowerSetRequest(holder.request, *request_type) } {
            holder.set = &requests[..already_set];
            return Err(e.into());
        }
    }
    holder.set = requests;
    Ok(holder)
}

impl Drop for Holder {
    fn drop(&mut self) {
        for request_type in self.set.iter().rev() {
            if let Err(e) = unsafe { PowerClearRequest(self.request, *request_type) } {
                log::error!("PowerClearRequest({request_type:?}) failed: {e}");
            }
        }
        if let Err(e) = unsafe { CloseHandle(self.request) } {
            log::error!("CloseHandle on the power request failed: {e}");
        }
    }
}
