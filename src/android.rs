//! Android inhibition via `PowerManager.WakeLock`.
//!
//! One mechanism, two levels: `PARTIAL_WAKE_LOCK` for a system inhibition (the
//! CPU keeps running with the screen off) and `FULL_WAKE_LOCK` for a display one
//! — see [`inhibit_display`] for why the second is the more fragile of the two
//! and what an application that owns a `Window` should do instead.
//!
//! Both need `<uses-permission android:name="android.permission.WAKE_LOCK" />`
//! in the host's manifest. Without it `WakeLock.acquire()` throws
//! `SecurityException`, which arrives here as an `Err` naming it.
//!
//! # Reaching the JVM
//!
//! `ndk_context::android_context()` hands back the `JavaVM` and the `Context`
//! that whichever glue crate started the process seeded — `android-activity`,
//! `ndk-glue`, or a host's own `JNI_OnLoad`. A library must not require a
//! particular one, and that process-global is the thing they all agree on.
//!
//! It is also a `static mut … .expect(…)`: reading it *panics* when nothing
//! seeded it, and the crate exposes no fallible accessor. So the read is guarded
//! ([`android_context`]) and a missing seed is an `Err`, which is a condition to
//! report rather than one to die of. Every other JNI step — `JavaVM::from_raw`,
//! the attach, each call — is likewise reported and never unwrapped.
//!
//! [`Holder`] carries the `JavaVM` it was taken on, so releasing never has to go
//! back to `ndk_context` at all — which is what takes the panic out of the one
//! place a panic is worst. The unwinding paths that remain under a destructor are
//! caught rather than assumed away: see [`Holder::drop`].
//!
//! # Local references are bounded per acquire
//!
//! jni 0.21's `JObject`/`JString` are transparent `jobject` newtypes with **no**
//! `Drop`, and `JavaVM::attach_current_thread` pushes no local frame of its own —
//! on an already-attached thread it takes the `new_nested` branch, which is a TLS
//! check and nothing more. So without an explicit frame every reference an
//! acquire mints stays live, rooting its Java object, until the *host's* frame is
//! popped, which for a native thread driving an event loop is never.
//!
//! Both halves therefore run their JNI inside [`JNIEnv::with_local_frame`], which
//! covers the success path and every failure path alike — including the likeliest
//! real failure, a missing `WAKE_LOCK` permission, where the throwable and the
//! references it takes to read it are minted too.
//!
//! Whether the leak would ever have *overflowed* depends on a floor a library
//! does not get to pick: ART's local-reference table became resizable in Android
//! 8.0 and today stops at `kMaxTableSizeInBytes = 128 * MB` of four-byte entries,
//! about 33.5 M references; on 7.1 and earlier it is a fixed `kLocalsMax = 512`
//! (`runtime/jni_env_ext.h`) whose overflow is `LOG(FATAL)`, a process abort after
//! roughly 128 acquire/release cycles. A push and a pop per acquire cost nothing
//! against the binder round trip they wrap, so this does not depend on the answer.
//!
//! # Exceptions are described, cleared, and reported
//!
//! Calling any JNI function with an exception pending is undefined behaviour, and
//! `jni::errors::Error::JavaException` says only *that* one is pending. Every
//! failure therefore goes through [`java_error`]: `ExceptionOccurred`,
//! `ExceptionDescribe` (which is what puts the Java stack trace in logcat, the
//! only place a field report can come from), `ExceptionClear`, and then the
//! throwable's own `toString()` — class and message both — as the returned error.

use jni::objects::{GlobalRef, JObject, JString, JThrowable};
use jni::sys::jint;
use jni::{JNIEnv, JavaVM};

/// `PowerManager.PARTIAL_WAKE_LOCK`.
const PARTIAL_WAKE_LOCK: jint = 0x0000_0001;

/// `PowerManager.FULL_WAKE_LOCK`.
const FULL_WAKE_LOCK: jint = 0x0000_001a;

