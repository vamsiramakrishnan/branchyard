//! Task graphs through the SDK against the fake ACP agent: dependent
//! children that start only once their prerequisites settle, in whichever
//! process settles them; blocked dependents; proposals that commit whole
//! or change nothing; recovery of a dependent no engine started; and
//! scratch-area bindings. The MCP tool, the CLI, the Python module and the
//! server are tested in their own crates.

#![allow(clippy::let_underscore_must_use, clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::process::{Child, Command, Stdio};

use branchyard::{
    Access, After, Binding, BranchStatus, Budget, ChildBudget, Dependency, DependencyRef, Envelope,
    Error, GraphEdit, Policy, SpawnSpec, TaskOptions, Yard,
};
use branchyard_testkit::wait;
use common::{fake_agent, Fixture};

/// Root options that may delegate. No prompt here starts the MCP server,
/// so any executable stands in for it.
fn delegating(f: &Fixture, envelope: Envelope) -> TaskOptions {
    TaskOptions {
        delegation: Some(envelope),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    }
}

fn root(f: &Fixture, options: &TaskOptions) -> branchyard::Branch {
    f.yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap()
}

fn spawn(name: &str, prompt: &str, depends_on: &[&str]) -> GraphEdit {
    GraphEdit::Spawn(SpawnSpec {
        prompt: prompt.into(),
        name: Some(name.into()),
        depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
        ..SpawnSpec::default()
    })
}

fn status(yard: &Yard, name: &str) -> BranchStatus {
    yard.branch(name).unwrap().info().status.clone()
}

fn denied(result: Result<impl std::fmt::Debug, Error>, needle: &str) {
    match result {
        Err(Error::Denied(why)) if why.contains(needle) => {}
        other => panic!("expected a denial mentioning {needle:?}, got {other:?}"),
    }
}

#[test]
fn a_dependent_starts_only_after_its_prerequisite_settles() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = root(&f, &options);
    let delegate = root.delegate(options).unwrap();
    assert_eq!(delegate.graph("root").unwrap().revision, 0);
    let applied = delegate
        .apply_graph(
            vec![
                spawn("a", "AWAIT_STEER", &[]),
                spawn("b", "WRITE b.txt=after-a", &["a"]),
                spawn("c", "WRITE c.txt=after-b", &["b"]),
            ],
            0,
        )
        .unwrap();
    assert_eq!(applied.revision, 1);
    let states: Vec<(&str, &BranchStatus)> = applied
        .spawned
        .iter()
        .map(|s| (s.name.as_str(), &s.status))
        .collect();
    assert_eq!(
        states,
        [
            ("a", &BranchStatus::Running),
            ("b", &BranchStatus::Waiting),
            ("c", &BranchStatus::Waiting)
        ]
    );
    assert_eq!(applied.spawned[1].depends_on, ["a"]);
    assert_eq!(applied.spawned[1].base, "", "no base until it starts");
    assert_eq!(
        applied.dependencies,
        [
            Dependency {
                dependent: "b".into(),
                prerequisite: "a".into(),
                after: After::Settled
            },
            Dependency {
                dependent: "c".into(),
                prerequisite: "b".into(),
                after: After::Settled
            },
        ]
    );
    // Waiting children have no worktree and ran no turn.
    wait::until("a to wait for steering", || {
        delegate
            .inspect("a")
            .is_ok_and(|i| i.last_message.contains("waiting for steering"))
    });
    let b = f.yard.branch("b").unwrap().info().clone();
    assert_eq!(b.status, BranchStatus::Waiting);
    assert!(!b.worktree.exists());
    assert_eq!(b.turns, 0);
    let inspected = delegate.inspect("b").unwrap();
    assert_eq!(inspected.depends_on.len(), 1);
    assert_eq!(delegate.inspect("root").unwrap().graph_revision, 1);
    // A waiting child cannot be sent to; it starts when a settles.
    denied(delegate.send("b", "go"), "waiting for its prerequisites");
    // The parent's branch moves on meanwhile; b starts from it as it is
    // when b starts.
    std::fs::write(root.info().worktree.join("later.txt"), "l\n").unwrap();
    f.git(&["-C", root.info().worktree.to_str().unwrap(), "add", "."]);
    f.git(&[
        "-C",
        root.info().worktree.to_str().unwrap(),
        "commit",
        "-q",
        "-m",
        "later",
    ]);
    delegate.steer("a", "done").unwrap();
    let done = root.wait_subtree().unwrap();
    let finished: Vec<(&str, &BranchStatus)> =
        done.iter().map(|i| (i.name.as_str(), &i.status)).collect();
    assert_eq!(
        finished,
        [
            ("a", &BranchStatus::NoChanges),
            ("b", &BranchStatus::Ready),
            ("c", &BranchStatus::Ready)
        ]
    );
    let b = f.yard.branch("b").unwrap().info().clone();
    assert!(
        b.worktree.join("later.txt").is_file(),
        "based on root's head"
    );
    assert_eq!(b.base, f.git(&["rev-parse", "by/root"]).trim());
    // The graph shows the edges and each child's status.
    let graph = f.yard.graph("root").unwrap();
    assert_eq!(graph.revision, 1);
    assert_eq!(graph.children[2].depends_on, ["b"]);
    assert_eq!(graph.children[2].status, BranchStatus::Ready);
    // The parent's log records the proposal.
    let log = root.events().unwrap();
    assert!(log.iter().any(|e| matches!(&e.activity,
        branchyard::Activity::Delegation { tool, refused: false, outcome, .. }
            if tool == "apply_graph" && outcome.contains("b (waiting for a)"))));
}

