//! Provider snapshots under git checkpoints, end to end against
//! `branchyard_sandbox::fake::FakeProvider` standing in for Microsandbox
//! (processes run on this host through the local provider, with the
//! sandbox's mounts applied by path, and each sandbox's root filesystem a
//! private directory named by `$BY_FAKE_ROOTFS`): a kept sandbox resumed
//! across turns, evicted, and replaced when it vanished; a snapshot with
//! every checkpoint, pruned to the newest K; `fork --at N`, a rewind and a
//! delegated child branching from the matching snapshot, and falling back
//! with the reason when the provider cannot; `[workspace]` setup run inside
//! the sandbox; and a fan whose setup runs once. Hermetic; nothing here is
//! evidence about Microsandbox itself.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::sync::Arc;

use branchyard::{
    Activity, Branch, BranchStatus, Envelope, Policy, Provider, RecordedEvent, SandboxEvent,
    SandboxKeep, SandboxOptions, SandboxOrigin, SnapshotMethod, Spawn, TaskOptions, WorkspacePhase,
    WorkspaceSpec,
};
use branchyard_runtime::LocalProvider;
use branchyard_sandbox::fake::{FakeOp, FakeProvider};
use branchyard_sandbox::{Capabilities, SandboxState};
use common::{text, Fixture};

/// A fake provider for the fixture's yard: `live` declares pause, live
/// branch and full snapshots; otherwise exec only.
fn fake(f: &Fixture, live: bool) -> Arc<FakeProvider> {
    let root = f.dir.join("fake-provider");
    let fake = Arc::new(match live {
        true => FakeProvider::live(Box::new(LocalProvider::new()), root),
        false => FakeProvider::plain(Box::new(LocalProvider::new()), root),
    });
    f.yard.use_sandbox_provider(fake.clone());
    fake
}

fn sandboxed(f: &Fixture, keep: SandboxKeep, snapshots: Option<u32>) -> TaskOptions {
    TaskOptions {
        provider: Some(Provider::Microsandbox(SandboxOptions {
            image: "registry.example/harness:1".into(),
            keep,
            snapshots,
            ..SandboxOptions::default()
        })),
        policy: Policy::allow_all(),
        ..f.options()
    }
}

fn kept(f: &Fixture) -> TaskOptions {
    sandboxed(f, SandboxKeep::Pause, None)
}

fn sandbox_events(events: &[RecordedEvent]) -> Vec<SandboxEvent> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Sandbox(event) => Some(event.as_ref().clone()),
            _ => None,
        })
        .collect()
}

fn origins(branch: &Branch) -> Vec<SandboxOrigin> {
    sandbox_events(&branch.events().unwrap())
        .into_iter()
        .filter_map(|e| match e {
            SandboxEvent::Started { origin, .. } => Some(origin),
            _ => None,
        })
        .collect()
}

fn started_names(branch: &Branch) -> Vec<String> {
    sandbox_events(&branch.events().unwrap())
        .into_iter()
        .filter_map(|e| match e {
            SandboxEvent::Started { sandbox, .. } => Some(sandbox),
            _ => None,
        })
        .collect()
}

fn last_text(branch: &Branch) -> String {
    let events = branch.events().unwrap();
    let start = events
        .iter()
        .rposition(|e| matches!(e.activity, Activity::Prompt(_)))
        .unwrap();
    text(&events[start..])
}

fn paused(fake: &FakeProvider) -> Vec<String> {
    fake.sandboxes()
        .into_iter()
        .filter(|(_, state)| *state == SandboxState::Paused)
        .map(|(name, _)| name)
        .collect()
}

/// A turn that writes `value` into the sandbox's root filesystem, outside
/// the worktree, and a file in the worktree.
fn mark(value: &str) -> String {
    format!("SH printf {value} > \"$BY_FAKE_ROOTFS/marker\" && printf {value} > turn.txt")
}

const READ_MARK: &str = "SH cat \"$BY_FAKE_ROOTFS/marker\"";

