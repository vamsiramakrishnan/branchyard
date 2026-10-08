//! Delegation through the SDK against the fake ACP agent: children,
//! envelopes, budgets, authority and cancellation. The MCP server and a
//! harness calling it are tested end to end in `branchyard-mcp`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::time::{Duration, Instant};

use branchyard::{
    Activity, BranchStatus, Budget, ChildBudget, Delegate, Envelope, Error, Policy, Provisioning,
    Seat, Seats, SecretSource, Spawn, SteerState, TaskOptions, Yard,
};
use branchyard_testkit::wait;
use common::{edit_record, fake_agent, Fixture};

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
            ..Envelope::default()
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
    // `max_children` bounds the children running at once.
    delegate.spawn(spawn("AWAIT_STEER", "a")).unwrap();
    delegate
        .spawn(Spawn {
            max_depth: Some(0),
            ..spawn("AWAIT_STEER", "b")
        })
        .unwrap();
    denied(delegate.spawn(spawn("say c", "c")), "max_children");
    for kid in ["a", "b"] {
        wait::until("the child to wait for steering", || {
            delegate
                .inspect(kid)
                .is_ok_and(|i| i.last_message.contains("waiting for steering"))
        });
        delegate.steer(kid, "that is all").unwrap();
    }
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

/// A parent adds input to its running child's turn: delivered by the
/// engine running the child, recorded with the parent as its sender, and
/// the child's turn goes on to end once. Only descendants can be steered.
#[test]
fn a_parent_steers_its_running_child() {
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
    delegate.spawn(spawn("AWAIT_STEER", "kid")).unwrap();
    wait::until("the child to wait for steering", || {
        delegate
            .inspect("kid")
            .is_ok_and(|i| i.last_message.contains("waiting for steering"))
    });
    // Through the tool as every surface calls it.
    let steered = delegate
        .call(
            "steer",
            serde_json::json!({"branch": "kid", "text": "use the fast path"}),
        )
        .unwrap();
    let state: SteerState = serde_json::from_value(steered["state"].clone()).unwrap();
    assert!(
        matches!(state, SteerState::Written | SteerState::Accepted),
        "{steered}"
    );
    assert_eq!(steered["by"], "root");
    let finished = root.wait_subtree().unwrap();
    assert_eq!(finished[0].status, BranchStatus::NoChanges);
    let log = f.yard.branch("kid").unwrap().events().unwrap();
    assert!(log.iter().any(|e| matches!(&e.activity,
        Activity::Steered { by, text, .. } if by == "root" && text == "use the fast path")));
    assert!(delegate
        .inspect("kid")
        .unwrap()
        .last_message
        .contains("steered: use the fast path"));
    // Recorded on the asking branch, like every delegation operation.
    let root_log = f.yard.branch("root").unwrap().events().unwrap();
    assert!(root_log.iter().any(|e| matches!(&e.activity,
        Activity::Delegation { tool, branch, refused: false, .. } if tool == "steer" && branch == "kid")));
    // With no turn running, and outside the subtree, it is refused.
    assert!(matches!(
        delegate.steer("kid", "again"),
        Err(Error::NotRunning(name)) if name == "kid"
    ));
    denied(delegate.steer("root", "hi"), "only on its descendants");
    denied(delegate.steer("main", "hi"), "not a descendant");
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
        "needs one too, max_usd (--budget-usd); $1.0000 remains",
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
        "max_turns (--max-turns) 4 exceeds root's 3",
    );
    let b = delegate.spawn(with(0.4, "b")).unwrap();
    assert_eq!(b.status, BranchStatus::Running);
    let me = delegate.inspect("root").unwrap();
    assert_eq!(me.max_usd, Some(1.0));
    assert!(me.remaining_usd.unwrap().abs() < 1e-9, "{me:?}");
    let a = delegate.inspect("a").unwrap();
    assert_eq!((a.max_usd, a.remaining_usd), (Some(0.6), Some(0.6)));

    // Reserving the whole budget leaves the parent no room for its own
    // turns: running children's reservations count against it.
    let exceeded = root.send("say more", options.clone()).unwrap();
    assert_eq!(
        exceeded.info().status,
        BranchStatus::BudgetExceeded {
            limit: "max_usd".into()
        }
    );
    // Cancelled, they hold only what they spent, and the parent runs.
    delegate.cancel("a").unwrap();
    delegate.cancel("b").unwrap();
    root.wait_subtree().unwrap();
    let root = root.send("say more", options).unwrap();
    assert_eq!(root.info().status, BranchStatus::NoChanges);
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