#[test]
fn a_prerequisite_that_fails_blocks_its_dependents_until_the_graph_changes() {
    let f = Fixture::new();
    let options = delegating(
        &f,
        Envelope {
            max_children: 10,
            ..Envelope::default()
        },
    );
    let root = root(&f, &options);
    let delegate = root.delegate(options).unwrap();
    delegate
        .apply_graph(
            vec![
                spawn("bad", "EXIT", &[]),
                spawn("next", "WRITE n.txt=1", &["bad"]),
                spawn("last", "WRITE l.txt=1", &["next"]),
                spawn("hang", "HANG", &[]),
                spawn("after-hang", "WRITE h.txt=1", &["hang"]),
            ],
            0,
        )
        .unwrap();
    wait::until("bad to fail", || {
        matches!(status(&f.yard, "bad"), BranchStatus::Failed { .. })
    });
    delegate.cancel("hang").unwrap();
    root.wait_subtree().unwrap();
    for (name, why) in [
        ("next", "its prerequisite bad failed"),
        ("last", "its prerequisite next is blocked"),
        ("after-hang", "its prerequisite hang was interrupted"),
    ] {
        match status(&f.yard, name) {
            BranchStatus::Blocked { reason } => assert!(reason.contains(why), "{name}: {reason}"),
            other => panic!("{name} is {other:?}"),
        }
    }
    // A blocked child never ran.
    assert_eq!(f.yard.branch("next").unwrap().info().turns, 0);
    denied(delegate.send("next", "go"), "is blocked");
    // Removing the failed prerequisite reopens it, and it starts.
    let revision = delegate.graph("root").unwrap().revision;
    let applied = delegate
        .apply_graph(
            vec![GraphEdit::RemoveDependency(DependencyRef {
                dependent: "next".into(),
                prerequisite: "bad".into(),
            })],
            revision,
        )
        .unwrap();
    assert_eq!(applied.revision, revision + 1);
    root.wait_subtree().unwrap();
    assert_eq!(status(&f.yard, "next"), BranchStatus::Ready);
    // last stays blocked: its prerequisite blocked it before it recovered.
    assert!(matches!(
        status(&f.yard, "last"),
        BranchStatus::Blocked { .. }
    ));
    // Cancelling a child still waiting ends it without a turn, and blocks
    // what waits for it.
    let revision = delegate.graph("root").unwrap().revision;
    delegate
        .apply_graph(
            vec![
                spawn("gate", "HANG", &[]),
                spawn("gated", "say x", &["gate"]),
                spawn("gated2", "say y", &["gated"]),
            ],
            revision,
        )
        .unwrap();
    assert_eq!(
        delegate.cancel("gated").unwrap().cancelled,
        ["gated".to_owned()]
    );
    assert_eq!(status(&f.yard, "gated"), BranchStatus::Interrupted);
    assert!(matches!(
        status(&f.yard, "gated2"),
        BranchStatus::Blocked { .. }
    ));
    delegate.cancel("gate").unwrap();
    root.wait_subtree().unwrap();
}