#[test]
fn a_kept_sandbox_is_resumed_across_turns_with_a_snapshot_per_checkpoint() {
    let f = Fixture::new();
    let fake = fake(&f, true);
    let branch = f
        .yard
        .task(mark("one"))
        .options(kept(&f))
        .name("warm")
        .run()
        .unwrap();
    assert_eq!(
        branch.info().status,
        BranchStatus::Ready,
        "{:?}",
        branch.events()
    );
    let events = sandbox_events(&branch.events().unwrap());
    assert!(
        matches!(
            &events[0],
            SandboxEvent::Started {
                origin: SandboxOrigin::Fresh { reason: None },
                ..
            }
        ),
        "{events:?}"
    );
    assert!(
        matches!(&events[1], SandboxEvent::Kept { .. }),
        "{events:?}"
    );
    let first = started_names(&branch)[0].clone();
    // The checkpoint records the provider snapshot it took.
    let checkpoint = &branch.checkpoints().unwrap().checkpoints[0].checkpoint;
    let snapshot = checkpoint
        .sandbox
        .clone()
        .expect("a snapshot with checkpoint 1");
    assert_eq!(snapshot.method, SnapshotMethod::LiveBranch);
    assert_eq!(snapshot.provider, "microsandbox");
    assert_eq!(snapshot.scope, branchyard::SandboxScope::Full);
    assert_eq!(snapshot.consistency, branchyard::SandboxConsistency::Crash);
    // Both the kept sandbox and its snapshot child are paused.
    let mut expected = vec![first.clone(), snapshot.handle.clone()];
    expected.sort();
    assert_eq!(paused(&fake), expected);

    let branch = branch.send(READ_MARK, kept(&f)).unwrap();
    assert_eq!(
        started_names(&branch)[1],
        first,
        "the same sandbox, resumed"
    );
    assert_eq!(origins(&branch)[1], SandboxOrigin::Resumed);
    assert!(last_text(&branch).contains("one"), "{}", last_text(&branch));
    assert!(fake.ops().contains(&FakeOp::Resume {
        name: first.clone()
    }));
    // Checkpoint 2 has its own snapshot; the first is kept (default 3).
    let list = branch.checkpoints().unwrap().checkpoints;
    assert!(list.iter().all(|e| e.checkpoint.sandbox.is_some()));
    assert_eq!(paused(&fake).len(), 3);

    // Removal destroys the kept sandbox and releases the snapshots.
    f.yard.remove("warm").unwrap();
    assert!(fake.sandboxes().is_empty(), "{:?}", fake.sandboxes());
}

#[test]
fn snapshots_are_pruned_to_the_newest_k_and_released() {
    let f = Fixture::new();
    let fake = fake(&f, true);
    let options = sandboxed(&f, SandboxKeep::Pause, Some(1));
    let branch = f
        .yard
        .task(mark("one"))
        .options(options.clone())
        .name("pruned")
        .run()
        .unwrap();
    let first = branch.checkpoints().unwrap().checkpoints[0]
        .checkpoint
        .sandbox
        .clone()
        .unwrap()
        .handle;
    let branch = branch.send(&mark("two"), options).unwrap();
    let released: Vec<(String, u32)> = sandbox_events(&branch.events().unwrap())
        .into_iter()
        .filter_map(|e| match e {
            SandboxEvent::Released { handle, turn, .. } => Some((handle, turn)),
            _ => None,
        })
        .collect();
    assert_eq!(released, [(first.clone(), 1)]);
    assert!(fake.ops().contains(&FakeOp::Destroy {
        name: first.clone()
    }));
    // The kept sandbox and checkpoint 2's snapshot remain.
    assert_eq!(paused(&fake).len(), 2);
    assert!(!paused(&fake).contains(&first));
}

