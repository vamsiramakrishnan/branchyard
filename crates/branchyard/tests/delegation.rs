//! Delegation through the SDK against the fake ACP agent: children,
//! envelopes, budgets, authority and cancellation. The MCP server and a
//! harness calling it are tested end to end in `branchyard-mcp`.

mod common;

use std::fs;
use std::time::{Duration, Instant};

use branchyard::{
    Activity, BranchStatus, Budget, Delegate, Envelope, Error, Policy, Spawn, TaskOptions,
};
use common::{fake_agent, Fixture};

/// Root options that may delegate. The MCP server is never started here,
/// since no prompt asks the agent to, so any executable stands in for it.
fn delegating(f: &Fixture, envelope: Envelope) -> TaskOptions {
    TaskOptions {
        delegation: Some(envelope),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    }
}

fn spawn(prompt: &str, name: &str) -> Spawn {
    Spawn {
        prompt: prompt.into(),
        name: Some(name.into()),
        ..Spawn::default()
    }
}

fn denied(result: Result<impl std::fmt::Debug, Error>, needle: &str) {
    match result {
        Err(Error::Denied(why)) if why.contains(needle) => {}
        other => panic!("expected a denial mentioning {needle:?}, got {other:?}"),
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn children_run_in_parallel_threads_from_the_parents_current_work() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = f
        .yard
        .task("WRITE root.txt=r")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    // Uncommitted work in the parent's worktree is committed before the
    // child starts from it.
    fs::write(root.info().worktree.join("pending.txt"), "p\n").unwrap();
    let delegate = root.delegate(options).unwrap();
    assert_eq!(delegate.branch(), "root");
    let kid = delegate
        .spawn(spawn("HANG", "kid"))
        .expect("a child within the envelope");
    assert_eq!(kid.status, BranchStatus::Running);
    assert_eq!(kid.depth, 1);
    assert_eq!(kid.base, f.git(&["rev-parse", "by/root"]).trim());
    let kid_info = f.yard.branch("kid").unwrap().info().clone();
    assert_eq!(kid_info.parent.as_deref(), Some("root"));
    assert!(kid_info.worktree.join("pending.txt").is_file());
    assert!(kid_info.worktree.join("root.txt").is_file());
    // spawn returned while the child's turn is still going.
    let other = delegate.spawn(spawn("WRITE other.txt=o", "other")).unwrap();
    assert_eq!(other.status, BranchStatus::Running);
    let root_info = f.yard.branch("root").unwrap().info().clone();
    assert_eq!(root_info.children, ["kid", "other"]);

    assert_eq!(delegate.cancel("kid").unwrap().cancelled, ["kid"]);
    let finished = root.wait_subtree().unwrap();
    let names: Vec<&str> = finished.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["kid", "other"]);
    assert_eq!(finished[0].status, BranchStatus::Interrupted);
    assert_eq!(finished[1].status, BranchStatus::Ready);
    let kid_log = f.yard.branch("kid").unwrap().events().unwrap();
    assert!(kid_log
        .iter()
        .any(|e| e.activity == Activity::Warning("cancelled by root".into())));
    // Nothing is left to cancel.
    assert!(delegate.cancel("kid").unwrap().cancelled.is_empty());
}

#[test]
fn a_parent_integrates_a_child_into_its_own_branch_only() {
    let f = Fixture::new();
    let options = TaskOptions {
        check: Some(vec!["test".into(), "-f".into(), "child.txt".into()]),
        ..delegating(&f, Envelope::default())
    };
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let main_before = f.git(&["rev-parse", "main"]);
    let delegate = root.delegate(options).unwrap();
    delegate
        .spawn(spawn("WRITE child.txt=from-kid", "kid"))
        .unwrap();
    root.wait_subtree().unwrap();
    let merged = delegate.integrate("kid").unwrap();
    assert_eq!(merged.target, "by/root");
    assert_eq!(f.git(&["rev-parse", "by/root"]).trim(), merged.commit);
    assert_eq!(
        f.git(&["rev-parse", "main"]),
        main_before,
        "main is untouched"
    );
    // The parent's worktree moved to the merge.
    let worktree = &root.info().worktree;
    assert_eq!(
        fs::read_to_string(worktree.join("child.txt")).unwrap(),
        "from-kid\n"
    );
    assert_eq!(
        f.yard.branch("kid").unwrap().info().status,
        BranchStatus::Merged {
            target: "by/root".into(),
            commit: merged.commit.clone()
        }
    );
    // The parent's next turn picks the merge up as its candidate.
    let root = root.send("say done", f.options()).unwrap();
    assert_eq!(root.info().status, BranchStatus::Ready);
    assert_eq!(
        root.info().candidate.as_ref().unwrap().commit,
        merged.commit
    );
}

