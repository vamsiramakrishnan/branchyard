//! A guarded value reaches no child made by `child::command` or passed
//! through `child::scrub`, under whatever variable holds it, and stays in
//! this process. A file of its own: it sets variables of this test process,
//! once.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report

use std::process::Command;

use branchyard_support::child;

const VALUE: &str = "guarded-value-0123456789";

/// What a child sees of each variable, `unset` for one it does not have.
fn said(command: &mut Command) -> String {
    let out = command
        .args([
            "-c",
            "printf '%s|%s|%s|%s' \"${BY_TEST_GUARD_COPY-unset}\" \"${BY_TEST_GUARD_INSIDE-unset}\" \
             \"${BY_TEST_GUARD_OTHER-unset}\" \"${BY_TEST_GUARD_GIVEN-unset}\"",
        ])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn a_guarded_value_reaches_no_child_under_any_name_and_stays_here() {
    std::env::set_var("BY_TEST_GUARD_COPY", VALUE);
    std::env::set_var("BY_TEST_GUARD_INSIDE", format!("Bearer {VALUE}"));
    std::env::set_var("BY_TEST_GUARD_OTHER", "another-value-0123456789");
    // Before it is guarded, a child inherits every one of them.
    assert_eq!(
        said(&mut child::command("sh")),
        format!("{VALUE}|Bearer {VALUE}|another-value-0123456789|unset")
    );
    assert_eq!(child::guarded(VALUE), None);
    child::guard("ap", VALUE);
    child::guard("again", VALUE);
    child::guard("empty", "");
    // The value, and a value with it inside, are held; another is not.
    assert_eq!(child::guarded(VALUE).as_deref(), Some("ap"));
    assert_eq!(
        child::guarded(&format!("Bearer {VALUE}")).as_deref(),
        Some("ap")
    );
    assert_eq!(child::guarded("another-value-0123456789"), None);
    assert_eq!(child::guarded(""), None);
    // No child inherits a variable that holds it, whatever its name, nor
    // gets one a caller set on the command before it was scrubbed.
    assert_eq!(
        said(&mut child::command("sh")),
        "unset|unset|another-value-0123456789|unset"
    );
    let mut given = Command::new("sh");
    given.env("BY_TEST_GUARD_GIVEN", format!("x{VALUE}y"));
    assert_eq!(
        said(child::scrub(&mut given)),
        "unset|unset|another-value-0123456789|unset"
    );
    let mut fine = child::command("sh");
    fine.env("BY_TEST_GUARD_GIVEN", "fine");
    assert_eq!(
        said(child::scrub(&mut fine)),
        "unset|unset|another-value-0123456789|fine"
    );
    // This process still reads them.
    assert_eq!(std::env::var("BY_TEST_GUARD_COPY").unwrap(), VALUE);
}
