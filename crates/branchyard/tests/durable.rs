//! Durable execution against the fake ACP agent: an engine killed mid-turn
//! and recovered, leases between two yards, journaled steps replayed rather
//! than repeated, durable cancellation, event cursors, and the import of
//! state left by earlier versions, and input steered into a turn another
//! process runs.
//!
//! Crashes are real: a child process (this test binary, running the
//! ignored `engine_child` test) starts a turn and is sent SIGKILL.

#![allow(clippy::let_underscore_must_use, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use branchyard::{
    Activity, BranchStatus, Error, Event, Policy, RecordedEvent, SteerState, TurnOutcome, Yard,
};
use branchyard_testkit::wait;
use common::{edit_record, fake_agent, text, Fixture};

/// Whether `pid` is a running (not zombie) process.
fn running(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    out.status.success() && !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

/// Run a turn in the repository at `BY_CHILD_ROOT` with the prompt in
/// `BY_CHILD_PROMPT`, on a branch named `crashy`. Run only as the child of
/// a crash test, which kills it.
#[test]
#[ignore = "the child process of the crash tests"]
fn engine_child() {
    let (Some(root), Some(prompt), Some(agent)) = (
        std::env::var_os("BY_CHILD_ROOT"),
        std::env::var("BY_CHILD_PROMPT").ok(),
        std::env::var("BY_CHILD_AGENT").ok(),
    ) else {
        return;
    };
    let yard = Yard::open(root).unwrap();
    let _ = yard
        .task(prompt)
        .harness("gemini-cli")
        .command([agent])
        .policy(Policy::allow_all())
        .name("crashy")
        .run();
}

fn start_child(f: &Fixture, prompt: &str, env: &[(&str, &str)]) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["engine_child", "--exact", "--ignored", "--test-threads=1"])
        .env("BY_CHILD_ROOT", &f.root)
        .env("BY_CHILD_AGENT", fake_agent())
        .env("BY_CHILD_PROMPT", prompt)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (name, value) in env {
        command.env(name, value);
    }
    command.spawn().unwrap()
}

fn events(yard: &Yard, name: &str) -> Vec<RecordedEvent> {
    match yard.branch(name) {
        Ok(branch) => branch.events().unwrap(),
        Err(_) => Vec::new(),
    }
}

fn recovered(events: &[RecordedEvent]) -> Vec<(String, Vec<u32>)> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Recovered { reason, killed } => Some((reason.clone(), killed.clone())),
            _ => None,
        })
        .collect()
}

/// Leave a report pending for `to` directly in the store, as a child's
/// `by report` would, before `to`'s engine starts; its id.
fn pending_report(f: &Fixture, to: &str) -> i64 {
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    db.execute(
        "INSERT INTO messages (from_branch, to_branch, kind, text, at_ms) \
         VALUES ('kid', ?1, 'report', 'tests pass', 1)",
        [to],
    )
    .unwrap();
    db.last_insert_rowid()
}

fn delivered(f: &Fixture, id: i64) -> bool {
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    db.query_row(
        "SELECT delivered_ms IS NOT NULL FROM messages WHERE id = ?1",
        [id],
        |r| r.get(0),
    )
    .unwrap()
}

fn prompts(events: &[RecordedEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.activity, Activity::Prompt(_)))
        .count()
}