#[test]
fn the_envelope_bounds_depth_width_and_harnesses() {
    let f = Fixture::new();
    let options = delegating(
        &f,
        Envelope {
            max_depth: 2,
            max_children: 2,
            harnesses: Vec::new(),
        },
    );
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    // Only the parent's own profile unless the envelope lists others.
    denied(
        delegate.spawn(Spawn {
            harness: Some("qwen-code".into()),
            ..spawn("x", "q")
        }),
        "may not delegate to qwen-code-acp",
    );
    // A child cannot be allowed what its parent is not.
    denied(
        delegate.spawn(Spawn {
            harnesses: Some(vec!["codex".into()]),
            ..spawn("x", "c")
        }),
        "may not be allowed codex",
    );
    delegate.spawn(spawn("say a", "a")).unwrap();
    delegate
        .spawn(Spawn {
            max_depth: Some(0),
            ..spawn("say b", "b")
        })
        .unwrap();
    denied(delegate.spawn(spawn("say c", "c")), "max_children");
    root.wait_subtree().unwrap();

    // `a` is one level down and may create one more level; `b` gave that up.
    let a = f
        .yard
        .branch("a")
        .unwrap()
        .delegate(options.clone())
        .unwrap();
    let grandchild = a.spawn(spawn("say g", "g")).unwrap();
    assert_eq!(grandchild.depth, 2);
    root.wait_subtree().unwrap();
    let g = f
        .yard
        .branch("g")
        .unwrap()
        .delegate(options.clone())
        .unwrap();
    denied(g.spawn(spawn("say gg", "gg")), "max_depth is 0");
    let b = f
        .yard
        .branch("b")
        .unwrap()
        .delegate(options.clone())
        .unwrap();
    denied(b.spawn(spawn("say bb", "bb")), "max_depth is 0");
    // The root sees the whole tree.
    let tree: Vec<String> = root
        .descendants()
        .unwrap()
        .into_iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(tree, ["a", "b", "g"]);
    // A branch that was never given delegation can look at itself but not
    // create children.
    let plain = f.task("say plain").name("plain").run().unwrap();
    let plain = plain.delegate(options).unwrap();
    assert_eq!(plain.inspect("plain").unwrap().envelope, None);
    denied(plain.spawn(spawn("say p", "p")), "not given delegation");
}

#[test]
fn a_branch_acts_only_on_its_descendants() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let sibling = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("sibling")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    delegate.spawn(spawn("say kid", "kid")).unwrap();
    sibling
        .delegate(options.clone())
        .unwrap()
        .spawn(spawn("say theirs", "theirs"))
        .unwrap();
    root.wait_subtree().unwrap();
    sibling.wait_subtree().unwrap();

    // Read-only tools accept the branch itself; nothing else outside the
    // subtree is reachable.
    assert_eq!(delegate.inspect("root").unwrap().children, ["kid"]);
    for target in ["sibling", "theirs", "main", "nope"] {
        denied(delegate.inspect(target), "not a descendant");
        denied(delegate.events(target, None, 10), "not a descendant");
        denied(delegate.send(target, "hi"), "not a descendant");
        denied(delegate.integrate(target), "not a descendant");
        denied(delegate.cancel(target), "not a descendant");
    }
    denied(delegate.cancel("root"), "only on its descendants");
    assert_eq!(
        delegate
            .children()
            .unwrap()
            .descendants
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        ["kid"]
    );
    // A child cannot reach its parent or its sibling's subtree.
    let kid = f.yard.branch("kid").unwrap().delegate(options).unwrap();
    denied(kid.inspect("root"), "not a descendant");
    denied(kid.send("theirs", "hi"), "not a descendant");

    // Events come with a cursor.
    let page = delegate.events("kid", Some(0), 2).unwrap();
    assert_eq!((page.events.len(), page.next_cursor), (2, 2));
    let rest = delegate.events("kid", Some(page.next_cursor), 200).unwrap();
    assert_eq!(rest.next_cursor, rest.total);
    let recent = delegate.events("kid", None, 1).unwrap();
    assert_eq!(recent.next_cursor, recent.total);
    assert_eq!(
        delegate.inspect("kid").unwrap().last_message,
        "echo: say kid"
    );
}

