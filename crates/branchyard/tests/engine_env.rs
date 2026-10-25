//! The environment a harness inherits, in a test binary of its own: the
//! engine drops the nested-session markers (`CLAUDECODE`) and a Branchyard
//! remote from what it starts a harness with, and keeps the rest. The test
//! gives its own process a `BRANCHYARD_REMOTE` for the engine to drop; set
//! beside other tests it would point everything they start at that remote.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use common::{fake_agent, text, Fixture};

#[test]
fn harnesses_inherit_the_environment_without_nested_session_markers() {
    // The harness must not inherit this remote from the engine's process.
    // The variable is process-wide, so this test is alone in its binary.
    std::env::set_var("BRANCHYARD_REMOTE", "http://127.0.0.1:9");
    let f = Fixture::new();
    let prompt = "ENV CLAUDECODE BRANCHYARD_REMOTE BY_TEST_VISIBLE";
    let shared = f.task(prompt).name("shared").run().unwrap();
    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        text(&shared.events().unwrap()),
        format!("HOME={home}\nCLAUDECODE unset\nBRANCHYARD_REMOTE unset\nBY_TEST_VISIBLE=yes\n")
    );

    let isolated = f
        .task(prompt)
        .name("isolated")
        .isolated(true)
        .run()
        .unwrap();
    let private = f.root.join(".branchyard/homes/isolated");
    assert!(private.is_dir());
    assert_eq!(
        text(&isolated.events().unwrap()),
        format!(
            "HOME={}\nCLAUDECODE unset\nBRANCHYARD_REMOTE unset\nBY_TEST_VISIBLE=yes\n",
            private.display()
        )
    );
    f.yard.remove("isolated").unwrap();
    assert!(!private.exists());
    assert!(fake_agent().is_file());
}