#[test]
fn a_killed_engine_is_recovered_its_harness_killed_and_nothing_resubmitted() {
    let f = Fixture::new();
    // Opens the store, so the message can be written before the child
    // starts; the child's turn start prepends it to the prompt.
    let report = pending_report(&f, "crashy");
    let mut child = start_child(&f, "ORPHAN", &[]);
    let mut pids = Vec::new();
    wait::until("the harness to report its processes", || {
        let said = text(&events(&f.yard, "crashy"));
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
    let (agent, sleeper) = (pids[0], pids[1]);
    assert!(running(agent) && running(sleeper));

    // The engine dies. The harness ignores its closed stdin, but on Linux
    // it dies with its engine (its parent-death signal); elsewhere it lives
    // on. The command it started in its process group lives on either way.
    child.kill().unwrap();
    child.wait().unwrap();
    if cfg!(target_os = "linux") {
        wait::until("the harness to die with its engine", || !running(agent));
    } else {
        wait::settle(
            "a harness that was going to die with its engine would have by now",
            Duration::from_millis(200),
        );
        assert!(running(agent), "the harness outlived its engine");
    }
    assert!(running(sleeper), "its group outlived its engine");
    assert_eq!(
        f.yard.branch("crashy").unwrap().info().status,
        BranchStatus::Running
    );

    let yard = Yard::open(&f.root).unwrap();
    let branch = yard.branch("crashy").unwrap();
    assert_eq!(branch.info().status, BranchStatus::Interrupted);
    assert_eq!(branch.info().turns, 1, "the prompt had been submitted");
    assert_eq!(branch.info().session.as_deref(), Some("fake-session-1"));
    let log = branch.events().unwrap();
    let found = recovered(&log);
    assert_eq!(found.len(), 1, "{log:?}");
    let (reason, killed) = &found[0];
    assert!(reason.contains("is no longer running"), "{reason}");
    assert!(reason.contains("the prompt had been submitted"), "{reason}");
    assert!(
        reason.contains("It was not submitted again: `by send crashy --retry` submits it again"),
        "{reason}"
    );
    assert!(
        killed.contains(&sleeper) && (cfg!(target_os = "linux") || killed.contains(&agent)),
        "{killed:?}"
    );
    wait::until("the harness's process group to die", || {
        !running(agent) && !running(sleeper)
    });
    assert_eq!(
        log.last().unwrap().activity,
        Activity::Status(BranchStatus::Interrupted)
    );
    let worktree = &branch.info().worktree;
    let noted = fs::read_to_string(worktree.join("orphan.log")).unwrap();
    assert_eq!(
        noted.lines().count(),
        1,
        "the prompt reached the harness once"
    );
    assert_eq!(prompts(&log), 1);
    // The report went out with the journaled prompt: it counts delivered
    // and is not given again.
    assert!(log.iter().any(|e| matches!(
        &e.activity,
        Activity::Prompt(prompt) if prompt.contains("tests pass") && prompt.ends_with("ORPHAN")
    )));
    assert!(delivered(&f, report));
    assert!(log.iter().any(|e| matches!(
        &e.activity,
        Activity::MessagesDelivered { ids, .. } if ids == &[report as u64]
    )));

    // A second recovery finds nothing, and the branch continues its
    // session with a new turn, after which there is nothing to retry.
    assert!(yard.recover().unwrap().is_empty());
    assert!(branch.retry_prompt().unwrap().ends_with("ORPHAN"));
    let sent = branch.send("WHOAMI", f.options()).unwrap();
    assert!(
        matches!(sent.retry_prompt(), Err(Error::Denied(why)) if why.contains("no cut-off turn"))
    );
    // The cut-off turn's orphan.log is the candidate, which this turn,
    // changing nothing, keeps: the branch is ready with it.
    assert_eq!(sent.info().status, BranchStatus::Ready);
    assert_eq!(
        sent.info().candidate.as_ref().unwrap().commit,
        branch.info().candidate.as_ref().unwrap().commit
    );
    assert!(text(&sent.events().unwrap()).contains("session fake-session-1 resumed=true"));
    assert_eq!(
        noted,
        fs::read_to_string(worktree.join("orphan.log")).unwrap()
    );
}

#[test]
fn a_crash_before_the_prompt_was_submitted_says_the_turn_never_ran() {
    let f = Fixture::new();
    let report = pending_report(&f, "crashy");
    let mut child = start_child(&f, "WRITE never.txt=1", &[("FAKE_ACP_SILENT", "1")]);
    let db = f.root.join(".branchyard/state.db");
    wait::until("the harness to start", || {
        rusqlite::Connection::open(&db)
            .and_then(|c| c.query_row("SELECT COUNT(*) FROM processes", [], |r| r.get(0)))
            .is_ok_and(|n: i64| n == 1)
    });
    child.kill().unwrap();
    child.wait().unwrap();

    let yard = Yard::open(&f.root).unwrap();
    let branch = yard.branch("crashy").unwrap();
    assert_eq!(branch.info().status, BranchStatus::Interrupted);
    assert_eq!(branch.info().turns, 0);
    let log = branch.events().unwrap();
    let found = recovered(&log);
    assert_eq!(found.len(), 1);
    assert!(
        found[0].0.ends_with(
            "before the prompt was submitted; the turn never ran: `by send crashy --retry` \
             runs its prompt"
        ),
        "{}",
        found[0].0
    );
    assert_eq!(branch.retry_prompt().unwrap(), "WRITE never.txt=1");
    assert_eq!(prompts(&log), 0);
    assert!(!branch.info().worktree.join("never.txt").exists());
    assert!(
        !delivered(&f, report),
        "a turn that never ran delivers nothing"
    );
}

/// The battery's crash scenario: a child cut off before its harness
/// recorded a session could not be continued (`has no harness session to
/// resume`), and nothing replayed its prompt. `retry_prompt` (`by send
/// --retry`) gives the cut-off prompt back, and a send to a branch with no
/// session starts a fresh one that begins with every earlier prompt.
#[test]
fn a_cut_off_turn_is_retried_and_a_lost_session_starts_fresh_with_its_prompts() {
    let f = Fixture::new();
    let go = f.dir.join("go");
    let prompt = format!(
        "SH until [ -f {} ]; do sleep 0.05; done; echo done > retried.txt",
        go.display()
    );
    let mut child = start_child(&f, &prompt, &[]);
    wait::until("the prompt", || prompts(&events(&f.yard, "crashy")) == 1);
    child.kill().unwrap();
    child.wait().unwrap();

    let yard = Yard::open(&f.root).unwrap();
    let branch = yard.branch("crashy").unwrap();
    assert_eq!(branch.retry_prompt().unwrap(), prompt);
    // As Claude Code leaves a branch whose engine stopped before the
    // harness named its session.
    edit_record(&f.root, "crashy", |record| {
        record["info"]["session"] = serde_json::Value::Null;
    });
    fs::write(&go, "").unwrap();
    let sent = branch
        .send(&branch.retry_prompt().unwrap(), f.options())
        .unwrap();
    assert!(sent.info().worktree.join("retried.txt").is_file());
    let log = sent.events().unwrap();
    assert!(log.iter().any(|e| matches!(&e.activity,
        Activity::Warning(w) if w.contains("has no harness session to resume")
            && w.contains("starts a fresh session"))));
    let last = log
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Prompt(p) => Some(p.clone()),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert!(last.contains("Its harness session was lost"), "{last}");
    assert!(last.contains(&format!("### Prompt 1\n{prompt}")), "{last}");
    assert!(last.contains("<branchyard-recovered>"), "{last}");
    assert!(sent.retry_prompt().is_err(), "a prompt reached the harness");
}

#[test]
fn two_yards_never_drive_one_branch_and_a_cancel_reaches_the_other() {
    let f = Fixture::new();
    let turn = {
        let task = f.task("HANG").name("held");
        std::thread::spawn(move || task.run())
    };
    wait::until("the prompt", || prompts(&events(&f.yard, "held")) == 1);

    // Another engine on the same repository: it recovers nothing, since
    // the lease is live, and may not start a turn, merge or remove.
    let other = Yard::open(&f.root).unwrap();
    assert!(other.recover().unwrap().is_empty());
    let held = other.branch("held").unwrap();
    assert!(matches!(
        held.send("WHOAMI", f.options()),
        Err(Error::Running(name)) if name == "held"
    ));
    assert!(matches!(other.remove("held"), Err(Error::Running(_))));
    assert!(matches!(
        other.merge("held", "main"),
        Err(Error::Running(_))
    ));

    // Its cancel is durable, and the engine running the turn observes it.
    assert_eq!(other.cancel_as("held", "the other yard").unwrap(), ["held"]);
    let ended = turn.join().unwrap().unwrap();
    assert_eq!(ended.info().status, BranchStatus::Interrupted);
    let log = ended.events().unwrap();
    assert!(log
        .iter()
        .any(|e| e.activity == Activity::Warning("cancelled by the other yard".into())));
    // Nothing runs any more; a later turn is not stopped by that cancel.
    assert!(other.cancel("held").unwrap().is_empty());
    let sent = held.send("WHOAMI", f.options()).unwrap();
    assert_eq!(sent.info().status, BranchStatus::NoChanges);
}

#[test]
fn a_recorded_snapshot_is_used_when_recovery_finishes_a_turn() {
    let f = Fixture::new();
    let branch = f.task("WRITE r.txt=1").name("again").run().unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    let candidate = branch.info().candidate.clone().unwrap();
    let head = f.git(&["rev-parse", "by/again"]);
    // Stand in for an engine that died after journaling the snapshot and
    // before settling the record: the record still says running, and the
    // lease is held by a process that no longer exists.
    let mut gone = Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    edit_record(&f.root, "again", |record| {
        record["info"]["status"] = serde_json::json!({"state": "running"});
        record["info"]["candidate"] = serde_json::Value::Null;
        record["info"]["turns"] = 0.into();
    });
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    db.execute(
        "UPDATE leases SET owner = 'gone', pid = ?1, expires_ms = 9999999999999",
        [dead],
    )
    .unwrap();
    drop(db);

    let yard = Yard::open(&f.root).unwrap();
    let recovered_branch = yard.branch("again").unwrap();
    let info = recovered_branch.info();
    assert_eq!(info.status, BranchStatus::Ready);
    assert_eq!(info.candidate.as_ref(), Some(&candidate));
    assert_eq!(info.turns, 1);
    assert_eq!(f.git(&["rev-parse", "by/again"]), head, "nothing committed");
    let log = recovered_branch.events().unwrap();
    let snapshots = log
        .iter()
        .filter(|e| matches!(e.activity, Activity::Snapshot(_)))
        .count();
    assert_eq!(snapshots, 1, "the recorded snapshot was not taken again");
    let found = recovered(&log);
    assert!(found[0].0.contains("the turn had ended"), "{}", found[0].0);
    assert!(found[0].1.is_empty());
}

#[test]
fn a_merge_cut_short_after_moving_the_target_is_recognised_not_repeated() {
    let f = Fixture::new();
    let count = f.dir.join("checks");
    let check = format!("echo run >> '{}'", count.display());
    f.task("WRITE m.txt=1")
        .name("merging")
        .check(["sh", "-c", &check])
        .run()
        .unwrap();
    let merged = f.yard.merge("merging", "main").unwrap();
    assert_eq!(fs::read_to_string(&count).unwrap().lines().count(), 1);
    let main = f.git(&["rev-parse", "main"]);
    // An engine that moved the target and stopped before recording it.
    edit_record(&f.root, "merging", |record| {
        record["info"]["status"] = serde_json::json!({"state": "ready"});
    });
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    let pending = db
        .execute(
            "UPDATE steps SET outcome = NULL WHERE step LIKE 'merge main %'",
            [],
        )
        .unwrap();
    assert_eq!(pending, 1);
    drop(db);

    let again = f.yard.merge("merging", "main").unwrap();
    assert_eq!(again.commit, merged.commit);
    assert_eq!(again.previous, merged.previous);
    assert_eq!(
        f.git(&["rev-parse", "main"]),
        main,
        "the target did not move"
    );
    assert_eq!(
        fs::read_to_string(&count).unwrap().lines().count(),
        1,
        "the check did not run again"
    );
    assert!(matches!(
        f.yard.branch("merging").unwrap().info().status,
        BranchStatus::Merged { .. }
    ));
    // A deliberate repeat is still refused.
    assert!(matches!(
        f.yard.merge("merging", "main"),
        Err(Error::AlreadyMerged { .. })
    ));
}

#[test]
fn events_are_read_from_cursors_and_waiting_readers_wake() {
    let f = Fixture::new();
    let branch = f.task("WRITE e.txt=1").name("paged").run().unwrap();
    let all = branch.events().unwrap();
    assert!(all.len() > 4);
    let first = branch.events_since(0, 3).unwrap();
    assert_eq!(first.events, all[..3]);
    assert_eq!(first.next_cursor, 3);
    let rest = branch.events_since(first.next_cursor, 10_000).unwrap();
    assert_eq!(rest.events, all[3..]);
    assert_eq!(rest.next_cursor, all.len() as u64);
    let end = branch.events_since(rest.next_cursor, 10).unwrap();
    assert!(end.events.is_empty());
    assert_eq!(end.next_cursor, rest.next_cursor);

    // The repository feed numbers every branch's events once, in order.
    let head = f.yard.events_head().unwrap();
    let feed = f.yard.events_since(0, 10_000).unwrap();
    assert_eq!(feed.next_cursor, head);
    assert_eq!(feed.events.len() as u64, head);
    let positions: Vec<u64> = feed.events.iter().map(|e| e.position).collect();
    assert_eq!(positions, (1..=head).collect::<Vec<_>>());
    assert!(feed.events.iter().all(|e| e.branch == "paged"));

    // A waiting reader times out empty, then wakes for a turn in another
    // thread without waiting out its timeout.
    let quiet = f
        .yard
        .wait_for_events(head, 10, Duration::from_millis(50))
        .unwrap();
    assert!(quiet.events.is_empty());
    assert_eq!(quiet.next_cursor, head);
    let sender = {
        let (branch, options) = (branch.clone(), f.options());
        std::thread::spawn(move || branch.send("WHOAMI", options).unwrap())
    };
    let started = Instant::now();
    let woke = f
        .yard
        .wait_for_events(head, 10, Duration::from_secs(30))
        .unwrap();
    assert!(!woke.events.is_empty());
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(woke.events[0].position, head + 1);
    let waited = branch
        .wait_for_events(all.len() as u64, 1, Duration::from_secs(30))
        .unwrap();
    assert_eq!(waited.events.len(), 1);
    sender.join().unwrap();

    // Events of a removed branch stay in the feed; a new branch with its
    // name starts its own numbering.
    let before = f.yard.events_head().unwrap();
    f.yard.remove("paged").unwrap();
    assert_eq!(f.yard.events_head().unwrap(), before);
    let again = f.task("WHOAMI").name("paged").run().unwrap();
    assert_eq!(again.events_since(0, 1).unwrap().next_cursor, 1);
}

/// A record in the JSON layout earlier versions wrote.
fn legacy_record(root: &Path, name: &str, status: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "info": {
            "name": name,
            "git_branch": format!("by/{name}"),
            "worktree": root.join(".branchyard/worktrees").join(name),
            "prompt": "old work",
            "harness": "gemini-cli",
            "profile": "gemini-cli-acp",
            "session": null,
            "parent": null,
            "base": "0000000000000000000000000000000000000000",
            "candidate": null,
            "status": status,
            "turns": 1,
            "cost_usd": null,
            "created_at": 1
        },
        "created_ms": 1000,
        "check": null,
        "command": null,
        "home": null,
        "cost_baseline": null
    })
}

