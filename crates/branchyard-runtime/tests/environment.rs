//! Environment scrubbing. One test in its own binary, because it sets this
//! process's environment.

#![cfg(unix)]

mod common;

use branchyard_harness::{PermissionDecision, SessionMode};
use branchyard_runtime::Environment;
use common::{start_with, workdir, WAIT};

#[test]
fn the_harness_sees_a_scrubbed_environment_and_a_private_home() {
    std::env::set_var("CLAUDE_BRANCHYARD_TEST_SECRET", "secret");
    std::env::set_var("anthropic_branchyard_test_lower", "secret");
    std::env::set_var("ANTHROPIC_BRANCHYARD_TEST_KEPT", "kept");
    std::env::set_var("BRANCHYARD_TEST_CUSTOM", "secret");
    std::env::set_var("BRANCHYARD_TEST_PLAIN", "plain");

    let dir = workdir("environment");
    let home = dir.join("home");
    let env = Environment::new(&home)
        .keep("ANTHROPIC_BRANCHYARD_TEST_KEPT")
        .strip("branchyard_test_custom")
        .set("BRANCHYARD_TEST_SET", "set");
    let mut session = start_with(&dir, SessionMode::Fresh, &env);
    let report = session
        .run_turn(
            "ENV CLAUDE_BRANCHYARD_TEST_SECRET anthropic_branchyard_test_lower \
             ANTHROPIC_BRANCHYARD_TEST_KEPT BRANCHYARD_TEST_CUSTOM BRANCHYARD_TEST_PLAIN \
             BRANCHYARD_TEST_SET",
            &mut |_| PermissionDecision::Allow,
            WAIT,
        )
        .unwrap();
    let lines: Vec<&str> = report.text.lines().collect();
    assert_eq!(
        lines,
        [
            format!("HOME={}", home.display()).as_str(),
            "CLAUDE_BRANCHYARD_TEST_SECRET unset",
            "anthropic_branchyard_test_lower unset",
            "ANTHROPIC_BRANCHYARD_TEST_KEPT=kept",
            "BRANCHYARD_TEST_CUSTOM unset",
            "BRANCHYARD_TEST_PLAIN=plain",
            "BRANCHYARD_TEST_SET=set",
        ]
    );
    session.close(WAIT).unwrap();
}