#[test]
fn fork_at_a_checkpoint_branches_from_its_snapshot_with_its_own_worktree() {
    let f = Fixture::new();
    let fake = fake(&f, true);
    let branch = f
        .yard
        .task(mark("one"))
        .options(kept(&f))
        .name("source")
        .run()
        .unwrap();
    let branch = branch.send(&mark("two"), kept(&f)).unwrap();
    let snapshot_one = branch.checkpoints().unwrap().checkpoints[0]
        .checkpoint
        .sandbox
        .clone()
        .unwrap()
        .handle;
    let fork = branch
        .fork_at(
            1,
            READ_MARK,
            TaskOptions {
                name: Some("fork".into()),
                ..kept(&f)
            },
        )
        .unwrap();
    assert_eq!(
        fork.info().status,
        BranchStatus::NoChanges,
        "{:?}",
        fork.events()
    );
    assert_eq!(
        origins(&fork),
        [SandboxOrigin::Branched {
            branch: "source".into(),
            turn: 1,
            method: SnapshotMethod::LiveBranch,
        }]
    );
    // The root filesystem is checkpoint 1's, the worktree git's checkpoint 1.
    assert!(last_text(&fork).contains("one"), "{}", last_text(&fork));
    assert_eq!(
        fs::read_to_string(fork.info().worktree.join("turn.txt")).unwrap(),
        "one"
    );
    let child = started_names(&fork)[0].clone();
    assert!(fake.ops().contains(&FakeOp::BranchLive {
        source: snapshot_one,
        children: vec![child.clone()],
    }));
    // Rebound: the child's workspace mount is its own worktree.
    let spec = fake.spec(&child).unwrap();
    let workspace = spec
        .mounts
        .iter()
        .find(|m| m.guest == std::path::Path::new(branchyard::SANDBOX_WORKSPACE))
        .unwrap();
    assert_eq!(workspace.host, fork.info().worktree);
    let line = SandboxEvent::Started {
        provider: "microsandbox".into(),
        sandbox: child,
        origin: origins(&fork)[0].clone(),
    }
    .describe();
    assert_eq!(
        line,
        "sandbox: branched from source's checkpoint 1 (microsandbox live branch)"
    );
}

