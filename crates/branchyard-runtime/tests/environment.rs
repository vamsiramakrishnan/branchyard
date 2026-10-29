//! Environment scrubbing. One test in its own binary, because it sets this
//! process's environment.

#![cfg(unix)]

mod common;

use branchyard_harness::{PermissionDecision, SessionMode};
use branchyard_runtime::Environment;
use common::{start_with, workdir, WAIT};

#[test]
fn the_harness_sees_a_scrubbed_environment_and_a_private_home_unless_inheriting() {
    std::env::set_var("CLAUDE_BRANCHYARD_TEST_SECRET", "secret");
    std::env::set_var("anthropic_branchyard_test_lower", "secret");
    std::env::set_var("ANTHROPIC_BRANCHYARD_TEST_KEPT", "kept");
    std::env::set_var("BRANCHYARD_TEST_CUSTOM", "secret");
    std::env::set_var("BRANCHYARD_TEST_PLAIN", "plain");
    std::env::set_var(
        branchyard_support::MANAGER_TOKEN_ENV,
        "fixture-operator-secret",
    );

    let dir = workdir("environment");
    let home = dir.join("home");
    let env = Environment::new(&home)
        .keep("ANTHROPIC_BRANCHYARD_TEST_KEPT")
        .keep(branchyard_support::MANAGER_TOKEN_ENV)
        .strip("branchyard_test_custom")
        .set("BRANCHYARD_TEST_SET", "set")
        .set(
            branchyard_support::MANAGER_TOKEN_ENV,
            "fixture-explicit-secret",
        );
    // `admits` answers as the scrub does, for a variable from elsewhere.
    assert!(!env.admits("ANTHROPIC_API_KEY"));
    assert!(!env.admits("openai_api_key"));
    assert!(env.admits("ANTHROPIC_BRANCHYARD_TEST_KEPT"));
    assert!(!env.admits("BRANCHYARD_TEST_CUSTOM"));
    assert!(env.admits("NODE_VERSION"));
    assert!(!Environment::inherit().remove("X").admits("X"));
    assert!(Environment::inherit().admits("ANTHROPIC_API_KEY"));
    assert!(!env.admits(branchyard_support::MANAGER_TOKEN_ENV));
    assert!(!format!("{env:?}").contains("fixture-explicit-secret"));
    assert!(!Environment::inherit().admits("branchyard_manager_token"));
    assert!(!Environment::inherit()
        .set("branchyard_manager_token", "fixture")
        .resolve()
        .contains_key(std::ffi::OsStr::new("branchyard_manager_token")));
    let mut session = start_with(&dir, SessionMode::Fresh, &env);
    let report = session
        .run_turn(
            "ENV CLAUDE_BRANCHYARD_TEST_SECRET anthropic_branchyard_test_lower \
             ANTHROPIC_BRANCHYARD_TEST_KEPT BRANCHYARD_TEST_CUSTOM BRANCHYARD_TEST_PLAIN \
             BRANCHYARD_TEST_SET BRANCHYARD_MANAGER_TOKEN",
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
            "BRANCHYARD_MANAGER_TOKEN unset",
        ]
    );
    session.close(WAIT).unwrap();

    // Inheriting keeps normal credentials but never the operator manager token.
    let inherited = Environment::inherit()
        .remove("BRANCHYARD_TEST_PLAIN")
        .set("BRANCHYARD_TEST_SET", "set")
        .keep(branchyard_support::MANAGER_TOKEN_ENV)
        .set(
            branchyard_support::MANAGER_TOKEN_ENV,
            "fixture-explicit-secret",
        );
    let real_home = std::env::var("HOME").unwrap();
    assert_eq!(inherited.home(), std::path::Path::new(&real_home));
    let mut session = start_with(&dir, SessionMode::Fresh, &inherited);
    let report = session
        .run_turn(
            "ENV CLAUDE_BRANCHYARD_TEST_SECRET BRANCHYARD_TEST_PLAIN BRANCHYARD_TEST_SET BRANCHYARD_MANAGER_TOKEN",
            &mut |_| PermissionDecision::Allow,
            WAIT,
        )
        .unwrap();
    let lines: Vec<&str> = report.text.lines().collect();
    assert_eq!(
        lines,
        [
            format!("HOME={real_home}").as_str(),
            "CLAUDE_BRANCHYARD_TEST_SECRET=secret",
            "BRANCHYARD_TEST_PLAIN unset",
            "BRANCHYARD_TEST_SET=set",
            "BRANCHYARD_MANAGER_TOKEN unset",
        ]
    );
    session.close(WAIT).unwrap();
}
