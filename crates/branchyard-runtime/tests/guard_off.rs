//! `BRANCHYARD_GUARD=off`: the write guard reported unavailable without
//! trying, a spec that asks for a write scope refused (never run
//! unguarded), and one that does not run as before. Its own binary, so
//! the variable is this process's from its one test's start.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::collections::BTreeMap;

use branchyard_runtime::{LocalProvider, ENV_GUARD};
use branchyard_sandbox::{ExecSpec, Operation, ProviderError, SandboxProvider, SandboxSpec};
use branchyard_testkit::Scratch;

#[test]
fn turned_off_the_guard_refuses_a_write_scope_and_runs_the_rest() {
    std::env::set_var(ENV_GUARD, "off");
    let why = LocalProvider::write_guard().unwrap_err();
    assert_eq!(why, "BRANCHYARD_GUARD=off turns it off");

    let scratch = Scratch::new("guard-off");
    let spec = |writable| ExecSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), "printf x > ran.txt".into()],
        cwd: scratch.path().to_path_buf(),
        env: BTreeMap::new(),
        writable,
    };
    let error = LocalProvider::spawn(&spec(Some(vec![scratch.path().to_path_buf()])))
        .map(|_| ())
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported, "{error}");
    assert!(
        error.to_string().contains("BRANCHYARD_GUARD=off"),
        "{error}"
    );
    assert!(!scratch.join("ran.txt").exists(), "nothing ran unguarded");

    let provider = LocalProvider::new();
    provider.ensure(&SandboxSpec::new("off")).unwrap();
    match provider.exec("off", &spec(Some(Vec::new()))) {
        Err(ProviderError::Unsupported(unsupported)) => {
            assert_eq!(unsupported.operation, Operation::WriteScope);
        }
        Err(other) => panic!("expected unsupported, got {other}"),
        Ok(_) => panic!("a write scope this host cannot hold was run"),
    }
    let mut process = provider.exec("off", &spec(None)).unwrap();
    drop(process.take_stdin());
    assert!(process.wait().unwrap().success());
    assert!(scratch.join("ran.txt").exists());
    provider.destroy("off").unwrap();
}