/// A child holds its whole limit only while it can run turns without
/// asking: settled, merged or removed, it holds what it spent, and
/// `max_children` counts only live children. Sending a settled child more
/// work holds its limit again, narrowed to what is left.
#[test]
fn settled_children_give_back_what_they_did_not_spend() {
    let f = Fixture::new();
    let options = TaskOptions {
        budget: Budget::usd(1.5),
        ..delegating(
            &f,
            Envelope {
                max_children: 2,
                ..Envelope::default()
            },
        )
    };
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    let with = |usd: f64, prompt: &str, name: &str| Spawn {
        budget: Budget::usd(usd),
        ..spawn(prompt, name)
    };
    delegate.spawn(with(0.6, "WRITE a.txt=a", "a")).unwrap();
    delegate.spawn(with(0.6, "say b", "b")).unwrap();
    root.wait_subtree().unwrap();
    // The children spent $0.25 and nothing; they are settled.
    edit_record(&f.root, "a", |r| r["info"]["cost_usd"] = 0.25.into());
    let me = delegate.inspect("root").unwrap();
    assert!((me.remaining_usd.unwrap() - 1.25).abs() < 1e-9, "{me:?}");
    assert_eq!((me.reserved_usd, me.reserving_children), (0.0, 0));
    assert!((me.settled_children_usd - 0.25).abs() < 1e-9, "{me:?}");
    assert!((me.subtree_cost_usd - 0.25).abs() < 1e-9, "{me:?}");

    // Two children exist, but neither is live: there is room for two more.
    delegate.spawn(with(0.6, "AWAIT_STEER", "c")).unwrap();
    delegate.spawn(with(0.6, "HANG", "d")).unwrap();
    denied(delegate.spawn(with(0.01, "say e", "e")), "2 live children");
    let me = delegate.inspect("root").unwrap();
    assert_eq!(me.reserving_children, 2, "{me:?}");
    assert!((me.reserved_usd - 1.2).abs() < 1e-9, "{me:?}");
    wait::until("c to wait for steering", || {
        delegate
            .inspect("c")
            .is_ok_and(|i| i.last_message.contains("waiting for steering"))
    });
    delegate.steer("c", "finish").unwrap();

    // Merged, then removed, a child still counts what it spent.
    delegate.integrate("a").unwrap();
    f.yard.remove("a").unwrap();
    f.yard.remove("b").unwrap();
    let me = delegate.inspect("root").unwrap();
    assert!((me.subtree_cost_usd - 0.25).abs() < 1e-9, "{me:?}");
    assert!((me.remaining_usd.unwrap() - (1.5 - 0.25 - 0.6)).abs() < 1e-9);

    // c, settled, is sent more work while d holds $0.60: its $0.60 limit
    // no longer fits in the $0.65 left once it runs beside a $0.60 child,
    // so it is narrowed.
    wait::until("c to settle", || {
        delegate
            .inspect("c")
            .is_ok_and(|c| c.status == BranchStatus::NoChanges)
    });
    edit_record(&f.root, "root", |r| r["info"]["cost_usd"] = 0.5.into());
    // Left: 1.5 - 0.5 (root) - 0.25 (a) - 0.6 (d) = 0.15.
    delegate.send("c", "say again").unwrap();
    let c = wait::until("c to settle again", || {
        delegate
            .inspect("c")
            .ok()
            .filter(|c| c.turns == 2 && c.status != BranchStatus::Running)
    });
    assert!((c.max_usd.unwrap() - 0.15).abs() < 1e-9, "{c:?}");
    let log = f.yard.branch("c").unwrap().events().unwrap();
    assert!(log.iter().any(|e| matches!(&e.activity,
        Activity::Warning(w) if w.contains("narrowed from $0.6000 to $0.1500"))));
    delegate.cancel("d").unwrap();
    root.wait_subtree().unwrap();
    // With nothing left, a settled child is not sent more.
    edit_record(&f.root, "root", |r| r["info"]["cost_usd"] = 1.25.into());
    denied(delegate.send("c", "and again"), "nothing left");
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
    wait::until("the token file", || token_file.is_file());
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
    wait::until(name, || {
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

/// Leave `name` as an engine that died mid-turn would: its record says
/// running, its lease is held by a process that has exited, and the turn's
/// end was never recorded.
fn die_mid_turn(f: &Fixture, name: &str) {
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    edit_record(&f.root, name, |record| {
        record["info"]["status"] = serde_json::json!({"state": "running"});
    });
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    let held = db
        .execute(
            "UPDATE leases SET owner = 'gone', pid = ?2, expires_ms = 9999999999999 \
             WHERE branch = ?1",
            rusqlite::params![name, dead],
        )
        .unwrap();
    assert_eq!(held, 1);
    db.execute(
        "DELETE FROM steps WHERE branch = ?1 AND step IN ('turn_end', 'snapshot')",
        [name],
    )
    .unwrap();
}

fn recovered(f: &Fixture, name: &str) -> Vec<String> {
    f.yard
        .branch(name)
        .unwrap()
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Recovered { reason, .. } => Some(reason),
            _ => None,
        })
        .collect()
}

#[test]
fn a_subtree_driven_by_another_engine_is_waited_for_and_a_stopped_ones_recovered() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = f
        .yard
        .task("WRITE root.txt=r")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    // The child runs on a thread of the fixture's yard; another yard,
    // standing in for another process, has no thread to join.
    let delegate = root.delegate(options.clone()).unwrap();
    delegate.spawn(spawn("SH sleep 1", "slow")).unwrap();
    let other = Yard::open(&f.root).unwrap();
    let started = Instant::now();
    let waited = other.branch("root").unwrap().wait_subtree().unwrap();
    assert!(started.elapsed() >= Duration::from_millis(500), "it waited");
    assert_eq!(waited.len(), 1);
    assert_eq!(waited[0].status, BranchStatus::NoChanges);
    root.wait_subtree().unwrap();

    // A child whose engine stopped is recovered by the wait, not waited
    // for forever: through the subtree wait and through a delegate's.
    die_mid_turn(&f, "slow");
    let waited = root.wait_subtree().unwrap();
    assert_eq!(waited[0].status, BranchStatus::Interrupted);
    let reasons = recovered(&f, "slow");
    assert_eq!(reasons.len(), 1);
    assert!(
        reasons[0].contains("is no longer running"),
        "{}",
        reasons[0]
    );

    die_mid_turn(&f, "slow");
    let other_delegate = other.branch("root").unwrap().delegate(options).unwrap();
    let inspection = other_delegate
        .wait("slow", Duration::from_secs(30))
        .unwrap();
    assert_eq!(inspection.status, BranchStatus::Interrupted);
    assert_eq!(recovered(&f, "slow").len(), 2);

    // A running record no engine holds a lease for is settled too.
    edit_record(&f.root, "slow", |record| {
        record["info"]["status"] = serde_json::json!({"state": "running"});
    });
    let waited = other.branch("root").unwrap().wait_subtree().unwrap();
    assert_eq!(waited[0].status, BranchStatus::Interrupted);
    let reasons = recovered(&f, "slow");
    assert!(
        reasons[2].contains("no engine holds its lease"),
        "{}",
        reasons[2]
    );
}

