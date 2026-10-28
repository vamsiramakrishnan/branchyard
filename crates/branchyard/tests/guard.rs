//! The file guard (`docs/modes.md#the-file-guard`) through the engine: a
//! local harness under `edit-worktree` writes its worktree and commits,
//! and is refused a write beside it; under `read-only` it is refused its
//! worktree and the branch ends with no changes; `full` and no preset run
//! unguarded; the branch records which it got. The fake agent's `WRITE`
//! asks no permission, as a harness that routes nothing to Branchyard.
//!
//! A host without Landlock skips the guarded cases, printing why; the
//! CLI's `tests/guard.rs` turns it off with `BRANCHYARD_GUARD=off`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::path::{Path, PathBuf};

use branchyard::{BranchStatus, Guard, Policy, PolicyPreset, TaskOptions};
use common::{text, Fixture};

/// Why this host cannot hold a harness's writes, when it cannot.
fn unavailable(test: &str) -> bool {
    match branchyard_runtime::LocalProvider::write_guard() {
        Ok(_) => false,
        Err(why) => {
            eprintln!("SKIPPED {test}: no file guard on this host: {why}");
            true
        }
    }
}

fn options(f: &Fixture, policy: Policy) -> TaskOptions {
    TaskOptions {
        policy,
        ..f.options()
    }
}

/// The Landlock guard recorded on a branch: its preset and its scope.
fn landlock(guard: Option<&Guard>) -> (PolicyPreset, Vec<PathBuf>) {
    match guard {
        Some(Guard::Landlock {
            preset,
            abi,
            writable,
        }) => {
            assert!(*abi >= 1);
            (*preset, writable.clone())
        }
        other => panic!("expected a Landlock guard, got {other:?}"),
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap()
}

#[test]
fn edit_worktree_writes_the_worktree_and_is_refused_beside_it() {
    if unavailable("edit_worktree_writes_the_worktree_and_is_refused_beside_it") {
        return;
    }
    let f = Fixture::new();
    let branch = f
        .task("WRITE inside.txt=in WRITE ../outside.txt=out")
        .options(options(&f, PolicyPreset::EditWorktree.policy()))
        .name("edits")
        .run()
        .unwrap();
    let said = text(&branch.events().unwrap());
    assert!(said.contains("wrote inside.txt"), "{said}");
    assert!(
        said.contains("write ../outside.txt failed: Permission denied"),
        "{said}"
    );
    let info = branch.info();
    assert_eq!(info.status, BranchStatus::Ready, "{said}");
    assert_eq!(info.candidate.as_ref().unwrap().files_changed, 1);
    assert!(!info.worktree.parent().unwrap().join("outside.txt").exists());

    let (preset, writable) = landlock(info.guard.as_ref());
    assert_eq!(preset, PolicyPreset::EditWorktree);
    let store = f.root.join(".branchyard");
    for path in [
        info.worktree.clone(),
        canonical(&f.root.join(".git/worktrees/edits")),
        canonical(&f.root.join(".git/objects")),
        canonical(&f.root.join(".git/refs")),
        store.join("tmp/edits"),
        PathBuf::from("/dev"),
        PathBuf::from(std::env::var_os("HOME").unwrap()),
    ] {
        assert!(
            writable.contains(&path),
            "{} not in {writable:?}",
            path.display()
        );
    }
    // The rest of the repository is out of scope: its main checkout and
    // the other branches' state.
    assert!(!writable.contains(&f.root), "{writable:?}");
    assert!(!writable.contains(&store), "{writable:?}");
    // What `by show --json` and the server read.
    let shown = serde_json::to_value(f.yard.branch("edits").unwrap().info()).unwrap();
    assert_eq!(shown["guard"]["kind"], "landlock");
    assert_eq!(shown["guard"]["preset"], "edit-worktree");
}

#[test]
fn a_harness_under_edit_worktree_commits_in_its_worktree() {
    if unavailable("a_harness_under_edit_worktree_commits_in_its_worktree") {
        return;
    }
    let f = Fixture::new();
    let branch = f
        .task(
            "SH printf c > c.txt && git add c.txt && git -c commit.gpgsign=false commit -qm c \
             && echo committed",
        )
        .options(options(&f, PolicyPreset::EditWorktree.policy()))
        .name("commits")
        .run()
        .unwrap();
    let said = text(&branch.events().unwrap());
    assert!(said.contains("sh: 0\ncommitted"), "{said}");
    let info = branch.info();
    assert_eq!(info.status, BranchStatus::Ready, "{said}");
    assert_eq!(info.candidate.as_ref().unwrap().files_changed, 1);
    let log = f.git(&["log", "--format=%s", "by/commits"]);
    assert!(log.lines().any(|subject| subject == "c"), "{log}");
}

#[test]
fn read_only_is_refused_its_worktree_and_ends_with_no_changes() {
    if unavailable("read_only_is_refused_its_worktree_and_ends_with_no_changes") {
        return;
    }
    let f = Fixture::new();
    let branch = f
        .task("WRITE notes.txt=x")
        .options(options(&f, PolicyPreset::ReadOnly.policy()))
        .isolated(true)
        .name("reads")
        .run()
        .unwrap();
    let said = text(&branch.events().unwrap());
    assert!(
        said.contains("write notes.txt failed: Permission denied"),
        "{said}"
    );
    let info = branch.info();
    assert_eq!(info.status, BranchStatus::NoChanges, "{said}");
    assert!(!info.worktree.join("notes.txt").exists());
    let (preset, writable) = landlock(info.guard.as_ref());
    assert_eq!(preset, PolicyPreset::ReadOnly);
    let store = f.root.join(".branchyard");
    assert_eq!(
        writable,
        [
            store.join("homes/reads"),
            store.join("tmp/reads"),
            PathBuf::from("/dev")
        ]
    );
}

#[test]
fn full_and_no_preset_run_unguarded_and_say_why() {
    let f = Fixture::new();
    for (name, policy, reason) in [
        (
            "yes",
            Policy::allow_all(),
            "no permission preset names a file scope (--yes, --ask and explicit rules name none)",
        ),
        (
            "full",
            PolicyPreset::Full.policy(),
            "the full preset allows every tool and names no file scope",
        ),
    ] {
        let branch = f
            .task(&format!("WRITE ../{name}-outside.txt=out"))
            .options(options(&f, policy))
            .name(name)
            .run()
            .unwrap();
        let said = text(&branch.events().unwrap());
        assert!(
            said.contains(&format!("wrote ../{name}-outside.txt")),
            "{said}"
        );
        assert_eq!(
            branch.info().guard,
            Some(Guard::Unguarded {
                reason: reason.into()
            })
        );
        let outside = branch
            .info()
            .worktree
            .parent()
            .unwrap()
            .join(format!("{name}-outside.txt"));
        assert!(outside.exists());
    }
}