#[test]
fn records_and_logs_of_earlier_versions_are_imported_once() {
    let f = Fixture::new();
    let old = tempdir(&f, "old");
    fs::create_dir_all(old.join(".branchyard/branches")).unwrap();
    fs::create_dir_all(old.join(".branchyard/events")).unwrap();
    let write = |name: &str, status: serde_json::Value| {
        let record = legacy_record(&old, name, status);
        fs::write(
            old.join(format!(".branchyard/branches/{name}.json")),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .unwrap();
    };
    write("done", serde_json::json!({"state": "no_changes"}));
    write("stuck", serde_json::json!({"state": "running"}));
    fs::write(old.join(".branchyard/branches/reserved.json"), "").unwrap();
    let line = |at_ms: u64, activity: Activity| {
        serde_json::to_string(&RecordedEvent { at_ms, activity }).unwrap() + "\n"
    };
    fs::write(
        old.join(".branchyard/events/done.jsonl"),
        line(10, Activity::Prompt("old work".into()))
            + &line(30, Activity::Status(BranchStatus::NoChanges))
            + "{\"at_ms\": 40, \"activ",
    )
    .unwrap();
    fs::write(
        old.join(".branchyard/events/stuck.jsonl"),
        line(20, Activity::Prompt("old work".into())),
    )
    .unwrap();

    let yard = Yard::open(&old).unwrap();
    let names: Vec<String> = yard
        .branches()
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(names, ["done", "stuck"]);
    let done = yard.branch("done").unwrap();
    assert_eq!(done.info().status, BranchStatus::NoChanges);
    assert_eq!(done.events().unwrap().len(), 2, "the torn line is dropped");
    // The feed orders imported events by time across branches.
    let feed = yard.events_since(0, 100).unwrap();
    let order: Vec<(&str, u64)> = feed
        .events
        .iter()
        .take(3)
        .map(|e| (e.branch.as_str(), e.event.at_ms))
        .collect();
    assert_eq!(order, [("done", 10), ("stuck", 20), ("done", 30)]);
    // A branch an earlier version left running is recovered, and the
    // reserved name stays taken.
    let stuck = yard.branch("stuck").unwrap();
    assert_eq!(stuck.info().status, BranchStatus::Interrupted);
    let found = recovered(&stuck.events().unwrap());
    assert!(found[0].0.contains("earlier version"), "{}", found[0].0);
    assert!(!matches!(
        yard.task("x").name("reserved").planned_names(&[]),
        Ok(names) if names == ["reserved"]
    ));
    // The old files moved aside and are not imported again.
    assert!(!old.join(".branchyard/branches").exists());
    assert!(old.join(".branchyard/legacy/branches/done.json").is_file());
    assert!(old.join(".branchyard/legacy/events/done.jsonl").is_file());
    drop(yard);
    let reopened = Yard::open(&old).unwrap();
    assert_eq!(reopened.branch("done").unwrap().events().unwrap().len(), 2);
}

/// A second repository in the fixture's directory, with one commit.
fn tempdir(f: &Fixture, name: &str) -> std::path::PathBuf {
    let root = f.dir.join(name);
    fs::create_dir_all(&root).unwrap();
    common::git(&root, &["init", "-q", "-b", "main"]);
    common::git(&root, &["config", "user.name", "Test"]);
    common::git(&root, &["config", "user.email", "test@localhost"]);
    fs::write(root.join("a.txt"), "a\n").unwrap();
    common::git(&root, &["add", "."]);
    common::git(&root, &["commit", "-q", "-m", "initial"]);
    root
}

#[test]
fn an_event_of_a_turn_that_lost_its_lease_is_refused() {
    let f = Fixture::new();
    let turn = {
        let task = f.task("HANG").name("fenced");
        std::thread::spawn(move || task.run())
    };
    wait::until("the prompt", || prompts(&events(&f.yard, "fenced")) == 1);
    // Another engine takes the lease over, as recovery of an expired lease
    // does; the running turn notices at its next heartbeat and stops.
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    db.execute(
        "UPDATE leases SET generation = generation + 1, owner = 'another' WHERE branch = 'fenced'",
        [],
    )
    .unwrap();
    drop(db);
    let result = turn.join().unwrap();
    assert!(
        matches!(&result, Err(Error::Fenced(why)) if why.contains("superseded")),
        "{result:?}"
    );
    // Nothing more was written by the fenced engine.
    let branch = f.yard.branch("fenced").unwrap();
    assert_eq!(branch.info().status, BranchStatus::Running);
    assert!(!branch
        .events()
        .unwrap()
        .iter()
        .any(|e| matches!(e.activity, Activity::Harness(Event::TurnEnded { .. }))));
}

/// A process that has exited: its pid and the start time it had.
fn exited_process() -> (u32, String) {
    let mut gone = Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    (pid, "1".into())
}

#[test]
fn a_name_reserved_by_an_engine_that_stopped_is_reclaimed_by_recovery() {
    let f = Fixture::new();
    let (dead, start) = exited_process();
    // This host's identity, as the engine records it.
    f.task("WRITE r.txt=1").name("probe").run().unwrap();
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    let host: String = db
        .query_row("SELECT host FROM leases WHERE branch = 'probe'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let now = branchyard_support::time::now_ms() as i64;
    let reserve = |name: &str, host: &str, pid: u32, start: &str, at_ms: i64| {
        db.execute(
            "INSERT INTO branches (name, created_ms, record) VALUES (?1, ?2, NULL)",
            rusqlite::params![name, at_ms],
        )
        .unwrap();
        db.execute(
            "INSERT INTO reservations (name, owner, host, pid, pid_start, reserved_ms) \
             VALUES (?1, 'engine', ?2, ?3, ?4, ?5)",
            rusqlite::params![name, host, pid, start, at_ms],
        )
        .unwrap();
    };
    // Its engine died on this host before creating the branch.
    reserve("orphaned", &host, dead, &start, now);
    // A live engine elsewhere, reserved just now: kept.
    reserve("elsewhere", "another-host/boot", 1, "1", now);
    // An engine elsewhere that reserved it an hour ago and never created
    // it: expired.
    reserve("stale", "another-host/boot", 1, "1", now - 3_600_000);
    // An earlier version's reservation names no engine: kept.
    db.execute(
        "INSERT INTO branches (name, created_ms, record) VALUES ('legacy', 1, NULL)",
        [],
    )
    .unwrap();
    drop(db);
    let planned = |yard: &Yard, name: &str| {
        yard.task("x")
            .name(name)
            .planned_names(&[])
            .is_ok_and(|names| names == [name])
    };
    for name in ["orphaned", "elsewhere", "stale", "legacy"] {
        assert!(!planned(&f.yard, name), "{name} is taken before recovery");
    }

    let yard = Yard::open(&f.root).unwrap();
    assert!(planned(&yard, "orphaned"));
    assert!(planned(&yard, "stale"));
    assert!(!planned(&yard, "elsewhere"));
    assert!(!planned(&yard, "legacy"));
    let branch = yard
        .task("WRITE o.txt=1")
        .options(f.options())
        .name("orphaned")
        .run();
    assert_eq!(branch.unwrap().info().status, BranchStatus::Ready);
    // A second recovery changes nothing.
    assert!(yard.recover().unwrap().is_empty());
    assert!(!planned(&yard, "elsewhere"));
}

#[test]
fn a_microsandbox_sandbox_left_by_a_stopped_engine_is_reported_when_it_cannot_be_destroyed() {
    let f = Fixture::new();
    f.task("WRITE s.txt=1").name("boxed").run().unwrap();
    // Stand in for an engine that journaled its Microsandbox sandbox and
    // died mid-turn: the turn's end was never recorded.
    let (dead, _) = exited_process();
    edit_record(&f.root, "boxed", |record| {
        record["info"]["status"] = serde_json::json!({"state": "running"});
        record["provider"] = serde_json::json!({"kind": "microsandbox", "image": "alpine:3.20"});
    });
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    db.execute(
        "UPDATE leases SET owner = 'gone', pid = ?1, expires_ms = 9999999999999",
        [dead],
    )
    .unwrap();
    db.execute(
        "DELETE FROM steps WHERE step IN ('turn_end', 'snapshot', 'submit')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO steps (incarnation, turn, step, branch, generation, intent, started_ms) \
         SELECT incarnation, turn, 'sandbox', branch, generation, \
         '{\"provider\":\"microsandbox\",\"sandbox\":\"by-boxed-1\"}', 0 \
         FROM leases WHERE branch = 'boxed'",
        [],
    )
    .unwrap();
    drop(db);

    let yard = Yard::open(&f.root).unwrap();
    let branch = yard.branch("boxed").unwrap();
    assert_eq!(branch.info().status, BranchStatus::Interrupted);
    let found = recovered(&branch.events().unwrap());
    assert_eq!(found.len(), 1);
    let reason = &found[0].0;
    // This build has no Microsandbox SDK; recovery says what it could not
    // do rather than claiming the sandbox is gone. The destroy itself is
    // tested against a stand-in provider in `placement`.
    if !branchyard_microsandbox::ENABLED {
        assert!(
            reason.contains(
                "could not destroy its Microsandbox sandbox by-boxed-1: this build has no \
                 Microsandbox support"
            ),
            "{reason}"
        );
    }
    assert!(yard.recover().unwrap().is_empty());
}

#[test]
fn a_harness_started_just_before_its_engine_stopped_is_found_by_its_marker() {
    let f = Fixture::new();
    let mut child = start_child(&f, "ORPHAN", &[]);
    let mut pids = Vec::new();
    wait::until("the harness to report its processes", || {
        let said = text(&events(&f.yard, "crashy"));
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
    let (agent, sleeper) = (pids[0], pids[1]);
    child.kill().unwrap();
    child.wait().unwrap();
    // On Linux the harness dies with its engine; what it started in its
    // group does not.
    if cfg!(target_os = "linux") {
        wait::until("the harness to die with its engine", || !running(agent));
    } else {
        assert!(running(agent));
    }
    assert!(running(sleeper));
    // Leave the journal as an engine that stopped between spawning the
    // harness and recording it would have: the start's intent and nothing
    // after it.
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    assert_eq!(db.execute("DELETE FROM processes", []).unwrap(), 1);
    db.execute("UPDATE steps SET outcome = NULL WHERE step = 'start'", [])
        .unwrap();
    db.execute(
        "DELETE FROM steps WHERE step <> 'start' AND step <> 'create'",
        [],
    )
    .unwrap();
    drop(db);

    let yard = Yard::open(&f.root).unwrap();
    let branch = yard.branch("crashy").unwrap();
    assert_eq!(branch.info().status, BranchStatus::Interrupted);
    let found = recovered(&branch.events().unwrap());
    assert_eq!(found.len(), 1);
    let (reason, killed) = &found[0];
    assert!(
        reason.contains("before the prompt was submitted"),
        "{reason}"
    );
    assert!(
        killed.contains(&sleeper) && (cfg!(target_os = "linux") || killed.contains(&agent)),
        "{killed:?}"
    );
    wait::until("the unrecorded harness to die", || {
        !running(agent) && !running(sleeper)
    });
}

/// A child engine process, killed if a test fails before it exits.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn steered(events: &[RecordedEvent]) -> Vec<(u64, String, String)> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Steered { id, by, text } => Some((*id, by.clone(), text.clone())),
            _ => None,
        })
        .collect()
}

/// A turn running in another process takes steered input from this one:
/// the input is queued durably, bound to that turn, delivered by the engine
/// that runs it, recorded with its sender, and the turn ends once.
#[test]
fn input_steered_from_another_process_reaches_the_running_turn() {
    let f = Fixture::new();
    let mut child = Reaped(start_child(&f, "AWAIT_STEER", &[]));
    wait::until("the harness to wait for steering", || {
        text(&events(&f.yard, "crashy")).contains("waiting for steering")
    });
    // Another yard on the repository, as a separate process would open it.
    let other = Yard::open(&f.root).unwrap();
    let steer = other
        .steer_as("crashy", "also look at b.txt", "the other process")
        .unwrap();
    assert_eq!(steer.branch, "crashy");
    assert_eq!(steer.by, "the other process");
    let settled = other
        .wait_steer("crashy", steer.id, Duration::from_secs(30))
        .unwrap();
    assert!(
        matches!(settled.state, SteerState::Written | SteerState::Accepted),
        "{settled:?}"
    );
    assert!(child.0.wait().unwrap().success());

    let branch = other.branch("crashy").unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges);
    let log = branch.events().unwrap();
    assert_eq!(
        steered(&log),
        [(
            steer.id,
            "the other process".to_owned(),
            "also look at b.txt".to_owned()
        )]
    );
    assert!(text(&log).contains("steered: also look at b.txt"));
    assert!(log
        .iter()
        .any(|e| matches!(e.activity, Activity::Harness(Event::SteerAccepted { .. }))));
    let ends = log
        .iter()
        .filter(|e| matches!(e.activity, Activity::Harness(Event::TurnEnded { .. })))
        .count();
    assert_eq!(ends, 1);
    assert_eq!(
        other.steer_state("crashy", steer.id).unwrap().state,
        SteerState::Accepted
    );
    // Bound to that turn: with none running, steering is refused.
    assert!(matches!(
        branch.steer("too late"),
        Err(Error::NotRunning(name)) if name == "crashy"
    ));
}