fn seat(below: &[&str]) -> Seat {
    Seat {
        harness: "gemini-cli".into(),
        budget: ChildBudget::default(),
        check: None,
        deny: Vec::new(),
        isolated: false,
        provision: None,
        delegates_to: below.iter().map(|s| (*s).to_owned()).collect(),
        escalates_to: Vec::new(),
        instances: 1,
        bindings: Vec::new(),
    }
}

/// lead -> worker (twice, 3 turns, instructed), lead -> planner -> helper.
fn team() -> Seats {
    let worker = Seat {
        budget: ChildBudget {
            max_turns: Some(3),
            ..ChildBudget::default()
        },
        deny: vec!["Bash".into()],
        provision: Some(Provisioning {
            instructions: Some("You are the worker.".into()),
            ..Provisioning::default()
        }),
        instances: 2,
        ..seat(&[])
    };
    Seats {
        rig: "team".into(),
        seat: "lead".into(),
        delegates_to: vec!["worker".into(), "planner".into()],
        escalates_to: Vec::new(),
        table: [
            ("worker".to_owned(), worker),
            ("planner".to_owned(), seat(&["helper"])),
            ("helper".to_owned(), seat(&[])),
        ]
        .into_iter()
        .collect(),
    }
}

fn by_seat(seat: &str, prompt: &str) -> Spawn {
    Spawn {
        seat: Some(seat.into()),
        ..Spawn::new(prompt)
    }
}