/// Each invalid proposal is refused whole: no branch, no dependency, no
/// revision, no name taken, and the parent's budget untouched.
#[test]
fn an_invalid_proposal_changes_nothing() {
    let f = Fixture::new();
    let options = TaskOptions {
        budget: Budget::usd(1.0),
        ..delegating(
            &f,
            Envelope {
                max_depth: 2,
                max_children: 4,
                harnesses: Vec::new(),
            },
        )
    };
    let root = root(&f, &options);
    let other = f
        .yard
        .task("say other")
        .options(options.clone())
        .name("other")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    let budgeted = |name: &str, usd: f64, depends_on: &[&str]| {
        GraphEdit::Spawn(SpawnSpec {
            prompt: "WRITE x.txt=1".into(),
            name: Some(name.into()),
            budget: Some(ChildBudget {
                max_usd: Some(usd),
                ..ChildBudget::default()
            }),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            ..SpawnSpec::default()
        })
    };
    delegate
        .apply_graph(vec![budgeted("kid", 0.2, &[])], 0)
        .unwrap();
    other
        .delegate(options.clone())
        .unwrap()
        .apply_graph(vec![budgeted("theirs", 0.2, &[])], 0)
        .unwrap();
    root.wait_subtree().unwrap();
    other.wait_subtree().unwrap();
    f.yard
        .branch("kid")
        .unwrap()
        .delegate(options.clone())
        .unwrap()
        .apply_graph(vec![budgeted("grandkid", 0.1, &[])], 0)
        .unwrap();
    root.wait_subtree().unwrap();
    let before = delegate.inspect("root").unwrap();
    let edge = |dependent: &str, prerequisite: &str| {
        GraphEdit::AddDependency(Dependency {
            dependent: dependent.into(),
            prerequisite: prerequisite.into(),
            after: After::Settled,
        })
    };
    let refusals: Vec<(Vec<GraphEdit>, u64, &str)> = vec![
        (
            vec![budgeted("p", 0.1, &["q"]), budgeted("q", 0.1, &["p"])],
            1,
            "dependency cycle",
        ),
        (
            vec![
                budgeted("p", 0.1, &[]),
                budgeted("q", 0.1, &[]),
                edge("p", "p"),
            ],
            1,
            "cannot depend on itself",
        ),
        (vec![budgeted("p", 0.1, &[])], 0, "stale"),
        (
            vec![budgeted("p", 0.1, &["theirs"])],
            1,
            "not a child of root",
        ),
        (
            vec![budgeted("p", 0.1, &["grandkid"])],
            1,
            "not a child of root",
        ),
        (
            vec![budgeted("p", 0.1, &["nobody"])],
            1,
            "not a child of root",
        ),
        (vec![edge("kid", "theirs")], 1, "not a child of root"),
        (vec![edge("kid", "kid")], 1, "cannot depend on itself"),
        (
            vec![budgeted("p", 0.1, &[]), edge("kid", "p")],
            1,
            "already started",
        ),
        (
            // `kid` has settled, so all of root's $1.00 is left.
            vec![budgeted("p", 0.6, &[]), budgeted("q", 0.5, &["p"])],
            1,
            "exceeds what root has left",
        ),
        (
            vec![
                budgeted("p", 0.1, &[]),
                budgeted("q", 0.1, &[]),
                budgeted("r", 0.1, &[]),
                budgeted("s", 0.1, &[]),
                budgeted("t", 0.1, &[]),
            ],
            1,
            "max_children",
        ),
        (
            vec![budgeted("p", 0.1, &[]), budgeted("p", 0.1, &[])],
            1,
            "already exists",
        ),
        (
            vec![
                budgeted("p", 0.1, &[]),
                GraphEdit::RemoveDependency(DependencyRef {
                    dependent: "p".into(),
                    prerequisite: "kid".into(),
                }),
            ],
            1,
            "does not depend on",
        ),
        (
            vec![GraphEdit::Spawn(SpawnSpec {
                prompt: "x".into(),
                name: Some("p".into()),
                budget: Some(ChildBudget {
                    max_usd: Some(0.1),
                    ..ChildBudget::default()
                }),
                bindings: vec![Binding {
                    scratch: "nowhere".into(),
                    access: Access::ReadOnly,
                }],
                ..SpawnSpec::default()
            })],
            1,
            "does not exist",
        ),
        (Vec::new(), 1, "at least one edit"),
    ];
    for (edits, expected, needle) in refusals {
        let result = delegate.apply_graph(edits.clone(), expected);
        let message = match &result {
            Err(error) => error.to_string(),
            Ok(applied) => panic!("{needle}: applied {applied:?}"),
        };
        assert!(message.contains(needle), "{needle}: {message}");
        if needle == "stale" {
            assert_eq!(result.unwrap_err().kind(), "stale_revision");
        }
        let now = delegate.inspect("root").unwrap();
        assert_eq!(now.graph_revision, 1, "{needle}");
        assert_eq!(now.children, before.children, "{needle}");
        assert_eq!(now.remaining_usd, before.remaining_usd, "{needle}");
        assert!(f.yard.graph("root").unwrap().dependencies.is_empty());
        for name in ["p", "q", "r", "s", "t"] {
            assert!(f.yard.branch(name).is_err(), "{needle}: {name} exists");
            assert!(
                f.yard
                    .task("x")
                    .name(name)
                    .planned_names(&[])
                    .is_ok_and(|n| n == [name]),
                "{needle}: {name} is taken"
            );
        }
    }
    // The root's log records each refusal.
    let refused = root
        .events()
        .unwrap()
        .iter()
        .filter(|e| {
            matches!(&e.activity, branchyard::Activity::Delegation { tool, refused: true, .. }
                if tool == "apply_graph")
        })
        .count();
    assert_eq!(refused, 15);
    // The same proposal against the current revision commits.
    delegate
        .apply_graph(vec![budgeted("p", 0.4, &[]), budgeted("q", 0.4, &["p"])], 1)
        .unwrap();
    root.wait_subtree().unwrap();
    assert_eq!(status(&f.yard, "q"), BranchStatus::Ready);
}

