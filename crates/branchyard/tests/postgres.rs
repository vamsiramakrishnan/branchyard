//! The engine with its state in PostgreSQL (`Yard::open_postgres`) against
//! the fake ACP agent: a task through merge and removal, cursors and
//! waits, two yards on one branch with a cancel, and an engine killed
//! mid-turn and recovered. Runs with the `postgres` feature when
//! `BY_TEST_POSTGRES_URL` names a database the tests may create tables in;
//! otherwise each test says it was skipped.

#![cfg(feature = "postgres")]

mod common;

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use branchyard::{Activity, BranchStatus, Error, Policy, Yard};
use branchyard_testkit::wait;
use common::{fake_agent, text, Fixture};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn url() -> Option<String> {
    let url = std::env::var("BY_TEST_POSTGRES_URL")
        .ok()
        .filter(|u| !u.is_empty());
    if url.is_none() {
        eprintln!("skipped: set BY_TEST_POSTGRES_URL to run the PostgreSQL engine tests");
    }
    url
}

/// A repository whose state is in PostgreSQL under a fresh scope.
struct Pg {
    f: Fixture,
    url: String,
    scope: String,
    yard: Yard,
}

impl Pg {
    fn new() -> Option<Pg> {
        let url = url()?;
        let f = Fixture::new();
        let scope = format!(
            "engine-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let yard = Yard::open_postgres(&f.root, &url, &scope).unwrap();
        Some(Pg {
            f,
            url,
            scope,
            yard,
        })
    }

    fn open(&self) -> Yard {
        Yard::open_postgres(&self.f.root, &self.url, &self.scope).unwrap()
    }
}

fn prompts(yard: &Yard, name: &str) -> usize {
    yard.branch(name)
        .and_then(|b| b.events())
        .unwrap_or_default()
        .iter()
        .filter(|e| matches!(e.activity, Activity::Prompt(_)))
        .count()
}

#[test]
fn a_task_runs_through_merge_with_its_state_in_postgres() {
    let Some(pg) = Pg::new() else { return };
    let options = pg.f.options();
    let branch = pg
        .yard
        .task("WRITE hello.txt=hi")
        .options(options.clone())
        .policy(Policy::allow_all())
        .name("hello")
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    let sent = branch.send("WHOAMI", options.clone()).unwrap();
    assert!(text(&sent.events().unwrap()).contains("resumed=true"));

    // Another yard on the same database sees the same branch and feed.
    let other = pg.open();
    assert_eq!(other.branches().unwrap(), pg.yard.branches().unwrap());
    let feed = other.events_since(0, 1000).unwrap();
    assert_eq!(feed.events.len(), branch.events().unwrap().len());
    assert_eq!(other.events_head().unwrap(), feed.next_cursor);
    let page = other.branch("hello").unwrap().events_since(2, 3).unwrap();
    assert_eq!(page.next_cursor, 5);

    // A SQLite yard on the same repository sees none of it.
    assert!(
        !pg.f.root.join(".branchyard/state.db").exists() || {
            let sqlite = Yard::open(&pg.f.root).unwrap();
            sqlite.branches().unwrap().is_empty()
        }
    );

    let merged = pg.yard.merge("hello", "main").unwrap();
    assert_eq!(merged.target, "main");
    assert_eq!(pg.f.git(&["show", "main:hello.txt"]), "hi\n");
    assert!(matches!(
        pg.yard.merge("hello", "main"),
        Err(Error::AlreadyMerged { .. })
    ));
    let before = other.events_since(0, 1000).unwrap().events.len();
    assert!(before > feed.events.len(), "the merge was recorded");
    pg.yard.remove("hello").unwrap();
    assert!(matches!(
        other.branch("hello"),
        Err(Error::UnknownBranch(_))
    ));
    // Its events stay in the feed.
    assert_eq!(other.events_since(0, 1000).unwrap().events.len(), before);
}

#[test]
fn a_waiting_reader_sees_another_yards_events() {
    let Some(pg) = Pg::new() else { return };
    let head = pg.yard.events_head().unwrap();
    let reader = pg.open();
    let waiting = std::thread::spawn(move || {
        reader
            .wait_for_events(head, 10, Duration::from_secs(30))
            .unwrap()
    });
    std::thread::sleep(Duration::from_millis(200));
    pg.yard
        .task("say hi")
        .options(pg.f.options())
        .name("hi")
        .run()
        .unwrap();
    let page = waiting.join().unwrap();
    assert!(!page.events.is_empty());
    assert_eq!(page.events[0].branch, "hi");
    let empty = pg
        .yard
        .wait_for_events(
            pg.yard.events_head().unwrap(),
            10,
            Duration::from_millis(150),
        )
        .unwrap();
    assert!(empty.events.is_empty());
}

#[test]
fn two_yards_never_drive_one_branch_and_a_cancel_reaches_the_other() {
    let Some(pg) = Pg::new() else { return };
    let turn = {
        let task = pg.yard.task("HANG").options(pg.f.options()).name("held");
        std::thread::spawn(move || task.run())
    };
    wait::until("the prompt", || prompts(&pg.yard, "held") == 1);
    let other = pg.open();
    assert!(other.recover().unwrap().is_empty());
    let held = other.branch("held").unwrap();
    assert!(matches!(
        held.send("WHOAMI", pg.f.options()),
        Err(Error::Running(name)) if name == "held"
    ));
    assert!(matches!(other.remove("held"), Err(Error::Running(_))));
    assert_eq!(other.cancel_as("held", "the other yard").unwrap(), ["held"]);
    let ended = turn.join().unwrap().unwrap();
    assert_eq!(ended.info().status, BranchStatus::Interrupted);
    assert!(ended
        .events()
        .unwrap()
        .iter()
        .any(|e| e.activity == Activity::Warning("cancelled by the other yard".into())));
    assert!(other.cancel("held").unwrap().is_empty());
    let sent = held.send("WHOAMI", pg.f.options()).unwrap();
    assert_eq!(sent.info().status, BranchStatus::NoChanges);
}

/// Run a turn with the state in PostgreSQL, from the environment the
/// crash test sets. Run only as that test's child, which kills it.
#[test]
#[ignore = "the child process of the PostgreSQL crash test"]
fn postgres_engine_child() {
    let var = |name: &str| std::env::var(name).ok();
    let (Some(root), Some(url), Some(scope), Some(agent)) = (
        var("BY_CHILD_ROOT"),
        var("BY_CHILD_URL"),
        var("BY_CHILD_SCOPE"),
        var("BY_CHILD_AGENT"),
    ) else {
        return;
    };
    let yard = Yard::open_postgres(root, &url, &scope).unwrap();
    let _ = yard
        .task("ORPHAN")
        .harness("gemini-cli")
        .command([agent])
        .policy(Policy::allow_all())
        .name("crashy")
        .run();
}

#[test]
fn a_killed_engine_is_recovered_from_postgres_and_nothing_resubmitted() {
    let Some(pg) = Pg::new() else { return };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "postgres_engine_child",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("BY_CHILD_ROOT", &pg.f.root)
        .env("BY_CHILD_URL", &pg.url)
        .env("BY_CHILD_SCOPE", &pg.scope)
        .env("BY_CHILD_AGENT", fake_agent())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut pids: Vec<u32> = Vec::new();
    wait::until("the harness to report its processes", || {
        let said = pg
            .yard
            .branch("crashy")
            .and_then(|b| b.events())
            .map(|e| text(&e))
            .unwrap_or_default();
        pids = said
            .strip_prefix("orphan ")
            .map(|rest| {
                rest.split_whitespace()
                    .filter_map(|p| p.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        pids.len() == 2
    });
    child.kill().unwrap();
    child.wait().unwrap();

    let yard = pg.open();
    let branch = yard.branch("crashy").unwrap();
    assert_eq!(branch.info().status, BranchStatus::Interrupted);
    assert_eq!(branch.info().turns, 1);
    let log = branch.events().unwrap();
    let recovered: Vec<(String, Vec<u32>)> = log
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Recovered { reason, killed } => Some((reason.clone(), killed.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(recovered.len(), 1, "{log:?}");
    assert!(
        recovered[0].0.contains("It was not submitted again"),
        "{}",
        recovered[0].0
    );
    assert!(
        pids.iter().all(|p| recovered[0].1.contains(p)),
        "{recovered:?} {pids:?}"
    );
    assert_eq!(prompts(&yard, "crashy"), 1);
    assert!(yard.recover().unwrap().is_empty());
}

/// Task graphs on PostgreSQL: an invalid proposal changes nothing, a
/// dependent waits for its prerequisite and starts in another yard's
/// process when that yard integrates it, and a failed prerequisite blocks.
#[test]
fn a_graph_of_children_runs_with_its_state_in_postgres() {
    use branchyard::{After, Envelope, GraphEdit, SpawnSpec};
    let Some(pg) = Pg::new() else { return };
    let options = branchyard::TaskOptions {
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..pg.f.options()
    };
    let root = pg
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    let spawn = |name: &str, prompt: &str, depends_on: &[&str], after: After| {
        GraphEdit::Spawn(SpawnSpec {
            prompt: prompt.into(),
            name: Some(name.into()),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            after,
            ..SpawnSpec::default()
        })
    };
    let cycle = delegate.apply_graph(
        vec![
            spawn("x", "say", &["y"], After::Settled),
            spawn("y", "say", &["x"], After::Settled),
        ],
        0,
    );
    assert!(matches!(cycle, Err(Error::Denied(_))), "{cycle:?}");
    assert!(pg.yard.branch("x").is_err());
    assert_eq!(pg.yard.graph("root").unwrap().revision, 0);
    let applied = delegate
        .apply_graph(
            vec![
                spawn("lib", "WRITE lib.txt=1", &[], After::Settled),
                spawn("app", "WRITE app.txt=1", &["lib"], After::Integrated),
                spawn("bad", "EXIT", &[], After::Settled),
                spawn("blocked", "say", &["bad"], After::Settled),
            ],
            0,
        )
        .unwrap();
    assert_eq!(applied.revision, 1);
    assert!(matches!(
        delegate.apply_graph(vec![spawn("z", "say", &[], After::Settled)], 0),
        Err(Error::StaleRevision { .. })
    ));
    root.wait_subtree().unwrap();
    let status = |yard: &Yard, name: &str| yard.branch(name).unwrap().info().status.clone();
    assert_eq!(status(&pg.yard, "app"), BranchStatus::Waiting);
    assert!(matches!(
        status(&pg.yard, "blocked"),
        BranchStatus::Blocked { .. }
    ));
    // Another yard integrates lib; app starts there.
    let other = pg.open();
    let parent = other.branch("root").unwrap();
    parent.delegate(options).unwrap().integrate("lib").unwrap();
    parent.wait_subtree().unwrap();
    assert_eq!(status(&pg.yard, "app"), BranchStatus::Ready);
    assert_eq!(pg.yard.graph("root").unwrap().dependencies.len(), 2);
}

/// Several processes on one database run `resume_graph` at once (every
/// server and `by worker` does on its recovery tick): a dependent whose
/// prerequisite settled with no engine starting it is claimed, and so
/// started, by exactly one of them.
#[test]
fn concurrent_resume_graph_on_one_database_starts_a_dependent_once() {
    use branchyard::{After, Envelope, GraphEdit, SpawnSpec};
    let Some(pg) = Pg::new() else { return };
    let options = branchyard::TaskOptions {
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..pg.f.options()
    };
    let root = pg
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let spawn = |name: &str, prompt: &str, depends_on: &[&str], after: After| {
        GraphEdit::Spawn(SpawnSpec {
            prompt: prompt.into(),
            name: Some(name.into()),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            after,
            ..SpawnSpec::default()
        })
    };
    root.delegate(options.clone())
        .unwrap()
        .apply_graph(
            vec![
                spawn("first", "WRITE f.txt=1", &[], After::Settled),
                spawn("second", "WRITE s.txt=1", &["first"], After::Integrated),
            ],
            0,
        )
        .unwrap();
    root.wait_subtree().unwrap();
    let status = |yard: &Yard, name: &str| yard.branch(name).unwrap().info().status.clone();
    assert_eq!(status(&pg.yard, "second"), BranchStatus::Waiting);
    // The state an engine that stopped between settling the prerequisite
    // and starting the dependent leaves: satisfied, never claimed.
    let mut db = postgres::Client::connect(&pg.url, postgres::NoTls).unwrap();
    let changed = db
        .execute(
            "UPDATE by_graph_edges SET after = 'settled' WHERE repo = $1 AND dependent = 'second'",
            &[&pg.scope],
        )
        .unwrap();
    assert_eq!(changed, 1);

    const RACERS: usize = 6;
    let yards: Vec<Yard> = (0..RACERS).map(|_| pg.open()).collect();
    let barrier = std::sync::Barrier::new(RACERS);
    let started: Vec<Vec<String>> = std::thread::scope(|s| {
        let handles: Vec<_> = yards
            .iter()
            .map(|yard| {
                let (barrier, options) = (&barrier, options.clone());
                s.spawn(move || {
                    barrier.wait();
                    yard.resume_graph(&options).unwrap()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let winners: Vec<usize> = (0..RACERS)
        .filter(|&i| started[i].iter().any(|n| n == "second"))
        .collect();
    assert_eq!(winners.len(), 1, "started by {winners:?}: {started:?}");
    yards[winners[0]]
        .branch("root")
        .unwrap()
        .wait_subtree()
        .unwrap();
    assert_eq!(status(&pg.yard, "second"), BranchStatus::Ready);
    assert_eq!(prompts(&pg.yard, "second"), 1, "one turn, from one engine");
    assert_eq!(pg.yard.branch("second").unwrap().info().turns, 1);
    // Nothing is left for any of them.
    for yard in &yards {
        assert!(yard.resume_graph(&options).unwrap().is_empty());
    }
    assert_eq!(prompts(&pg.yard, "second"), 1);
}

#[test]
fn a_workspace_port_is_reserved_in_postgres_stable_and_released_on_removal() {
    let Some(pg) = Pg::new() else { return };
    let spec = branchyard::WorkspaceSpec {
        setup: vec!["echo $BRANCHYARD_PORT > port.txt".into()],
        teardown: vec!["echo $BRANCHYARD_PORT > $BRANCHYARD_ROOT/torn.txt".into()],
        ..Default::default()
    };
    let options = branchyard::TaskOptions {
        workspace: Some(spec),
        ..pg.f.options()
    };
    let one = pg
        .yard
        .task("ENV BRANCHYARD_PORT")
        .options(options.clone())
        .name("ws-one")
        .run()
        .unwrap();
    let two = pg
        .yard
        .task("ENV BRANCHYARD_PORT")
        .options(options)
        .name("ws-two")
        .run()
        .unwrap();
    let port = pg.yard.workspace("ws-one").unwrap().port.unwrap();
    assert_ne!(Some(port), pg.yard.workspace("ws-two").unwrap().port);
    assert_eq!(
        std::fs::read_to_string(one.info().worktree.join("port.txt"))
            .unwrap()
            .trim(),
        port.to_string()
    );
    // Another engine on the database: the same port on the next turn.
    let again = pg
        .open()
        .branch("ws-one")
        .unwrap()
        .send("ENV BRANCHYARD_PORT", pg.f.options())
        .unwrap();
    let said = text(&again.events().unwrap());
    assert_eq!(said.matches(&format!("BRANCHYARD_PORT={port}")).count(), 2);
    let report = pg
        .yard
        .remove_reporting("ws-one", &Default::default())
        .unwrap()
        .unwrap();
    assert!(report.ok, "{report:?}");
    assert_eq!(
        std::fs::read_to_string(pg.f.root.join("torn.txt"))
            .unwrap()
            .trim(),
        port.to_string()
    );
    assert!(pg.yard.workspace("ws-one").is_err());
    assert!(pg.yard.workspace("ws-two").unwrap().ready);
    drop(two);
}

/// A kept sandbox and its snapshots recorded in PostgreSQL: a second
/// engine (another `Yard::open_postgres`) resumes the sandbox the first
/// parked, forks from a snapshot the first took, and removal releases
/// everything; the fake provider stands in for Microsandbox.
#[test]
fn kept_sandboxes_and_snapshots_are_recorded_in_postgres_across_engines() {
    use branchyard::{
        Provider, SandboxEvent, SandboxKeep, SandboxOptions, SandboxOrigin, SnapshotMethod,
        TaskOptions,
    };
    use branchyard_sandbox::fake::FakeProvider;
    let Some(pg) = Pg::new() else { return };
    let fake = std::sync::Arc::new(FakeProvider::live(
        Box::new(branchyard_runtime::LocalProvider::new()),
        pg.f.dir.join("fake-provider"),
    ));
    let options = TaskOptions {
        provider: Some(Provider::Microsandbox(SandboxOptions {
            image: "registry.example/harness:1".into(),
            keep: SandboxKeep::Pause,
            ..SandboxOptions::default()
        })),
        policy: Policy::allow_all(),
        ..pg.f.options()
    };
    pg.yard.use_sandbox_provider(fake.clone());
    let first = pg
        .yard
        .task("SH printf one > \"$BY_FAKE_ROOTFS/marker\"")
        .options(options.clone())
        .name("pg-kept")
        .run()
        .unwrap();
    assert!(
        first
            .events()
            .unwrap()
            .iter()
            .any(|e| matches!(&e.activity, Activity::Sandbox(event) if matches!(**event, SandboxEvent::Kept { .. }))),
        "{:?}",
        first.events()
    );
    let other = pg.open();
    other.use_sandbox_provider(fake.clone());
    let sent = other
        .branch("pg-kept")
        .unwrap()
        .send("SH cat \"$BY_FAKE_ROOTFS/marker\"", options.clone())
        .unwrap();
    let origins: Vec<SandboxOrigin> = sent
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Sandbox(event) => match *event {
                SandboxEvent::Started { origin, .. } => Some(origin),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(origins.last(), Some(&SandboxOrigin::Resumed));
    assert!(text(&sent.events().unwrap()).contains("one"));
    let fork = other
        .branch("pg-kept")
        .unwrap()
        .fork_at(
            1,
            "SH cat \"$BY_FAKE_ROOTFS/marker\"",
            TaskOptions {
                name: Some("pg-fork".into()),
                ..options.clone()
            },
        )
        .unwrap();
    let origin = fork
        .events()
        .unwrap()
        .into_iter()
        .find_map(|e| match e.activity {
            Activity::Sandbox(event) => match *event {
                SandboxEvent::Started { origin, .. } => Some(origin),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert_eq!(
        origin,
        SandboxOrigin::Branched {
            branch: "pg-kept".into(),
            turn: 1,
            method: SnapshotMethod::LiveBranch,
        }
    );
    pg.yard.remove("pg-fork").unwrap();
    pg.yard.remove("pg-kept").unwrap();
    assert!(fake.sandboxes().is_empty(), "{:?}", fake.sandboxes());
}

fn pool_spec(setup: &str) -> branchyard::WorkspaceSpec {
    branchyard::WorkspaceSpec {
        setup: vec![setup.into()],
        prepare: true,
        pool: Some(branchyard::PoolSpec {
            size: 2,
            ..branchyard::PoolSpec::default()
        }),
        ..branchyard::WorkspaceSpec::default()
    }
}

const POOL_SETUP: &str = "mkdir -p deps && echo built > deps/lib.txt";

/// The slot a branch's worktree came from, if any.
fn pool_used(branch: &branchyard::Branch) -> Option<String> {
    branch
        .events()
        .unwrap()
        .iter()
        .rev()
        .find_map(|e| match &e.activity {
            Activity::Workspace(r) if r.phase == branchyard::WorkspacePhase::Setup => {
                Some(r.pool.as_ref().and_then(|p| p.slot.clone()))
            }
            _ => None,
        })
        .flatten()
}

/// Fill a pool whose setup stops, with the state in PostgreSQL. Run only
/// as the pool recovery test's child, which kills it.
#[test]
#[ignore = "the child process of the PostgreSQL pool recovery test"]
fn postgres_pool_child() {
    let var = |name: &str| std::env::var(name).ok();
    let (Some(root), Some(url), Some(scope), Some(setup)) = (
        var("BY_CHILD_ROOT"),
        var("BY_CHILD_URL"),
        var("BY_CHILD_SCOPE"),
        var("BY_CHILD_SETUP"),
    ) else {
        return;
    };
    let yard = Yard::open_postgres(root, &url, &scope).unwrap();
    let _ = yard.fill_pool(&pool_spec(&setup));
}

#[test]
fn a_warm_pool_is_kept_in_postgres_across_restarts_and_claimed_once() {
    let Some(pg) = Pg::new() else { return };
    std::fs::write(pg.f.root.join(".gitignore"), "deps/\n").unwrap();
    pg.f.git(&["add", ".gitignore"]);
    pg.f.git(&["commit", "-q", "-m", "ignore"]);

    // A filler killed in setup leaves a row and a worktree; the next open
    // reclaims both.
    let marker = pg.f.dir.join("started");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "postgres_pool_child",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("BY_CHILD_ROOT", &pg.f.root)
        .env("BY_CHILD_URL", &pg.url)
        .env("BY_CHILD_SCOPE", &pg.scope)
        .env(
            "BY_CHILD_SETUP",
            format!("touch {}; sleep 120", marker.display()),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait::until("setup in the slot", || marker.exists());
    child.kill().unwrap();
    child.wait().unwrap();
    let left = pg.yard.pool_slots().unwrap();
    assert_eq!(left.len(), 1);
    assert!(left[0].path.is_dir());
    let yard = pg.open();
    assert!(yard.pool_slots().unwrap().is_empty());
    assert!(!left[0].path.exists());

    // Filled, then seen whole by another engine: nothing lost, nothing
    // made twice.
    let spec = pool_spec(POOL_SETUP);
    let filled = yard.fill_pool(&spec).unwrap();
    assert_eq!(filled.made.len(), 2, "{filled:?}");
    let again = pg.open();
    assert_eq!(again.pool_status(&spec).unwrap().ready(), 2);
    assert!(again.fill_pool(&spec).unwrap().made.is_empty());

    // Two engines' branches at once each take a different slot.
    let used: Vec<Option<String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = ["p", "q"]
            .into_iter()
            .map(|name| {
                let yard = pg.open();
                let options = branchyard::TaskOptions {
                    workspace: Some(spec.clone()),
                    ..pg.f.options()
                };
                scope.spawn(move || {
                    let branch = yard
                        .task("SH cat deps/lib.txt")
                        .options(options)
                        .name(name)
                        .run()
                        .unwrap();
                    assert!(text(&branch.events().unwrap()).contains("built"));
                    pool_used(&branch)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(used.iter().all(Option::is_some), "{used:?}");
    assert_ne!(used[0], used[1]);
    assert!(pg.open().pool_slots().unwrap().is_empty());
}
