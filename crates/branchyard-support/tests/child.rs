//! A hidden variable reaches no child made by `child::command` or passed
//! through `child::scrub`, and stays in this process. A file of its own: it
//! sets a variable of this test process, once.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report

use std::process::Command;
use std::sync::Once;

use branchyard_support::child;

static VARIABLE: Once = Once::new();

fn said(command: &mut Command) -> String {
    let out = command
        .args(["-c", "printf %s \"${BY_TEST_CHILD_SECRET-unset}\""])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn a_hidden_variable_reaches_no_child_and_stays_here() {
    VARIABLE.call_once(|| std::env::set_var("BY_TEST_CHILD_SECRET", "s3cret"));
    // Before it is hidden, a child inherits it, as any variable.
    assert_eq!(said(&mut child::command("sh")), "s3cret");
    child::hide("BY_TEST_CHILD_SECRET");
    child::hide("BY_TEST_CHILD_SECRET");
    assert_eq!(child::hidden(), ["BY_TEST_CHILD_SECRET"]);
    assert_eq!(said(&mut child::command("sh")), "unset");
    let mut made_elsewhere = Command::new("sh");
    assert_eq!(said(child::scrub(&mut made_elsewhere)), "unset");
    // Set for the child on purpose, it is set.
    let mut explicit = child::command("sh");
    explicit.env("BY_TEST_CHILD_SECRET", "given");
    assert_eq!(said(&mut explicit), "given");
    // This process still reads it.
    assert_eq!(std::env::var("BY_TEST_CHILD_SECRET").unwrap(), "s3cret");
}