/// Parent -> child -> grandchild, each level created by the level above it
/// while its own graph grows: no graph is declared up front.
#[test]
fn a_three_level_graph_is_built_at_runtime() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let root = root(&f, &options);
    let delegate = root.delegate(options.clone()).unwrap();
    let applied = delegate
        .apply_graph(vec![spawn("child", "say child", &[])], 0)
        .unwrap();
    assert_eq!(applied.spawned[0].depth, 1);
    root.wait_subtree().unwrap();
    let child = f.yard.branch("child").unwrap().delegate(options).unwrap();
    let applied = child
        .apply_graph(
            vec![
                spawn("grandchild", "WRITE g.txt=1", &[]),
                spawn("grandchild-2", "WRITE h.txt=1", &["grandchild"]),
            ],
            0,
        )
        .unwrap();
    assert_eq!(applied.spawned[0].depth, 2);
    root.wait_subtree().unwrap();
    assert_eq!(status(&f.yard, "grandchild-2"), BranchStatus::Ready);
    // Each level has its own graph and revision.
    assert_eq!(f.yard.graph("root").unwrap().revision, 1);
    assert_eq!(f.yard.graph("child").unwrap().revision, 1);
    assert_eq!(
        f.yard.graph("child").unwrap().dependencies[0].dependent,
        "grandchild-2"
    );
    // Dependencies join siblings: the root cannot tie its child to its
    // grandchild.
    denied(
        delegate.apply_graph(vec![spawn("late", "say", &["grandchild"])], 1),
        "not a child of root",
    );
}

