//! A trigger's precheck is a repository's command that the server starts:
//! it is not given a control plane's token, under whatever variable of the
//! server holds it. A file of its own: it sets a variable of this test
//! process.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report

use std::time::Duration;

use branchyard_support::child;

const TOKEN: &str = "precheck-guarded-token-0123456789";

#[test]
fn a_precheck_is_started_without_a_guarded_value() {
    std::env::set_var("BY_TEST_PRECHECK_COPY", TOKEN);
    std::env::set_var("BY_TEST_PRECHECK_OTHER", "another-value-0123456789");
    // What a gateway with a stored AIPlex key registers when it reads it.
    child::guard("ap", TOKEN);
    let dir = tempfile::tempdir().unwrap();
    let given = [
        ("PRECHECK_GIVEN".to_owned(), format!("Bearer {TOKEN}")),
        ("PRECHECK_FINE".to_owned(), "fine".to_owned()),
    ];
    // What the command sees of the four variables, and how many of all its
    // variables hold the token: never the environment itself, which is the
    // machine's that runs this test.
    let command = format!(
        "printf '%s|%s|%s|%s|' \"${{BY_TEST_PRECHECK_COPY-unset}}\" \"${{PRECHECK_GIVEN-unset}}\" \
         \"${{PRECHECK_FINE-unset}}\" \"${{BY_TEST_PRECHECK_OTHER-unset}}\"; \
         env | grep -c {TOKEN} || true"
    );
    let result = branchyard_server::triggers::precheck::run(
        &command,
        Duration::from_secs(20),
        dir.path(),
        &given,
    );
    assert_eq!(
        result.exit_code,
        Some(0),
        "it exited {:?}",
        result.exit_code
    );
    // Neither the server's variable that holds it nor one it was given;
    // everything else is the server's environment, as it was.
    assert_eq!(
        result.stdout.trim(),
        "unset|unset|fine|another-value-0123456789|0"
    );
}
