//! Capacity and placement through the SDK against the fake ACP agent: a
//! child past its harness's limit waits for a free slot and starts when one
//! frees, a parent never holds the slot its child needs, and `auto` places
//! a child by the router and says why.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use branchyard::{recorded_choice, BranchStatus, Envelope, Policy, Spawn, TaskOptions};
use branchyard_testkit::wait;
use common::{fake_agent, Fixture};

fn delegating(f: &Fixture) -> TaskOptions {
    TaskOptions {
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    }
}

/// One branch at a time on the fixture's harness.
fn one_slot(f: &Fixture) {
    f.yard
        .use_max_running(BTreeMap::from([("gemini-cli".to_owned(), 1)]));
}

/// A prompt that waits for `go` to exist, then writes `file`.
fn held(go: &Path, file: &str) -> String {
    format!(
        "SH until [ -f {} ]; do sleep 0.05; done; echo x > {file}",
        go.display()
    )
}

fn child(prompt: String, name: &str) -> Spawn {
    Spawn {
        prompt,
        name: Some(name.into()),
        ..Spawn::default()
    }
}

fn status(f: &Fixture, name: &str) -> BranchStatus {
    f.yard.branch(name).unwrap().info().status.clone()
}

#[test]
fn a_child_past_its_harness_limit_waits_and_starts_when_a_slot_frees() {
    let f = Fixture::new();
    one_slot(&f);
    let options = delegating(&f);
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options).unwrap();
    let go = f.dir.join("go");
    let a = delegate.spawn(child(held(&go, "a.txt"), "a")).unwrap();
    assert_eq!(a.status, BranchStatus::Running);
    assert!(a.waiting_for.is_none());
    let b = delegate.spawn(child("WRITE b.txt=b".into(), "b")).unwrap();
    assert_eq!(b.status, BranchStatus::Waiting);
    assert!(b.depends_on.is_empty());
    let why = b.waiting_for.unwrap();
    assert!(
        why.starts_with("a free slot on gemini-cli, which runs 1 of its 1 at once (a;"),
        "{why}"
    );
    // It waits while a holds the slot, and starts once a settles.
    assert_eq!(status(&f, "b"), BranchStatus::Waiting);
    fs::write(&go, "").unwrap();
    root.wait_subtree().unwrap();
    for name in ["a", "b"] {
        assert_eq!(status(&f, name), BranchStatus::Ready, "{name}");
    }
    // The parent's log says why b waited.
    let log = format!("{:?}", f.yard.branch("root").unwrap().events().unwrap());
    assert!(
        log.contains("waiting for a free slot on gemini-cli"),
        "{log}"
    );
}

#[test]
fn a_parent_does_not_hold_the_slot_its_child_needs() {
    let f = Fixture::new();
    one_slot(&f);
    let options = delegating(&f);
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    let gate = f.dir.join("gate");
    let turn = held(&gate, "root.txt");
    std::thread::scope(|s| {
        let running = s.spawn(|| root.send(&turn, options.clone()).unwrap());
        wait::until("root's turn to run", || {
            status(&f, "root") == BranchStatus::Running
        });
        // Its parent's running turn does not count against it.
        let kid = delegate
            .spawn(child("WRITE kid.txt=k".into(), "kid"))
            .unwrap();
        assert_ne!(kid.status, BranchStatus::Waiting, "{:?}", kid.waiting_for);
        wait::until("kid to finish", || status(&f, "kid") == BranchStatus::Ready);
        fs::write(&gate, "").unwrap();
        running.join().unwrap();
    });
    root.wait_subtree().unwrap();
}

#[test]
fn auto_places_a_child_by_the_router_and_says_why() {
    let f = Fixture::new();
    let options = delegating(&f);
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options).unwrap();
    let placed = delegate
        .spawn(Spawn {
            harness: Some(branchyard::AUTO_HARNESS.into()),
            ..child("WRITE kid.txt=k".into(), "kid")
        })
        .unwrap();
    // The only harness it may run here is its parent's own.
    assert_eq!(placed.harness, "gemini-cli");
    root.wait_subtree().unwrap();
    let events = f.yard.branch("kid").unwrap().events().unwrap();
    let why = recorded_choice(&events).and_then(|c| c.why).unwrap();
    assert!(why.starts_with("placed by the router: "), "{why}");

    // With its harness full, the pick waits for a free slot, and says so.
    one_slot(&f);
    let go = f.dir.join("go");
    delegate.spawn(child(held(&go, "a.txt"), "a")).unwrap();
    let waits = delegate
        .spawn(Spawn {
            harness: Some(branchyard::AUTO_HARNESS.into()),
            ..child("WRITE b.txt=b".into(), "b")
        })
        .unwrap();
    assert_eq!(waits.status, BranchStatus::Waiting);
    fs::write(&go, "").unwrap();
    root.wait_subtree().unwrap();
    let events = f.yard.branch("b").unwrap().events().unwrap();
    let why = recorded_choice(&events).and_then(|c| c.why).unwrap();
    assert!(
        why.contains("it waits for a free slot on gemini-cli"),
        "{why}"
    );
    assert_eq!(status(&f, "b"), BranchStatus::Ready);

    // A seat sets the harness: auto with one is refused.
    let seated = delegate.spawn(Spawn {
        harness: Some(branchyard::AUTO_HARNESS.into()),
        seat: Some("worker".into()),
        ..child("say hi".into(), "c")
    });
    assert!(
        matches!(&seated, Err(branchyard::Error::Denied(why)) if why.contains("a seat sets")),
        "{seated:?}"
    );
}

/// `[usage] max_running` reaches the yard it configures.
#[test]
fn the_configuration_sets_the_limits() {
    let f = Fixture::new();
    let config = branchyard::config::ProjectConfig {
        usage: branchyard::config::UsageConfig {
            max_running: BTreeMap::from([("codex".to_owned(), 4)]),
            ..Default::default()
        },
        ..Default::default()
    };
    f.yard.configure(&config).unwrap();
    assert_eq!(
        f.yard.max_running(),
        BTreeMap::from([("codex".to_owned(), 4)])
    );
}