#[test]
fn a_provider_that_cannot_pause_or_branch_falls_back_and_says_why() {
    let f = Fixture::new();
    let fake = fake(&f, false);
    let branch = f
        .yard
        .task(mark("one"))
        .options(kept(&f))
        .name("plain")
        .run()
        .unwrap();
    assert_eq!(
        branch.info().status,
        BranchStatus::Ready,
        "{:?}",
        branch.events()
    );
    let events = sandbox_events(&branch.events().unwrap());
    assert!(
        matches!(&events[1], SandboxEvent::NotKept { reason, .. } if reason.contains("can't pause")),
        "{events:?}"
    );
    assert!(fake.sandboxes().is_empty(), "destroyed at the turn's end");
    assert!(branch.checkpoints().unwrap().checkpoints[0]
        .checkpoint
        .sandbox
        .is_none());
    let fork = branch
        .fork_at(
            1,
            READ_MARK,
            TaskOptions {
                name: Some("fallback".into()),
                ..kept(&f)
            },
        )
        .unwrap();
    match &origins(&fork)[0] {
        SandboxOrigin::Fresh {
            reason: Some(reason),
        } => {
            assert!(reason.contains("can't branch"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    // Today's path: git set the worktree to checkpoint 1.
    assert_eq!(
        fs::read_to_string(fork.info().worktree.join("turn.txt")).unwrap(),
        "one"
    );
}

#[test]
fn a_rewind_restores_the_sandbox_from_its_own_snapshot() {
    let f = Fixture::new();
    let fake = fake(&f, true);
    let branch = f
        .yard
        .task(mark("one"))
        .options(kept(&f))
        .name("rewound")
        .run()
        .unwrap();
    let branch = branch.send(&mark("two"), kept(&f)).unwrap();
    let kept_name = started_names(&branch)[0].clone();
    branch.rewind(1).unwrap();
    let branch = f.yard.branch("rewound").unwrap();
    let events = sandbox_events(&branch.events().unwrap());
    assert!(
        events.iter().any(|e| matches!(e, SandboxEvent::NotKept { reason, .. } if reason.contains("rewound to checkpoint 1"))),
        "{events:?}"
    );
    assert!(
        fake.inspect_state(&kept_name).is_none(),
        "the later sandbox went"
    );
    let branch = branch.send(READ_MARK, kept(&f)).unwrap();
    assert_eq!(
        origins(&branch).last().unwrap(),
        &SandboxOrigin::Branched {
            branch: "rewound".into(),
            turn: 1,
            method: SnapshotMethod::LiveBranch,
        }
    );
    assert!(last_text(&branch).contains("one"), "{}", last_text(&branch));
}

#[test]
fn kept_sandboxes_beyond_max_paused_are_evicted_least_recently_used_first() {
    let f = Fixture::new();
    let fake = fake(&f, true);
    let options = |name: &str| TaskOptions {
        name: Some(name.into()),
        provider: Some(Provider::Microsandbox(SandboxOptions {
            image: "registry.example/harness:1".into(),
            keep: SandboxKeep::Pause,
            snapshots: Some(0),
            max_paused: Some(1),
            ..SandboxOptions::default()
        })),
        ..kept(&f)
    };
    let a = f.yard.task("SH true").options(options("a")).run().unwrap();
    let a_sandbox = started_names(&a)[0].clone();
    assert_eq!(paused(&fake), std::slice::from_ref(&a_sandbox));
    let b = f.yard.task("SH true").options(options("b")).run().unwrap();
    let evicted: Vec<(String, String)> = sandbox_events(&b.events().unwrap())
        .into_iter()
        .filter_map(|e| match e {
            SandboxEvent::Evicted {
                sandbox, branch, ..
            } => Some((branch, sandbox)),
            _ => None,
        })
        .collect();
    assert_eq!(evicted, [("a".to_owned(), a_sandbox.clone())]);
    assert_eq!(paused(&fake), started_names(&b));
    // a's next turn has nothing kept: a fresh sandbox.
    let a = a.send("SH true", options("a")).unwrap();
    assert!(
        matches!(origins(&a).last().unwrap(), SandboxOrigin::Fresh { .. }),
        "{:?}",
        origins(&a)
    );
}

#[test]
fn a_kept_sandbox_that_vanished_falls_back_to_a_fresh_one_recorded_as_such() {
    let f = Fixture::new();
    let fake = fake(&f, true);
    let branch = f
        .yard
        .task(mark("one"))
        .options(kept(&f))
        .name("lost")
        .run()
        .unwrap();
    let kept_name = started_names(&branch)[0].clone();
    fake.vanish(&kept_name);
    let branch = branch.send(READ_MARK, kept(&f)).unwrap();
    // Reading changes nothing: the first turn's candidate stays, ready.
    assert_eq!(branch.info().status, BranchStatus::Ready);
    match origins(&branch).last().unwrap() {
        SandboxOrigin::Fresh {
            reason: Some(reason),
        } => assert!(
            reason.contains(&format!("its kept sandbox {kept_name} no longer exists")),
            "{reason}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(
        last_text(&branch).contains("No such file"),
        "{}",
        last_text(&branch)
    );
}

#[test]
fn setup_runs_inside_the_sandbox_and_what_it_installs_lives_there() {
    let f = Fixture::new();
    fs::write(f.root.join(".gitignore"), "deps/\n").unwrap();
    f.git(&["add", "."]);
    f.git(&["commit", "-q", "-m", "ignore deps"]);
    let fake = fake(&f, true);
    let setup = "printf installed > \"$BY_FAKE_ROOTFS/toolchain\" && mkdir -p deps && \
                 printf lib > deps/lib.txt && printf \"$BRANCHYARD_WORKTREE\" > deps/where";
    let options = TaskOptions {
        workspace: Some(WorkspaceSpec {
            setup: vec![setup.into()],
            ..WorkspaceSpec::default()
        }),
        ..kept(&f)
    };
    let branch = f
        .yard
        .task("SH cat \"$BY_FAKE_ROOTFS/toolchain\" deps/lib.txt")
        .options(options)
        .name("setup")
        .run()
        .unwrap();
    assert_eq!(
        branch.info().status,
        BranchStatus::NoChanges,
        "{:?}",
        branch.events()
    );
    let sandbox = started_names(&branch)[0].clone();
    assert!(fake.ops().contains(&FakeOp::Exec {
        sandbox: sandbox.clone(),
        argv: vec!["sh".into(), "-c".into(), setup.into()],
        cwd: branchyard::SANDBOX_WORKSPACE.into(),
    }));
    let report = branch
        .events()
        .unwrap()
        .into_iter()
        .find_map(|e| match e.activity {
            Activity::Workspace(r) if r.phase == WorkspacePhase::Setup => Some(r),
            _ => None,
        })
        .unwrap();
    assert!(report.ok, "{report:?}");
    assert_eq!(report.ran_in, Some(branchyard::RanIn::Sandbox));
    // The harness saw what setup installed, in and outside the worktree.
    assert!(
        last_text(&branch).contains("installedlib"),
        "{}",
        last_text(&branch)
    );
    // Setup was told the worktree where the sandbox has it (the fake maps
    // that path to this host's worktree, as a mount does).
    assert_eq!(
        fs::read_to_string(branch.info().worktree.join("deps/where"))
            .unwrap()
            .trim_end_matches('/'),
        branch.info().worktree.display().to_string()
    );
    // A fork from its snapshot inherits the setup instead of running it.
    let fork = branch
        .fork_at(
            1,
            "SH cat \"$BY_FAKE_ROOTFS/toolchain\" deps/lib.txt",
            TaskOptions {
                name: Some("inherits".into()),
                ..kept(&f)
            },
        )
        .unwrap();
    assert!(
        last_text(&fork).contains("installedlib"),
        "{}",
        last_text(&fork)
    );
    let inherited = fork
        .events()
        .unwrap()
        .into_iter()
        .find_map(|e| match e.activity {
            Activity::Workspace(r) if r.phase == WorkspacePhase::Setup => Some(r),
            _ => None,
        })
        .unwrap();
    assert_eq!(inherited.inherited_from.as_deref(), Some("setup"));
    let setups = fake
        .ops()
        .into_iter()
        .filter(|op| matches!(op, FakeOp::Exec { argv, .. } if argv.get(2).map(String::as_str) == Some(setup)))
        .count();
    assert_eq!(setups, 1, "setup ran once");
}

#[test]
fn a_fan_runs_setup_once_and_branches_every_sandbox_from_it() {
    let f = Fixture::new();
    fs::write(f.root.join(".gitignore"), "deps/\n").unwrap();
    f.git(&["add", "."]);
    f.git(&["commit", "-q", "-m", "ignore deps"]);
    let fake = fake(&f, true);
    let setup = "printf installed > \"$BY_FAKE_ROOTFS/toolchain\" && mkdir -p deps && \
                 printf lib > deps/lib.txt";
    let options = TaskOptions {
        workspace: Some(WorkspaceSpec {
            setup: vec![setup.into()],
            ..WorkspaceSpec::default()
        }),
        name: Some("fan".into()),
        ..sandboxed(&f, SandboxKeep::Destroy, None)
    };
    let branches = f
        .yard
        .task("SH cat \"$BY_FAKE_ROOTFS/toolchain\" deps/lib.txt")
        .options(options)
        .run_on(&["gemini-cli", "qwen-code"])
        .unwrap();
    let setups = fake
        .ops()
        .into_iter()
        .filter(|op| matches!(op, FakeOp::Exec { argv, .. } if argv.get(2).map(String::as_str) == Some(setup)))
        .count();
    assert_eq!(setups, 1, "{:?}", fake.ops());
    let batches: Vec<Vec<String>> = fake
        .ops()
        .into_iter()
        .filter_map(|op| match op {
            FakeOp::BranchLive { children, .. } => Some(children),
            _ => None,
        })
        .collect();
    assert_eq!(batches.len(), 1, "one branch_live for every branch");
    assert_eq!(batches[0].len(), 2);
    let first = branches[0].info().name.clone();
    for branch in &branches {
        assert_eq!(
            branch.info().status,
            BranchStatus::NoChanges,
            "{:?}",
            branch.events()
        );
        assert_eq!(
            origins(branch),
            [SandboxOrigin::Prepared {
                branch: first.clone(),
                method: SnapshotMethod::LiveBranch,
            }]
        );
        assert!(
            last_text(branch).contains("installedlib"),
            "{}",
            last_text(branch)
        );
    }
    let second = branches[1].events().unwrap();
    assert!(second.iter().any(|e| matches!(&e.activity,
        Activity::Workspace(r) if r.inherited_from.as_deref() == Some(first.as_str()))));
    // Destroyed at the turns' ends (keep = destroy), the prepared one too.
    assert!(fake.sandboxes().is_empty(), "{:?}", fake.sandboxes());
}

#[test]
fn a_fan_member_inherits_a_copied_file_as_setup_changed_it() {
    let f = Fixture::new();
    fs::write(f.root.join(".gitignore"), ".env\n").unwrap();
    f.git(&["add", "."]);
    f.git(&["commit", "-q", "-m", "ignore .env"]);
    fs::write(f.root.join(".env"), "COPIED=1\n").unwrap();
    let fake = fake(&f, true);
    let setup = "printf 'SETUP=1\\n' >> .env";
    let options = TaskOptions {
        workspace: Some(WorkspaceSpec {
            copy: vec![".env".into()],
            setup: vec![setup.into()],
            ..WorkspaceSpec::default()
        }),
        name: Some("envfan".into()),
        ..sandboxed(&f, SandboxKeep::Destroy, None)
    };
    let branches = f
        .yard
        .task("SH cat .env")
        .options(options)
        .run_on(&["gemini-cli", "qwen-code"])
        .unwrap();
    let setups = fake
        .ops()
        .into_iter()
        .filter(|op| matches!(op, FakeOp::Exec { argv, .. } if argv.get(2).map(String::as_str) == Some(setup)))
        .count();
    assert_eq!(setups, 1, "{:?}", fake.ops());
    let first = branches[0].info().name.clone();
    assert!(branches[1]
        .events()
        .unwrap()
        .iter()
        .any(|e| matches!(&e.activity,
        Activity::Workspace(r) if r.inherited_from.as_deref() == Some(first.as_str()))));
    for branch in &branches {
        assert_eq!(
            fs::read_to_string(branch.info().worktree.join(".env")).unwrap(),
            "COPIED=1\nSETUP=1\n",
            "{}",
            branch.info().name
        );
        assert!(
            last_text(branch).contains("SETUP=1"),
            "{}",
            last_text(branch)
        );
    }
    // The repository's own copy is untouched.
    assert_eq!(
        fs::read_to_string(f.root.join(".env")).unwrap(),
        "COPIED=1\n"
    );
}

#[test]
fn a_sandboxed_teardown_gets_the_branchs_port() {
    let f = Fixture::new();
    let fake = fake(&f, true);
    let marker = f.dir.join("torn-down");
    let options = TaskOptions {
        workspace: Some(WorkspaceSpec {
            setup: vec!["true".into()],
            teardown: vec![format!(
                "printf \"$BRANCHYARD_BRANCH $BRANCHYARD_PORT\" > {}",
                marker.display()
            )],
            ..WorkspaceSpec::default()
        }),
        ..kept(&f)
    };
    f.yard
        .task("SH true")
        .options(options)
        .name("torn")
        .run()
        .unwrap();
    let port = f.yard.workspace("torn").unwrap().port.unwrap();
    let report = f
        .yard
        .remove_reporting("torn", &Default::default())
        .unwrap()
        .expect("a teardown ran");
    assert!(report.ok, "{report:?}");
    assert_eq!(report.ran_in, Some(branchyard::RanIn::Sandbox));
    assert!(fake
        .ops()
        .iter()
        .any(|op| matches!(op, FakeOp::Exec { argv, .. } if argv.get(2).is_some_and(|c| c.contains("torn-down")))));
    assert_eq!(fs::read_to_string(&marker).unwrap(), format!("torn {port}"));
}

#[test]
fn a_fan_on_a_provider_that_cannot_live_branch_runs_setup_in_each_branch() {
    let f = Fixture::new();
    let fake = fake(&f, false);
    let options = TaskOptions {
        workspace: Some(WorkspaceSpec {
            setup: vec!["true".into()],
            ..WorkspaceSpec::default()
        }),
        name: Some("fanned".into()),
        ..sandboxed(&f, SandboxKeep::Destroy, None)
    };
    let branches = f
        .yard
        .task("SH true")
        .options(options)
        .run_on(&["gemini-cli", "qwen-code"])
        .unwrap();
    for branch in &branches {
        match &origins(branch)[0] {
            SandboxOrigin::Fresh {
                reason: Some(reason),
            } => {
                assert!(reason.contains("can't live-branch"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }
    let setups = fake
        .ops()
        .into_iter()
        .filter(|op| matches!(op, FakeOp::Exec { argv, .. } if argv.get(2).map(String::as_str) == Some("true")))
        .count();
    assert_eq!(setups, 2);
}

#[test]
fn a_delegated_child_starts_from_its_parents_snapshot() {
    let f = Fixture::new();
    let fake = fake(&f, true);
    let root = f
        .yard
        .task(mark("one"))
        .options(kept(&f))
        .name("parent")
        .run()
        .unwrap();
    // A sandboxed harness gets no delegation tools yet, so the grant comes
    // from a send whose turn is refused before its sandbox starts.
    let delegating = TaskOptions {
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec![common::fake_agent().display().to_string()]),
        ..kept(&f)
    };
    let refused = root.send("SH true", delegating.clone()).unwrap();
    assert!(
        matches!(&refused.info().status, BranchStatus::Failed { reason } if reason.contains("delegation")),
        "{:?}",
        refused.info().status
    );
    let root = f.yard.branch("parent").unwrap();
    let delegate = root.delegate(delegating).unwrap();
    delegate
        .spawn(Spawn {
            prompt: READ_MARK.into(),
            name: Some("kid".into()),
            ..Spawn::default()
        })
        .unwrap();
    root.wait_subtree().unwrap();
    let kid = f.yard.branch("kid").unwrap();
    assert_eq!(
        origins(&kid),
        [SandboxOrigin::Branched {
            branch: "parent".into(),
            turn: 1,
            method: SnapshotMethod::LiveBranch,
        }],
        "{:?}",
        kid.events()
    );
    assert!(last_text(&kid).contains("one"), "{}", last_text(&kid));
    assert!(fake
        .ops()
        .iter()
        .any(|op| matches!(op, FakeOp::BranchLive { .. })));
}

/// Capabilities are what choose the path, not the provider's name.
#[test]
fn the_path_follows_declared_capabilities() {
    let f = Fixture::new();
    let only_pause = Arc::new(FakeProvider::new(
        Box::new(LocalProvider::new()),
        f.dir.join("pause-only"),
        Capabilities {
            exec: true,
            pause: true,
            ..Capabilities::default()
        },
    ));
    f.yard.use_sandbox_provider(only_pause.clone());
    let branch = f
        .yard
        .task(mark("one"))
        .options(kept(&f))
        .name("pausing")
        .run()
        .unwrap();
    let events = sandbox_events(&branch.events().unwrap());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SandboxEvent::Kept { .. })),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, SandboxEvent::NoSnapshot { turn: 1, reason } if reason.contains("can't branch"))),
        "{events:?}"
    );
    let branch = branch.send(READ_MARK, kept(&f)).unwrap();
    assert_eq!(origins(&branch)[1], SandboxOrigin::Resumed);
    assert!(last_text(&branch).contains("one"));
}

trait InspectState {
    fn inspect_state(&self, name: &str) -> Option<SandboxState>;
}

impl InspectState for FakeProvider {
    fn inspect_state(&self, name: &str) -> Option<SandboxState> {
        self.sandboxes()
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, state)| state)
    }
}

/// `[workspace] prepare` on a sandboxed branch: setup runs once in a
/// sandbox, which is snapshotted (a paused live branch) as the key's
/// prepared environment; the next branch's sandbox branches from it with
/// setup's work inside and outside the worktree, and runs no setup. A
/// build that fails is recorded, and later branches start from the last
/// good environment, saying so. Pruning releases the snapshot.
#[test]
fn a_prepared_environment_is_a_snapshot_later_sandboxes_branch_from() {
    let f = Fixture::new();
    fs::write(f.root.join(".gitignore"), "deps/\n").unwrap();
    fs::write(f.root.join("pnpm-lock.yaml"), "v1").unwrap();
    f.git(&["add", "."]);
    f.git(&["commit", "-q", "-m", "lockfile"]);
    let fake = fake(&f, true);
    let log = f.dir.join("setups.log");
    let setup = format!(
        "echo run >> {log}; if grep -q broken pnpm-lock.yaml; then exit 7; fi; \
         printf installed > \"$BY_FAKE_ROOTFS/toolchain\" && mkdir -p deps && \
         printf lib > deps/lib.txt",
        log = log.display()
    );
    let options = TaskOptions {
        workspace: Some(WorkspaceSpec {
            setup: vec![setup],
            prepare: true,
            ..WorkspaceSpec::default()
        }),
        ..sandboxed(&f, SandboxKeep::Destroy, Some(0))
    };
    let runs = || fs::read_to_string(&log).unwrap_or_default().lines().count();
    let read = "SH cat \"$BY_FAKE_ROOTFS/toolchain\" deps/lib.txt";
    let a = f
        .yard
        .task(read)
        .options(options.clone())
        .name("a")
        .run()
        .unwrap();
    assert!(last_text(&a).contains("installedlib"), "{}", last_text(&a));
    assert_eq!(runs(), 1);
    let envs = f.yard.environments();
    assert_eq!(envs.len(), 1, "{envs:?}");
    let snapshot = envs[0].snapshot.clone().unwrap();
    assert_eq!(snapshot.method, SnapshotMethod::LiveBranch);
    assert_eq!(envs[0].produced, ["deps"]);
    assert!(
        envs[0].place.starts_with("microsandbox"),
        "{}",
        envs[0].place
    );
    assert!(paused(&fake).contains(&snapshot.handle));

    let b = f
        .yard
        .task(read)
        .options(options.clone())
        .name("b")
        .run()
        .unwrap();
    assert_eq!(runs(), 1, "setup ran again");
    assert!(last_text(&b).contains("installedlib"), "{}", last_text(&b));
    assert_eq!(
        origins(&b),
        [SandboxOrigin::Environment {
            key: envs[0].key.clone(),
            used: None,
            method: SnapshotMethod::LiveBranch,
            reason: None,
        }]
    );
    let setup_report = b
        .events()
        .unwrap()
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Workspace(r) if r.phase == WorkspacePhase::Setup => Some(r.clone()),
            _ => None,
        })
        .unwrap();
    assert!(setup_report.commands.is_empty());
    assert_eq!(
        setup_report.environment.unwrap().origin,
        branchyard::EnvironmentOrigin::Restored
    );

    // A lockfile that breaks setup: that branch fails, the failure is
    // recorded, and the next branch starts from the last good build.
    fs::write(f.root.join("pnpm-lock.yaml"), "broken").unwrap();
    f.git(&["commit", "-q", "-am", "broken"]);
    let c = f
        .yard
        .task(read)
        .options(options.clone())
        .name("c")
        .run()
        .unwrap();
    assert!(
        matches!(c.info().status, BranchStatus::Failed { .. }),
        "{:?}",
        c.info().status
    );
    assert_eq!(runs(), 2);
    let d = f
        .yard
        .task(read)
        .options(options.clone())
        .name("d")
        .run()
        .unwrap();
    assert_eq!(runs(), 2);
    assert!(last_text(&d).contains("installedlib"), "{}", last_text(&d));
    let origin = origins(&d).pop().unwrap();
    let SandboxOrigin::Environment { used, reason, .. } = origin else {
        panic!("{origin:?}");
    };
    assert_eq!(used.as_deref(), Some(envs[0].key.as_str()));
    assert!(reason.unwrap().contains("by env rebuild"));

    // Pruning it releases the snapshot.
    let pruned = f.yard.prune_environments(
        1,
        std::time::Duration::from_secs(3600),
        std::slice::from_ref(&envs[0].key),
    );
    assert_eq!(pruned.removed.len(), 1, "{pruned:?}");
    assert!(!paused(&fake).contains(&snapshot.handle));
}

#[test]
fn a_provider_that_cannot_live_branch_keeps_no_prepared_environment() {
    let f = Fixture::new();
    fake(&f, false);
    let options = TaskOptions {
        workspace: Some(WorkspaceSpec {
            setup: vec!["mkdir -p deps".into()],
            prepare: true,
            ..WorkspaceSpec::default()
        }),
        ..sandboxed(&f, SandboxKeep::Destroy, Some(0))
    };
    let a = f
        .yard
        .task("WRITE a.txt=1")
        .options(options)
        .name("a")
        .run()
        .unwrap();
    assert_eq!(a.info().status, BranchStatus::Ready);
    let used = a
        .events()
        .unwrap()
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Workspace(r) if r.phase == WorkspacePhase::Setup => r.environment.clone(),
            _ => None,
        })
        .unwrap();
    assert_eq!(used.origin, branchyard::EnvironmentOrigin::NotKept);
    assert!(used.reason.unwrap().contains("can't branch"));
    assert!(f.yard.environments().is_empty());
}
