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

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
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
    wait_until("the prompt", || prompts(&pg.yard, "held") == 1);
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
    wait_until("the harness to report its processes", || {
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