/// How many local references an acquire's frame must guarantee.
///
/// The success path holds at most five at once — the two `String`s, the
/// `PowerManager`, the `WakeLock` local, and the transient class reference
/// `call_method` resolves a method ID against. The failure path adds the
/// throwable plus the four that `toString` and `JNIEnv::get_string` mint between
/// them (`get_string` alone does a `FindClass` and a `GetObjectClass` to check
/// the type, and frees neither). Sixteen is that, rounded up; the push and the
/// pop cost the same whatever the number is.
const LOCAL_FRAME_CAPACITY: i32 = 16;

/// A held wake lock. Dropping it releases.
pub struct Holder {
    /// The `PowerManager$WakeLock`. A *global* reference: it outlives both the
    /// local frame it was made in and the thread that made it.
    ///
    /// `Option` only because [`Holder::release`] takes it: `DeleteGlobalRef` has
    /// to happen while the releasing thread is still attached, and a field
    /// dropped in place would run after the attach guard above it. It is `Some`
    /// for the whole life of a `Holder`.
    wake_lock: Option<GlobalRef>,
    /// The VM the lock was taken on, carried so that the release never has to go
    /// back to `ndk_context` — whose read panics, and a destructor must not.
    vm: JavaVM,
}

/// Keeps the CPU running with the screen and keyboard backlight free to go off:
/// `PARTIAL_WAKE_LOCK`, the only non-deprecated wake-lock level whose job is
/// keeping the device awake.
pub fn inhibit_system(name: &str, reason: &str) -> Result<Holder, Box<dyn std::error::Error>> {
    acquire_wake_lock(name, reason, PARTIAL_WAKE_LOCK)
}

/// Keeps the screen and the keyboard backlight on at full brightness:
/// `FULL_WAKE_LOCK`.
///
/// **Deprecated by the platform since API 17, but not defunct.** It is still in
/// `PowerManager.validateWakeLockParameters`' accepted-level list, and
/// `PowerManagerService.getWakeLockSummaryFlags` still maps it to
/// `WAKE_LOCK_SCREEN_BRIGHT | WAKE_LOCK_BUTTON_BRIGHT` with no `targetSdkVersion`
/// gate anywhere — so this works, and the deprecation is advice rather than a
/// countdown. Two platform behaviours come with it that `PARTIAL_WAKE_LOCK` does
/// not have: the system **implicitly releases** a `FULL_WAKE_LOCK` when the user
/// presses the power button, and a screen wake lock held by a cached or idle app
/// can be disabled underneath the holder.
///
/// The advice itself is worth taking where it can be: the javadoc points at
/// `WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON`, "as it will be correctly
/// managed by the platform as the user moves between applications and doesn't
/// require a special permission". That flag is not offered here because it cannot
/// be — `Window.addFlags` is a UI-thread API, and this crate's surface is a plain
/// function callable from any thread with no main-thread hop of its own to ride.
/// A host that *has* one (`AndroidApp::set_window_flags` under
/// `android-activity`, `runOnUiThread` under a plain `Activity`) should set the
/// flag there and use only [`inhibit_system`] from here.
pub fn inhibit_display(name: &str, reason: &str) -> Result<Holder, Box<dyn std::error::Error>> {
    acquire_wake_lock(name, reason, FULL_WAKE_LOCK)
}

/// The `AndroidContext` the process was seeded with, or an error saying it was
/// not.
///
/// `ndk_context::android_context()` is `ANDROID_CONTEXT.expect(…)` over a
/// `static mut`, and the crate offers nothing fallible to ask instead — so the
/// panic is caught, which is the only way to turn "nothing seeded this process"
/// into the `Err` a library owes its caller.
fn android_context() -> Result<ndk_context::AndroidContext, Box<dyn std::error::Error>> {
    std::panic::catch_unwind(ndk_context::android_context).map_err(|_| {
        "no Android context: nothing in this process seeded ndk_context (android-activity, \
         ndk-glue, or ndk_context::initialize_android_context)"
            .into()
    })
}

