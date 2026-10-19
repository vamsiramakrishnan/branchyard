//! Ownership scopes through the SDK against the fake ACP agent: a child is
//! given the paths it owns, told them, and refused at integration for
//! changes anywhere else, with its scope bounded by every ancestor's.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use branchyard::{BranchStatus, Envelope, Error, Policy, Spawn, TaskOptions};
use common::{fake_agent, Fixture};

fn delegating(f: &Fixture, envelope: Envelope) -> TaskOptions {
    TaskOptions {
        delegation: Some(envelope),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    }
}

fn owning(prompt: &str, name: &str, owns: &[&str]) -> Spawn {
    Spawn {
        prompt: prompt.into(),
        name: Some(name.into()),
        owns: owns.iter().map(|o| (*o).to_owned()).collect(),
        ..Spawn::default()
    }
}

fn denied<T: std::fmt::Debug>(result: Result<T, Error>, needles: &[&str]) {
    match result {
        Err(Error::Denied(why)) if needles.iter().all(|n| why.contains(n)) => {}
        other => panic!("expected a denial mentioning {needles:?}, got {other:?}"),
    }
}

#[test]
fn a_child_in_its_scope_lands_and_one_that_strays_is_refused_with_its_paths() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options).unwrap();
    let a = delegate
        .spawn(owning("WRITE src/a/x.txt=1", "a", &["./src/a/"]))
        .unwrap();
    assert_eq!(a.owns, ["src/a"], "stored normalized");
    delegate
        .spawn(owning(
            "WRITE src/b/y.txt=2\nWRITE src/a/z.txt=3",
            "b",
            &["src/b"],
        ))
        .unwrap();
    root.wait_subtree().unwrap();

    // Its check says so before anything is integrated.
    let report = delegate.check("b").unwrap();
    assert_eq!(report.outside_scope, ["src/a/z.txt"], "{report:?}");
    assert!(!report.passed());
    assert!(delegate.check("a").unwrap().passed());

    delegate.integrate("a").unwrap();
    let head = f.git(&["rev-parse", "by/root"]);
    denied(
        delegate.integrate("b"),
        &[
            "outside the paths it owns (src/b)",
            "src/a/z.txt",
            "nothing was integrated",
        ],
    );
    // All or none: a batch that names it moves nothing either.
    denied(delegate.integrate_all(&["b"]), &["src/a/z.txt"]);
    assert_eq!(
        f.git(&["rev-parse", "by/root"]),
        head,
        "the target did not move"
    );
    assert_eq!(
        f.yard.branch("b").unwrap().info().status,
        BranchStatus::Ready,
        "still a candidate, to be fixed"
    );

    // Once it undoes the stray change, it lands.
    delegate.send("b", "SH git rm -q src/a/z.txt").unwrap();
    root.wait_subtree().unwrap();
    assert!(delegate.check("b").unwrap().outside_scope.is_empty());
    delegate.integrate("b").unwrap();
    let inspected = delegate.inspect("b").unwrap();
    assert_eq!(inspected.owns, ["src/b"]);
    assert!(inspected.owns_within.is_empty());
}

#[test]
fn a_siblings_work_already_in_the_target_is_not_the_childs_change() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options).unwrap();
    delegate
        .spawn(owning("WRITE src/a/x.txt=1", "a", &["src/a"]))
        .unwrap();
    delegate
        .spawn(owning("WRITE src/b/y.txt=2", "b", &["src/b"]))
        .unwrap();
    root.wait_subtree().unwrap();

    // b reconciles with a before either lands: a batch that merges a first
    // explains a's files in b; the other order does not.
    delegate
        .send("b", "SH git merge -q --no-edit by/a")
        .unwrap();
    root.wait_subtree().unwrap();
    denied(
        delegate.integrate_all(&["b", "a"]),
        &["b changed", "src/a/x.txt"],
    );
    let merged = delegate.integrate_all(&["a", "b"]).unwrap();
    assert_eq!(merged.branches.len(), 2);
    assert_eq!(
        common::git(&root.info().worktree, &["ls-files", "src"]),
        "src/a/x.txt\nsrc/b/y.txt\n"
    );
}

#[test]
fn a_grandchilds_scope_is_bounded_by_its_parents() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    delegate.spawn(owning("say hi", "lead", &["src"])).unwrap();
    root.wait_subtree().unwrap();
    let lead = f
        .yard
        .branch("lead")
        .unwrap()
        .delegate(options.clone())
        .unwrap();

    // Disjoint from its parent's scope: nothing it changed could land.
    denied(
        lead.spawn(owning("say hi", "docs", &["docs"])),
        &["may not own docs", "outside lead's scope (src)"],
    );
    // Malformed entries are refused before anything is created.
    denied(
        lead.spawn(owning("say hi", "up", &["../x"])),
        &["inside the repository"],
    );

    // A child given nothing of its own is still bounded by its parent's.
    lead.spawn(owning(
        "WRITE src/ok.txt=1\nWRITE docs/no.txt=2",
        "leaf",
        &[],
    ))
    .unwrap();
    f.yard.branch("lead").unwrap().wait_subtree().unwrap();
    let inspected = lead.inspect("leaf").unwrap();
    assert!(inspected.owns.is_empty());
    assert_eq!(inspected.owns_within, [vec!["src".to_owned()]]);
    denied(
        lead.integrate("leaf"),
        &["leaf changed 1 file", "docs/no.txt"],
    );
}

#[test]
fn a_child_is_told_the_paths_it_owns_every_turn() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options).unwrap();
    delegate
        .spawn(owning("SHOW_INSTRUCTIONS", "a", &["src/a", "tests/a_*.rs"]))
        .unwrap();
    delegate
        .spawn(owning("SHOW_INSTRUCTIONS", "free", &[]))
        .unwrap();
    root.wait_subtree().unwrap();
    let said = |name: &str| common::text(&f.yard.branch(name).unwrap().events().unwrap());
    let told = said("a");
    assert!(told.contains("## The paths you own"), "{told}");
    assert!(told.contains("`src/a`, `tests/a_*.rs`"), "{told}");
    assert!(told.contains("is refused"), "{told}");
    let free = said("free");
    assert!(free.contains("instructions:"), "{free}");
    assert!(!free.contains("The paths you own"), "{free}");
}
