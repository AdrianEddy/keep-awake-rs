//! Checks the macOS leg against what the OS actually reports.
//!
//! `pmset -g assertions` prints one line per live assertion, carrying the
//! assertion type and the name the assertion was created with, so it can confirm
//! both that the right assertion type was taken and that the attribution reached
//! it. Run with `cargo test --test macos`.
#![cfg(target_os = "macos")]

use std::process::Command;

/// The `pmset -g assertions` lines that mention `name`.
fn assertion_lines(name: &str) -> Vec<String> {
    let output = Command::new("pmset")
        .args(["-g", "assertions"])
        .output()
        .expect("pmset -g assertions");
    assert!(output.status.success(), "pmset failed: {:?}", output.status);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(name))
        .map(str::to_owned)
        .collect()
}

fn assert_assertion(name: &str, expected_type: &str) {
    let lines = assertion_lines(name);
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one assertion named {name:?}, got {lines:#?}"
    );
    assert!(
        lines[0].contains(expected_type),
        "assertion {name:?} is not a {expected_type}: {:?}",
        lines[0]
    );
}

#[test]
fn display_assertion_is_taken_and_released() {
    let name = "keep-awake-test-display";
    let reason = "display leg";
    assert!(assertion_lines(name).is_empty(), "stale assertion {name:?}");

    let holder = keep_awake::inhibit_display(name, reason).expect("inhibit_display");
    assert_assertion(&format!("{name}: {reason}"), "PreventUserIdleDisplaySleep");

    drop(holder);
    assert!(
        assertion_lines(name).is_empty(),
        "assertion {name:?} outlived its holder"
    );
}

#[test]
fn system_assertion_is_taken_and_released() {
    let name = "keep-awake-test-system";
    let reason = "system leg";
    assert!(assertion_lines(name).is_empty(), "stale assertion {name:?}");

    let holder = keep_awake::inhibit_system(name, reason).expect("inhibit_system");
    assert_assertion(&format!("{name}: {reason}"), "PreventUserIdleSystemSleep");

    drop(holder);
    assert!(
        assertion_lines(name).is_empty(),
        "assertion {name:?} outlived its holder"
    );
}

/// The two kinds are independent handles: taking one must not disturb the other,
/// and dropping one must not release the other. (On Windows this is exactly what
/// `SetThreadExecutionState`'s absolute per-thread state could not do.)
#[test]
fn the_two_kinds_do_not_clobber_each_other() {
    let display_name = "keep-awake-test-both-display";
    let system_name = "keep-awake-test-both-system";
    let reason = "coexistence";

    let display = keep_awake::inhibit_display(display_name, reason).expect("inhibit_display");
    let system = keep_awake::inhibit_system(system_name, reason).expect("inhibit_system");
    assert_assertion(
        &format!("{display_name}: {reason}"),
        "PreventUserIdleDisplaySleep",
    );
    assert_assertion(
        &format!("{system_name}: {reason}"),
        "PreventUserIdleSystemSleep",
    );

    drop(display);
    assert!(assertion_lines(display_name).is_empty());
    assert_assertion(
        &format!("{system_name}: {reason}"),
        "PreventUserIdleSystemSleep",
    );

    drop(system);
    assert!(assertion_lines(system_name).is_empty());
}