fn acquire_wake_lock(
    name: &str,
    reason: &str,
    flags: jint,
) -> Result<Holder, Box<dyn std::error::Error>> {
    let ctx = android_context()?;
    if ctx.context().is_null() {
        return Err("the Android context seeded into ndk_context is null".into());
    }

    // SAFETY: the pointer is the `JavaVM` the glue crate seeded and is valid for
    // the life of the process; `from_raw` only null-checks it.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }?;
    let tag = wake_lock_tag(name, reason);

    // The attach guard is scoped so that `vm` is free to move into the `Holder`.
    // Letting it go re-detaches a thread this call attached, which is right: the
    // global reference below is not thread-scoped, and the release attaches again.
    let wake_lock = {
        let mut env = vm.attach_current_thread()?;
        env.with_local_frame(LOCAL_FRAME_CAPACITY, |env| {
            match new_wake_lock(env, &ctx, &tag, flags) {
                Ok(wake_lock) => Ok(wake_lock),
                // Inside the frame on purpose: reading the throwable mints
                // several more local references, and this is what reclaims them.
                Err(e) => Err(java_error(env, e)),
            }
        })?
    };

    Ok(Holder {
        wake_lock: Some(wake_lock),
        vm,
    })
}

/// `getSystemService("power").newWakeLock(flags, tag).acquire()`.
///
/// The global reference is minted *before* `acquire()` rather than after, and the
/// order matters: `acquire()` is the call that throws (a manifest without
/// `WAKE_LOCK` fails exactly here), and a lock acquired against a handle this
/// function then failed to keep would be held for the life of the process with
/// nothing left able to release it.
fn new_wake_lock(
    env: &mut JNIEnv,
    ctx: &ndk_context::AndroidContext,
    tag: &str,
    flags: jint,
) -> jni::errors::Result<GlobalRef> {
    let service = env.new_string("power")?;
    let tag = env.new_string(tag)?;

    // SAFETY: a valid reference seeded by the glue crate, wrapped as a borrowed
    // (non-owning) `JObject` for the duration of the call — jni 0.21's `JObject`
    // has no `Drop`, so the seeded reference is neither freed nor duplicated.
    let context = unsafe { JObject::from_raw(ctx.context().cast()) };
    let power_manager = env
        .call_method(
            &context,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[(&service).into()],
        )?
        .l()?;

    let wake_lock = env
        .call_method(
            &power_manager,
            "newWakeLock",
            "(ILjava/lang/String;)Landroid/os/PowerManager$WakeLock;",
            &[flags.into(), (&tag).into()],
        )?
        .l()?;
    let wake_lock = env.new_global_ref(wake_lock)?;

    env.call_method(&wake_lock, "acquire", "()V", &[])?;

    Ok(wake_lock)
}

/// The tag the lock is attributed to in `dumpsys power`.
///
/// `newWakeLock`'s javadoc asks for "a unique prefix delimited by a colon for
/// your app/library (e.g. `gmail:mytag`)", so the two halves are joined with one.
/// Nothing in the platform *parses* that colon — `PowerManagerService` keeps the
/// tag as an opaque string and only ever hashes it, for the systrace track name —
/// and the only tag `validateWakeLockParameters` rejects is a null one. The
/// convention is for the human and the aggregating tool reading the dump.
///
/// Which is why an empty `reason` is joined to nothing rather than to a bare
/// colon: `"com.example.app"` is a tag whose prefix has no second half, and a
/// trailing `:` would only claim one is there and read as a bug in the caller.
fn wake_lock_tag(name: &str, reason: &str) -> String {
    if reason.is_empty() {
        name.to_owned()
    } else {
        format!("{name}:{reason}")
    }
}

