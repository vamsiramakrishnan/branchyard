//! The local provider's write guard (`ExecSpec::writable`, Landlock):
//! `/bin/sh` and the fake agent writing inside their scope and refused
//! outside it, descendants held too, reads untouched, the network
//! confinement composing with it, and a spec the host cannot hold refused.
//!
//! A host without Landlock skips the tests that need it, printing why
//! (`LocalProvider::write_guard`); the off switch has its own binary,
//! `tests/guard_off.rs`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use branchyard_harness::SessionMode;
use branchyard_runtime::{Confinement, Environment, LocalProvider, Session};
use branchyard_sandbox::{ExecSpec, Process};
use branchyard_testkit::Scratch;

/// Whether this host can hold a write scope; says why not when it cannot.
fn guarded(test: &str) -> bool {
    match LocalProvider::write_guard() {
        Ok(_) => true,
        Err(why) => {
            eprintln!("skipping {test}: no file guard on this host: {why}");
            false
        }
    }
}

fn sh(cwd: &Path, script: &str, writable: Option<Vec<PathBuf>>) -> ExecSpec {
    ExecSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        cwd: cwd.to_path_buf(),
        env: BTreeMap::from([("PATH".into(), std::env::var_os("PATH").unwrap_or_default())]),
        writable,
    }
}

/// Run a started process to its end: its exit code, stdout and stderr.
fn finish(mut process: impl Process) -> (Option<i32>, String, String) {
    drop(process.take_stdin());
    let mut out = String::new();
    process
        .take_stdout()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let mut err = String::new();
    process
        .take_stderr()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    (process.wait().unwrap().code, out, err)
}

#[test]
fn a_guarded_shell_writes_inside_its_scope_and_is_refused_outside() {
    if !guarded("a_guarded_shell_writes_inside_its_scope_and_is_refused_outside") {
        return;
    }
    let scratch = Scratch::new("guard-shell");
    let inside = scratch.join("inside");
    let outside = scratch.join("outside");
    std::fs::create_dir_all(inside.join("deep")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("readable.txt"), "read me\n").unwrap();
    std::fs::write(outside.join("kept.txt"), "kept\n").unwrap();
    let script = "printf in > deep/new.txt && echo inside-write-ok; \
                  mkdir made && mv deep/new.txt made/moved.txt && echo inside-rename-ok; \
                  cat ../outside/readable.txt; \
                  printf out > ../outside/new.txt || echo outside-write-refused; \
                  printf out >> ../outside/kept.txt || echo outside-append-refused; \
                  rm ../outside/kept.txt || echo outside-remove-refused; \
                  mkdir ../outside/dir || echo outside-mkdir-refused; \
                  sh -c 'printf child > ../outside/child.txt' || echo descendant-refused";
    let process = LocalProvider::spawn(&sh(&inside, script, Some(vec![inside.clone()]))).unwrap();
    let (code, out, err) = finish(process);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        [
            "inside-write-ok",
            "inside-rename-ok",
            "read me",
            "outside-write-refused",
            "outside-append-refused",
            "outside-remove-refused",
            "outside-mkdir-refused",
            "descendant-refused",
        ],
        "stderr: {err}"
    );
    // The kernel's answer: EACCES, as the shell reports it.
    assert!(err.contains("Permission denied"), "{err}");
    assert_eq!(
        std::fs::read_to_string(inside.join("made/moved.txt")).unwrap(),
        "in"
    );
    assert!(!outside.join("new.txt").exists());
    assert!(!outside.join("child.txt").exists());
    assert!(!outside.join("dir").exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("kept.txt")).unwrap(),
        "kept\n"
    );
}

#[test]
fn an_unguarded_spec_writes_where_its_user_may() {
    let scratch = Scratch::new("guard-none");
    let outside = scratch.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let process = LocalProvider::spawn(&sh(
        scratch.path(),
        "printf out > outside/new.txt && echo wrote",
        None,
    ))
    .unwrap();
    let (code, out, err) = finish(process);
    assert_eq!((code, out.trim()), (Some(0), "wrote"), "{err}");
    assert_eq!(
        std::fs::read_to_string(outside.join("new.txt")).unwrap(),
        "out"
    );
}