/// An agent that does not offer steering is never interrupted in its
/// place: the input is refused with the reason, and the turn goes on.
#[test]
fn steering_an_agent_without_the_extension_is_refused_not_an_interrupt() {
    let f = Fixture::new();
    let mut child = Reaped(start_child(&f, "HANG", &[("FAKE_ACP_NO_STEER", "1")]));
    wait::until("the prompt", || prompts(&events(&f.yard, "crashy")) == 1);
    let steer = f.yard.steer_as("crashy", "hello", "a test").unwrap();
    let settled = f
        .yard
        .wait_steer("crashy", steer.id, Duration::from_secs(30))
        .unwrap();
    assert!(
        matches!(&settled.state, SteerState::Refused { reason } if reason.contains("_session/steering")),
        "{settled:?}"
    );
    // Still running: the refusal did not stop the turn.
    assert_eq!(
        f.yard.branch("crashy").unwrap().info().status,
        BranchStatus::Running
    );
    assert_eq!(f.yard.cancel_as("crashy", "the test").unwrap(), ["crashy"]);
    assert!(child.0.wait().unwrap().success());
    let log = events(&f.yard, "crashy");
    assert!(steered(&log).is_empty());
    assert!(log.iter().any(|e| matches!(&e.activity,
        Activity::Warning(w) if w.starts_with(&format!("steered input {} from a test was not delivered", steer.id)))));
    assert!(log.iter().any(|e| matches!(
        e.activity,
        Activity::Harness(Event::TurnEnded {
            outcome: TurnOutcome::Interrupted,
            ..
        })
    )));
}

/// A profile that cannot take input mid-turn is refused before anything
/// is queued, with its driver's reason; so is empty input.
#[test]
fn a_profile_without_steering_is_refused_up_front() {
    let f = Fixture::new();
    let branch = f.task("hello").name("quiet").run().unwrap();
    assert!(matches!(
        branch.steer(" "),
        Err(Error::Denied(why)) if why.contains("needs some text")
    ));
    assert!(matches!(
        branch.steer("x"),
        Err(Error::NotRunning(name)) if name == "quiet"
    ));
    edit_record(&f.root, "quiet", |record| {
        record["info"]["harness"] = "amp".into();
        record["info"]["profile"] = "amp-stream-json".into();
    });
    let error = f.yard.branch("quiet").unwrap().steer("x").unwrap_err();
    assert_eq!(error.kind(), "unsupported");
    assert!(
        error
            .to_string()
            .contains("amp-stream-json cannot take input during a running turn"),
        "{error}"
    );
}