#[test]
fn a_rigs_branches_spawn_only_the_seats_below_their_own() {
    let f = Fixture::new();
    let seats = team();
    let envelope = seats.envelope();
    assert_eq!((envelope.max_depth, envelope.max_children), (2, 3));
    let options = TaskOptions {
        seats: Some(seats),
        ..delegating(&f, envelope)
    };
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let lead = root.delegate(options.clone()).unwrap();
    let me = lead.inspect("root").unwrap();
    assert_eq!(me.seat.as_deref(), Some("lead"));
    assert_eq!(me.seats, ["worker", "planner"]);

    denied(lead.spawn(spawn("x", "free")), "spawns only by seat");
    denied(
        lead.spawn(by_seat("helper", "x")),
        "may spawn only worker, planner, not helper",
    );
    denied(
        lead.spawn(Spawn {
            harness: Some("qwen-code".into()),
            ..by_seat("worker", "x")
        }),
        "fixes the child's harness",
    );
    denied(
        lead.spawn(Spawn {
            check: Some(vec!["true".into()]),
            ..by_seat("worker", "x")
        }),
        "fixes the child's check",
    );
    denied(
        lead.spawn(Spawn {
            budget: Budget::default().turns(5),
            ..by_seat("worker", "x")
        }),
        "exceeds seat worker's",
    );

    // The seat fills the child: its name, limits and instructions.
    let first = lead.spawn(by_seat("worker", "INSTRUCTED")).unwrap();
    assert_eq!(first.name, "root-worker");
    assert_eq!(first.seat.as_deref(), Some("worker"));
    assert_eq!(first.budget.max_turns, Some(3));
    let second = lead
        .spawn(Spawn {
            budget: Budget::default().turns(2),
            ..by_seat("worker", "say again")
        })
        .unwrap();
    assert_eq!(second.name, "root-worker-2");
    assert_eq!(second.budget.max_turns, Some(2));
    denied(lead.spawn(by_seat("worker", "x")), "the seat's instances");
    let planner = lead.spawn(by_seat("planner", "say plan")).unwrap();
    root.wait_subtree().unwrap();

    let worker = lead.inspect("root-worker").unwrap();
    assert_eq!(worker.seat.as_deref(), Some("worker"));
    assert!(worker.seats.is_empty());
    assert_eq!(worker.envelope.as_ref().unwrap().max_depth, 0);
    // A leaf seat gets no delegation skill: its instructions are its seat's.
    assert_eq!(worker.last_message, "instructed=true");
    let plan = lead.inspect(&planner.name).unwrap();
    assert_eq!(plan.seats, ["helper"]);
    assert_eq!(
        plan.envelope,
        Some(Envelope {
            max_depth: 1,
            max_children: 1,
            harnesses: vec!["gemini-cli".into()],
            ..Envelope::default()
        })
    );

    // One level down, only that seat's own seats.
    let planner = f
        .yard
        .branch(&planner.name)
        .unwrap()
        .delegate(options.clone())
        .unwrap();
    denied(
        planner.spawn(by_seat("worker", "x")),
        "seat planner of rig team may spawn only helper, not worker",
    );
    let helper = planner.spawn(by_seat("helper", "say help")).unwrap();
    assert_eq!(helper.name, "root-planner-helper");
    assert_eq!(helper.depth, 2);
    root.wait_subtree().unwrap();

    // Outside a rig, a seat means nothing.
    let plain = f
        .yard
        .task("say plain")
        .options(delegating(&f, Envelope::default()))
        .name("plain")
        .run()
        .unwrap();
    let plain = plain.delegate(options).unwrap();
    denied(plain.spawn(by_seat("worker", "x")), "not in a rig");
}

#[test]
fn seats_are_checked_before_anything_is_created() {
    let f = Fixture::new();
    let refused = |options: TaskOptions, needle: &str| {
        match f.yard.task("say hi").options(options).name("r").run() {
            Err(Error::Unsupported(why)) if why.contains(needle) => {}
            other => panic!("expected {needle:?}, got {other:?}"),
        }
        assert!(f.yard.branch("r").is_err(), "nothing was created");
    };
    refused(
        TaskOptions {
            seats: Some(team()),
            ..f.options()
        },
        "seats need a delegation envelope",
    );
    let mut loose = team();
    loose.table.insert("loose".into(), seat(&[]));
    refused(
        TaskOptions {
            seats: Some(loose),
            ..delegating(&f, Envelope::default())
        },
        "loose is not below seat lead",
    );
    let mut secret = team();
    secret.table.get_mut("helper").unwrap().provision = Some(Provisioning {
        secrets: vec![SecretSource::parse("GEMINI_API_KEY").unwrap()],
        ..Provisioning::default()
    });
    refused(
        TaskOptions {
            seats: Some(secret.clone()),
            ..delegating(&f, Envelope::default())
        },
        "seat helper",
    );
    // An isolated seat above it gives it a private home.
    secret.table.get_mut("planner").unwrap().isolated = true;
    let root = f
        .yard
        .task("say hi")
        .options(TaskOptions {
            seats: Some(secret),
            ..delegating(&f, Envelope::depth(2))
        })
        .name("r")
        .run()
        .unwrap();
    assert_eq!(root.info().status, BranchStatus::NoChanges);
}