#[test]
fn a_dependent_waits_to_be_integrated_and_starts_from_the_merge() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = root(&f, &options);
    let delegate = root.delegate(options).unwrap();
    delegate
        .apply_graph(
            vec![
                spawn("lib", "WRITE lib.txt=v1", &[]),
                GraphEdit::Spawn(SpawnSpec {
                    prompt: "WRITE app.txt=uses-lib".into(),
                    name: Some("app".into()),
                    depends_on: vec!["lib".into()],
                    after: After::Integrated,
                    ..SpawnSpec::default()
                }),
            ],
            0,
        )
        .unwrap();
    root.wait_subtree().unwrap();
    assert_eq!(status(&f.yard, "lib"), BranchStatus::Ready);
    assert_eq!(status(&f.yard, "app"), BranchStatus::Waiting);
    let merged = delegate.integrate("lib").unwrap();
    root.wait_subtree().unwrap();
    assert_eq!(status(&f.yard, "app"), BranchStatus::Ready);
    let app = f.yard.branch("app").unwrap().info().clone();
    assert_eq!(app.base, merged.commit);
    assert!(app.worktree.join("lib.txt").is_file());
}

/// A prerequisite settled and no engine started its dependent, as when the
/// engine that settled it stopped first: a later process starts it.
#[test]
fn a_dependent_no_engine_started_is_started_by_resume() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = root(&f, &options);
    let delegate = root.delegate(options.clone()).unwrap();
    delegate
        .apply_graph(
            vec![
                spawn("first", "WRITE f.txt=1", &[]),
                GraphEdit::Spawn(SpawnSpec {
                    prompt: "WRITE s.txt=1".into(),
                    name: Some("second".into()),
                    depends_on: vec!["first".into()],
                    after: After::Integrated,
                    ..SpawnSpec::default()
                }),
            ],
            0,
        )
        .unwrap();
    root.wait_subtree().unwrap();
    assert_eq!(status(&f.yard, "second"), BranchStatus::Waiting);
    // The state the stopped engine left: the prerequisite settled, the
    // dependency satisfied, the dependent never claimed.
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    db.execute(
        "UPDATE graph_edges SET after = 'settled' WHERE dependent = 'second'",
        [],
    )
    .unwrap();
    drop(db);
    // Another process: opening it recovers nothing and starts nothing, and
    // it does not know the options to start the dependent with.
    let yard = Yard::open(&f.root).unwrap();
    let root = yard.branch("root").unwrap();
    root.wait_subtree().unwrap();
    assert_eq!(status(&yard, "second"), BranchStatus::Waiting);
    assert_eq!(yard.resume_graph(&options).unwrap(), ["second"]);
    assert!(yard.resume_graph(&options).unwrap().is_empty());
    root.wait_subtree().unwrap();
    assert_eq!(status(&yard, "second"), BranchStatus::Ready);
}

