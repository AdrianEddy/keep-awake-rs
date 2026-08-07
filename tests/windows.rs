//! Checks the Windows leg against what the OS actually reports.
//!
//! `powercfg /requests` is the one place Windows shows a power request back to the
//! user, `REASON_CONTEXT` attribution string and all, so it is what these tests read.
//! It needs elevation, so the reading half of each test announces itself as skipped
//! rather than passing quietly when the query is refused; the half that needs no
//! privilege — that acquiring and releasing succeed at all, which is what catches a
//! malformed `REASON_CONTEXT` or a handle leak — always runs.
//!
//! The reading is a whole-output substring search: it proves the request is live and
//! carries its attribution, not which of `powercfg`'s sections it landed in.
#![cfg(target_os = "windows")]

use std::process::Command;

/// `powercfg /requests` output, or `None` when this process may not ask for it.
fn power_requests() -> Option<String> {
    let output = Command::new("powercfg").arg("/requests").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    // The section headings only appear once the query itself has succeeded, which it
    // does not do without elevation.
    (output.status.success() && text.contains("DISPLAY:")).then_some(text)
}

#[track_caller]
fn assert_listed(attribution: &str, expected: bool) {
    let Some(requests) = power_requests() else {
        eprintln!("skipped the `powercfg /requests` check: it needs elevation");
        return;
    };
    assert_eq!(
        requests.contains(attribution),
        expected,
        "{attribution:?} should{} be listed:\n{requests}",
        if expected { "" } else { " not" }
    );
}

fn attribution(name: &str, reason: &str) -> String {
    format!("{name}: {reason}")
}

#[test]
fn a_display_request_is_taken_and_released() {
    let (name, reason) = ("keep-awake-test-display", "display leg");
    let attribution = attribution(name, reason);
    assert_listed(&attribution, false);

    let holder = keep_awake::inhibit_display(name, reason).expect("inhibit_display");
    assert_listed(&attribution, true);

    drop(holder);
    assert_listed(&attribution, false);
}

#[test]
fn a_system_request_is_taken_and_released() {
    let (name, reason) = ("keep-awake-test-system", "system leg");
    let attribution = attribution(name, reason);
    assert_listed(&attribution, false);

    let holder = keep_awake::inhibit_system(name, reason).expect("inhibit_system");
    assert_listed(&attribution, true);

    drop(holder);
    assert_listed(&attribution, false);
}

/// The two kinds are separate request objects: taking one must not disturb the
/// other, and dropping one must not release the other. This is exactly what
/// `SetThreadExecutionState`'s absolute state could not do, and why this leg does
/// not use it.
#[test]
fn the_two_kinds_do_not_clobber_each_other() {
    let reason = "coexistence";
    let display_attribution = attribution("keep-awake-test-both-display", reason);
    let system_attribution = attribution("keep-awake-test-both-system", reason);

    let display =
        keep_awake::inhibit_display("keep-awake-test-both-display", reason).expect("display");
    let system = keep_awake::inhibit_system("keep-awake-test-both-system", reason).expect("system");
    assert_listed(&display_attribution, true);
    assert_listed(&system_attribution, true);

    drop(display);
    assert_listed(&display_attribution, false);
    assert_listed(&system_attribution, true);

    drop(system);
    assert_listed(&system_attribution, false);
}

/// Every power request API is process-scoped and the handle is an ordinary kernel
/// handle, so acquiring and releasing need not share a thread — the other half of
/// what `SetThreadExecutionState`, whose state is per thread, could not do.
#[test]
fn a_holder_may_be_released_on_another_thread() {
    let (name, reason) = ("keep-awake-test-thread", "crossing threads");
    let attribution = attribution(name, reason);

    let holder = keep_awake::inhibit_display(name, reason).expect("inhibit_display");
    assert_listed(&attribution, true);

    std::thread::spawn(move || drop(holder))
        .join()
        .expect("the releasing thread panicked");
    assert_listed(&attribution, false);
}