#[test]
fn several_paths_a_file_and_a_missing_path_make_one_scope() {
    if !guarded("several_paths_a_file_and_a_missing_path_make_one_scope") {
        return;
    }
    let scratch = Scratch::new("guard-paths");
    let (a, b) = (scratch.join("a"), scratch.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let file = scratch.join("one-file.txt");
    std::fs::write(&file, "before\n").unwrap();
    let scope = vec![
        a.clone(),
        b.clone(),
        file.clone(),
        scratch.join("not-there-yet"),
        PathBuf::from("/dev"),
    ];
    let script = "printf a > a/x && printf b > b/y && echo both; \
                  printf after > one-file.txt && echo file-ok; \
                  printf x > one-file.txt.new || echo sibling-refused; \
                  mkdir not-there-yet || echo missing-refused; \
                  printf null > /dev/null && echo dev-null-ok";
    let process = LocalProvider::spawn(&sh(scratch.path(), script, Some(scope))).unwrap();
    let (code, out, err) = finish(process);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        [
            "both",
            "file-ok",
            "sibling-refused",
            "missing-refused",
            "dev-null-ok"
        ],
        "stderr: {err}"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after");
}

#[test]
fn a_relative_path_in_a_scope_is_refused_before_anything_runs() {
    if !guarded("a_relative_path_in_a_scope_is_refused_before_anything_runs") {
        return;
    }
    let scratch = Scratch::new("guard-relative");
    let spec = sh(
        scratch.path(),
        "printf x > ran.txt",
        Some(vec![PathBuf::from("relative/dir")]),
    );
    let error = LocalProvider::spawn(&spec).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
    assert!(error.to_string().contains("relative/dir"), "{error}");
    assert!(!scratch.join("ran.txt").exists());
}

#[test]
fn the_guard_composes_with_network_confinement() {
    if !guarded("the_guard_composes_with_network_confinement") {
        return;
    }
    if let Err(why) = LocalProvider::confinement() {
        eprintln!("skipping the_guard_composes_with_network_confinement: {why}");
        return;
    }
    let scratch = Scratch::new("guard-netns");
    let inside = scratch.join("inside");
    std::fs::create_dir_all(&inside).unwrap();
    // The namespace's /proc/self maps are written before Landlock applies.
    let script = "printf in > in.txt && echo inside-write-ok; \
                  printf out > ../out.txt || echo outside-write-refused";
    let (process, listener) =
        LocalProvider::spawn_confined(&sh(&inside, script, Some(vec![inside.clone()])), 0).unwrap();
    let (code, out, err) = finish(process);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        ["inside-write-ok", "outside-write-refused"],
        "{err}"
    );
    assert!(listener.local_addr().is_ok());
    assert!(!scratch.join("out.txt").exists());
}

#[test]
fn a_guarded_harness_writes_its_worktree_and_not_beside_it() {
    if !guarded("a_guarded_harness_writes_its_worktree_and_not_beside_it") {
        return;
    }
    let scratch = Scratch::new("guard-session");
    let worktree = scratch.join("worktree");
    let home = scratch.join("home");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let confinement = Confinement {
        egress_port: None,
        writable: Some(vec![worktree.clone(), home.clone()]),
    };
    let (mut session, listener) = Session::start_local(
        common::agent(),
        common::open(&worktree, SessionMode::Fresh),
        &Environment::new(&home),
        None,
        &confinement,
    )
    .unwrap();
    assert!(listener.is_none());
    session.wait_ready(common::WAIT).unwrap();
    let mut allow =
        |_: &branchyard_harness::PermissionRequest| branchyard_harness::PermissionDecision::Allow;
    let report = session
        .run_turn(
            "WRITE inside.txt=in WRITE ../outside.txt=out",
            &mut allow,
            common::WAIT,
        )
        .unwrap();
    assert!(report.text.contains("wrote inside.txt"), "{}", report.text);
    assert!(
        report
            .text
            .contains("write ../outside.txt failed: Permission denied"),
        "{}",
        report.text
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("inside.txt")).unwrap(),
        "in\n"
    );
    assert!(!scratch.join("outside.txt").exists());
    session.close(common::WAIT).unwrap();
}