/// The child process of the cross-process test: integrates `lib` into
/// `root` in its own yard, then waits for what that started.
#[test]
#[ignore = "the child process of a cross-process graph test"]
fn graph_child() {
    let (Some(root), Some(agent)) = (
        std::env::var_os("BY_GRAPH_ROOT"),
        std::env::var("BY_GRAPH_AGENT").ok(),
    ) else {
        return;
    };
    let yard = Yard::open(root).unwrap();
    let parent = yard.branch("root").unwrap();
    let options = TaskOptions {
        harness: Some("gemini-cli".into()),
        command: Some(vec![agent]),
        policy: Policy::allow_all(),
        ..TaskOptions::default()
    };
    parent.delegate(options).unwrap().integrate("lib").unwrap();
    parent.wait_subtree().unwrap();
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

#[test]
fn a_dependent_starts_in_the_process_that_settles_its_prerequisite() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = root(&f, &options);
    root.delegate(options)
        .unwrap()
        .apply_graph(
            vec![
                spawn("lib", "WRITE lib.txt=v1", &[]),
                GraphEdit::Spawn(SpawnSpec {
                    prompt: "WRITE app.txt=1".into(),
                    name: Some("app".into()),
                    depends_on: vec!["lib".into()],
                    after: After::Integrated,
                    ..SpawnSpec::default()
                }),
            ],
            0,
        )
        .unwrap();
    root.wait_subtree().unwrap();
    assert_eq!(status(&f.yard, "app"), BranchStatus::Waiting);
    let mut child = Killed(
        Command::new(std::env::current_exe().unwrap())
            .args(["graph_child", "--exact", "--ignored", "--test-threads=1"])
            .env("BY_GRAPH_ROOT", &f.root)
            .env("BY_GRAPH_AGENT", fake_agent())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = child.0.id();
    assert!(child.0.wait().unwrap().success());
    assert_eq!(status(&f.yard, "app"), BranchStatus::Ready);
    // The other process ran app's turn: it held app's lease.
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    let holder: i64 = db
        .query_row("SELECT pid FROM leases WHERE branch = 'app'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(holder, i64::from(pid));
    assert!(f
        .yard
        .branch("app")
        .unwrap()
        .info()
        .worktree
        .join("lib.txt")
        .is_file());
}

#[test]
fn bindings_take_a_scratch_areas_writer_lock_for_each_turn() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = root(&f, &options);
    f.yard.create_scratch("root", "notes").unwrap();
    f.yard
        .task("say elsewhere")
        .options(options.clone())
        .name("elsewhere")
        .run()
        .unwrap();
    f.yard.create_scratch("elsewhere", "theirs").unwrap();
    let delegate = root.delegate(options).unwrap();
    let bound = |name: &str, prompt: &str, scratch: &str, access: Access| {
        GraphEdit::Spawn(SpawnSpec {
            prompt: prompt.into(),
            name: Some(name.into()),
            bindings: vec![Binding {
                scratch: scratch.into(),
                access,
            }],
            ..SpawnSpec::default()
        })
    };
    // Refused up front: an area the child could not read, or none at all.
    denied(
        delegate.apply_graph(vec![bound("x", "say", "theirs", Access::ReadOnly)], 0),
        "belongs to elsewhere",
    );
    denied(
        delegate.apply_graph(vec![bound("x", "say", "missing", Access::ReadOnly)], 0),
        "does not exist",
    );
    delegate
        .apply_graph(
            vec![
                bound("writer", "HANG", "notes", Access::ExclusiveWrite),
                bound("reader", "say read", "notes", Access::ReadOnly),
            ],
            0,
        )
        .unwrap();
    wait::until("writer to hold the lock", || {
        f.yard
            .scratch_lock_state("notes")
            .unwrap()
            .is_some_and(|l| l.holder_branch == "writer")
    });
    // Another exclusive writer's turn cannot take it while writer's runs.
    delegate
        .apply_graph(
            vec![bound("second", "say w", "notes", Access::ExclusiveWrite)],
            1,
        )
        .unwrap();
    wait::until("second to fail", || {
        matches!(status(&f.yard, "second"), BranchStatus::Failed { .. })
    });
    match status(&f.yard, "second") {
        BranchStatus::Failed { reason } => {
            assert!(reason.contains("exclusive_write binding"), "{reason}");
            assert!(reason.contains("held by writer"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    delegate.cancel("writer").unwrap();
    root.wait_subtree().unwrap();
    // Released when writer's turn ended; a read-only binding took nothing.
    assert!(f.yard.scratch_lock_state("notes").unwrap().is_none());
    assert_eq!(status(&f.yard, "reader"), BranchStatus::NoChanges);
    let inspected = delegate.inspect("writer").unwrap();
    assert_eq!(inspected.bindings[0].access, Access::ExclusiveWrite);
    // Sending second again takes the free lock.
    delegate.send("second", "say again").unwrap();
    root.wait_subtree().unwrap();
    assert!(!matches!(
        status(&f.yard, "second"),
        BranchStatus::Failed { .. }
    ));
}

/// The typed method and the tool every other surface calls give the same
/// JSON.
#[test]
fn the_apply_graph_tool_matches_the_typed_call() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = root(&f, &options);
    let delegate = root.delegate(options).unwrap();
    let edits = serde_json::json!([
        {"kind": "spawn", "prompt": "WRITE a.txt=1", "name": "a"},
        {"kind": "spawn", "prompt": "WRITE b.txt=1", "name": "b", "depends_on": ["a"]},
    ]);
    let stale = delegate.call(
        "apply_graph",
        serde_json::json!({"edits": edits, "expected_revision": 7}),
    );
    assert!(matches!(
        stale,
        Err(Error::StaleRevision {
            expected: 7,
            actual: 0,
            ..
        })
    ));
    let applied = delegate
        .call(
            "apply_graph",
            serde_json::json!({"edits": edits, "expected_revision": 0}),
        )
        .unwrap();
    assert_eq!(applied["revision"], 1);
    assert_eq!(
        applied["spawned"][1]["status"],
        serde_json::json!({"state": "waiting"})
    );
    assert_eq!(applied["dependencies"][0]["dependent"], "b");
    root.wait_subtree().unwrap();
    let shown = delegate.call("graph", serde_json::json!({})).unwrap();
    assert_eq!(
        shown,
        serde_json::to_value(delegate.graph("root").unwrap()).unwrap()
    );
    assert_eq!(shown["children"][1]["depends_on"], serde_json::json!(["a"]));
    assert!(delegate
        .call("apply_graph", serde_json::json!({"edits": [], "bogus": 1}))
        .is_err());
}

/// The child process of the crash test: applies a graph whose prerequisite
/// hangs, then waits, until it is killed.
#[test]
#[ignore = "the child process of a graph crash test"]
fn graph_crash_child() {
    let (Some(root), Some(agent)) = (
        std::env::var_os("BY_GRAPH_ROOT"),
        std::env::var("BY_GRAPH_AGENT").ok(),
    ) else {
        return;
    };
    let yard = Yard::open(root).unwrap();
    let options = TaskOptions {
        harness: Some("gemini-cli".into()),
        command: Some(vec![agent.clone()]),
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec![agent]),
        policy: Policy::allow_all(),
        ..TaskOptions::default()
    };
    let root = yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    root.delegate(options)
        .unwrap()
        .apply_graph(
            vec![
                spawn("stuck", "HANG", &[]),
                spawn("after", "WRITE a.txt=1", &["stuck"]),
            ],
            0,
        )
        .unwrap();
    root.wait_subtree().unwrap();
}

#[test]
fn a_prerequisite_whose_engine_was_killed_blocks_its_dependent_on_recovery() {
    let f = Fixture::new();
    let mut child = Killed(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "graph_crash_child",
                "--exact",
                "--ignored",
                "--test-threads=1",
            ])
            .env("BY_GRAPH_ROOT", &f.root)
            .env("BY_GRAPH_AGENT", fake_agent())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait::until("the prerequisite to run and its dependent to wait", || {
        let yard = Yard::open(&f.root).unwrap();
        // Its prompt is recorded just before it is submitted.
        yard.branch("stuck").is_ok_and(|b| {
            b.events()
                .unwrap_or_default()
                .iter()
                .any(|e| matches!(e.activity, branchyard::Activity::Prompt(_)))
        }) && yard
            .branch("after")
            .is_ok_and(|b| b.info().status == BranchStatus::Waiting)
    });
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    // Opening the repository recovers the prerequisite, and that blocks
    // the dependent, which never ran.
    let yard = Yard::open(&f.root).unwrap();
    assert_eq!(status(&yard, "stuck"), BranchStatus::Interrupted);
    match status(&yard, "after") {
        BranchStatus::Blocked { reason } => {
            assert!(reason.contains("stuck was interrupted"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(yard.branch("after").unwrap().info().turns, 0);
    assert!(yard
        .resume_graph(&TaskOptions::default())
        .unwrap()
        .is_empty());
}