#[test]
fn child_budgets_fit_in_what_the_parent_has_left() {
    let f = Fixture::new();
    let options = TaskOptions {
        budget: Budget::usd(1.0).turns(3),
        ..delegating(&f, Envelope::default())
    };
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    denied(
        delegate.spawn(spawn("HANG", "a")),
        "needs max_usd; $1.0000 remains",
    );
    let with = |usd: f64, name: &str| Spawn {
        budget: Budget::usd(usd),
        ..spawn("HANG", name)
    };
    delegate
        .spawn(Spawn {
            budget: Budget::usd(0.6).turns(1),
            ..spawn("HANG", "a")
        })
        .unwrap();
    denied(
        delegate.spawn(with(0.5, "b")),
        "exceeds what root has left, $0.4000",
    );
    denied(
        delegate.spawn(Spawn {
            budget: Budget::usd(0.1).turns(4),
            ..spawn("HANG", "b")
        }),
        "max_turns 4 exceeds root's 3",
    );
    let b = delegate.spawn(with(0.4, "b")).unwrap();
    assert_eq!(b.status, BranchStatus::Running);
    let me = delegate.inspect("root").unwrap();
    assert_eq!(me.max_usd, Some(1.0));
    assert!(me.remaining_usd.unwrap().abs() < 1e-9, "{me:?}");
    let a = delegate.inspect("a").unwrap();
    assert_eq!((a.max_usd, a.remaining_usd), (Some(0.6), Some(0.6)));

    // Reserving the whole budget leaves the parent no room for its own
    // turns: children's reservations count against it.
    delegate.cancel("a").unwrap();
    delegate.cancel("b").unwrap();
    root.wait_subtree().unwrap();
    let root = root.send("say more", options).unwrap();
    assert_eq!(
        root.info().status,
        BranchStatus::BudgetExceeded {
            limit: "max_usd".into()
        }
    );
    // A child's limits bound its turns whoever sends them.
    let a = f.yard.branch("a").unwrap();
    let a = a.send("say", f.options()).unwrap();
    assert_eq!(
        a.info().status,
        BranchStatus::BudgetExceeded {
            limit: "max_turns".into()
        }
    );
}

#[test]
fn tokens_are_issued_per_turn_and_forged_ones_are_refused() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let yard = f.yard.clone();
    let running = std::thread::spawn(move || {
        yard.task("HANG")
            .options(options)
            .name("root")
            .run()
            .unwrap()
    });
    let token_file = f.root.join(".branchyard/delegation/root.json");
    wait_until("the token file", || token_file.is_file());
    let file: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&token_file).unwrap()).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(&token_file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let token = file["token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 64);

    denied(f.yard.as_branch("forged").map(|_| ()), "no running turn");
    // In the engine's process the token finds the running turn directly...
    let delegate = f.yard.as_branch(&token).unwrap();
    assert_eq!(delegate.branch(), "root");
    assert_eq!(
        delegate.inspect("root").unwrap().status,
        BranchStatus::Running
    );
    let kid = delegate.spawn(spawn("say kid", "kid")).unwrap();
    assert_eq!(kid.depth, 1);
    // ...and from anywhere else, through the broker, with the same answers.
    let remote = Delegate::connect(&f.root, &token).unwrap();
    assert_eq!(remote.branch(), "root");
    root_waits_for(&f, "kid");
    assert_eq!(
        remote.inspect("kid").unwrap(),
        delegate.inspect("kid").unwrap()
    );
    assert_eq!(remote.children().unwrap(), delegate.children().unwrap());
    let refusal = remote.inspect("main").unwrap_err();
    assert_eq!(refusal.kind(), "denied");
    assert!(
        refusal.to_string().contains("not a descendant"),
        "{refusal}"
    );
    let value = remote.call("inspect", serde_json::json!({})).unwrap();
    assert_eq!(value["name"], "root");
    match Delegate::connect(&f.root, "forged") {
        Err(Error::Denied(why)) => assert!(why.contains("no running turn"), "{why}"),
        other => panic!("{other:?}"),
    }

    // Ending the turn revokes the token.
    let root = f.yard.branch("root").unwrap();
    assert!(root.cancel().unwrap().contains(&"root".to_owned()));
    let root_after = running.join().unwrap();
    assert_eq!(root_after.info().status, BranchStatus::Interrupted);
    root.wait_subtree().unwrap();
    assert!(!token_file.exists());
    assert!(
        fs::read_dir(f.root.join(".branchyard/delegation"))
            .unwrap()
            .all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".sock")),
        "the broker's socket is removed"
    );
    denied(f.yard.as_branch(&token).map(|_| ()), "no running turn");
    let stale = remote.inspect("root").unwrap_err();
    assert!(matches!(stale.kind(), "denied" | "state"), "{stale:?}");
}

fn root_waits_for(f: &Fixture, name: &str) {
    wait_until(name, || {
        f.yard.branch(name).unwrap().info().status != BranchStatus::Running
    });
}

#[test]
fn delegation_needs_a_server_before_anything_is_created() {
    let f = Fixture::new();
    let options = TaskOptions {
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec!["/nonexistent/branchyard-mcp".into()]),
        ..f.options()
    };
    let result = f.yard.task("say hi").options(options).name("x").run();
    assert!(
        matches!(&result, Err(Error::Unsupported(why)) if why.contains("MCP server")),
        "{result:?}"
    );
    assert!(f.yard.branches().unwrap().is_empty());
}