/// Turns a JNI error into one worth reporting, draining any pending Java
/// exception on the way.
///
/// Leaving a throwable pending makes the next JNI call on this thread undefined
/// behaviour, and `Error::JavaException` names nothing by itself, so the
/// throwable is taken, described into logcat, cleared, and its `toString()` —
/// class *and* message, which is how `java.lang.SecurityException: … requires
/// android.permission.WAKE_LOCK` gets named in full — returned in its place.
///
/// Mints local references, so it is only ever called from inside a local frame.
fn java_error(env: &mut JNIEnv, error: jni::errors::Error) -> Box<dyn std::error::Error> {
    if !matches!(error, jni::errors::Error::JavaException) {
        return Box::new(error);
    }
    let Ok(throwable) = env.exception_occurred() else {
        return Box::new(error);
    };
    // `ExceptionDescribe` is the only thing that ever prints the Java stack
    // trace; `ExceptionClear` is what makes the calls below legal.
    let _ = env.exception_describe();
    let _ = env.exception_clear();
    if throwable.is_null() {
        return Box::new(error);
    }
    match throwable_text(env, &throwable) {
        Ok(text) => text.into(),
        // A real throwable that would not describe itself. It is already in the
        // log, so say what is known rather than inventing a cause.
        Err(e) => format!("a Java exception was thrown, and reading it also failed: {e}").into(),
    }
}

fn throwable_text(env: &mut JNIEnv, throwable: &JThrowable) -> jni::errors::Result<String> {
    let text = env
        .call_method(throwable, "toString", "()Ljava/lang/String;", &[])?
        .l()?;
    let text = JString::from(text);
    Ok(env.get_string(&text)?.into())
}

impl Holder {
    /// `WakeLock.release()`, then `DeleteGlobalRef`, reporting every failure to
    /// the log — the only channel a destructor has.
    fn release(&mut self) {
        let Some(wake_lock) = self.wake_lock.take() else {
            return;
        };
        let mut env = match self.vm.attach_current_thread() {
            Ok(env) => env,
            Err(e) => {
                log::error!("could not attach this thread to release the wake lock: {e}");
                return;
            }
        };
        // A frame here for the same reason as on the acquire side: `release()`
        // itself mints nothing that survives `call_method`, but a throwable —
        // `release()` raises `RuntimeException` on an under-locked lock — and the
        // references reading it costs would otherwise escape into the host's frame.
        let released = env.with_local_frame(LOCAL_FRAME_CAPACITY, |env| {
            match env.call_method(&wake_lock, "release", "()V", &[]) {
                Ok(_) => Ok(()),
                Err(e) => Err(java_error(env, e)),
            }
        });
        if let Err(e) = released {
            log::error!("WakeLock.release() failed: {e}");
        }
        // Explicitly, and before `env`: dropping a `GlobalRef` on a *detached*
        // thread makes jni attach and detach a second time to run
        // `DeleteGlobalRef`, warning as it goes. Scope order alone would do the
        // opposite of what is wanted here — `env` was bound second, so it would
        // be dropped first.
        drop(wake_lock);
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        // A destructor can run while a panic is already unwinding, where a second
        // panic aborts the process — and under `panic = "abort"`, which is what a
        // shipped Android build uses, any panic at all does. So nothing here is
        // allowed to unwind.
        //
        // [`Holder::release`] reports rather than unwraps, which covers the JNI
        // itself. The one unwind it cannot turn into an `Err` is `thread_local!`'s
        // `with`, which `JavaVM::attach_current_thread` goes through when the
        // calling thread is not already attached, and which panics if that
        // thread's TLS is already being destroyed. That is what this catches —
        // and because `release` also consumes the `GlobalRef`, whose own drop
        // attaches by the same route, the whole destructor is inside it.
        let released = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.release()));
        if released.is_err() {
            log::error!("releasing the wake lock panicked; the lock is still held");
        }
    }
}
