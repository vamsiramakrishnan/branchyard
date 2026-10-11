//! The engine's side of a branch's lifecycle, against the fake ACP agent:
//! when a branch last ended a turn, parking it (idle or dormant) while it
//! releases what it holds, starting its session over in place, and
//! removing it with a record that outlives it in the feed. Hermetic.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::sync::Arc;

use branchyard::{
    Activity, Branch, BranchStatus, Error, LifecycleState, Policy, Provider, RecordedEvent,
    SandboxKeep, SandboxOptions, TaskOptions,
};
use branchyard_runtime::LocalProvider;
use branchyard_sandbox::fake::FakeProvider;
use branchyard_sandbox::SandboxState;
use branchyard_testkit::wait;
use common::{text, Fixture};

fn send(branch: &Branch, f: &Fixture, prompt: &str) -> Branch {
    let options = TaskOptions {
        policy: Policy::allow_all(),
        ..f.options()
    };
    branch.send(prompt, options).unwrap()
}

/// Every lifecycle event of the branch, in order.
fn lifecycle(events: &[RecordedEvent]) -> Vec<(LifecycleState, String, u64)> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Lifecycle {
                state,
                reason,
                idle_since_ms,
            } => Some((*state, reason.clone(), *idle_since_ms)),
            _ => None,
        })
        .collect()
}

/// When the log says the branch's last turn ended: its last status event.
fn last_status_ms(events: &[RecordedEvent]) -> u64 {
    events
        .iter()
        .rev()
        .find(|e| matches!(e.activity, Activity::Status(_)))
        .map(|e| e.at_ms)
        .unwrap()
}

fn reply(branch: &Branch) -> String {
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

#[test]
fn a_branch_knows_when_it_last_ended_a_turn_and_where_it_rests() {
    let f = Fixture::new();
    let branch = f
        .task("WRITE a.txt=1")
        .name("assistant")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    let resting = f.yard.resting("assistant").unwrap();
    assert_eq!(resting.status, BranchStatus::Ready);
    assert_eq!(resting.last_turn_end_ms, last_status_ms(&events));
    assert_eq!(resting.state, None);
    assert!(!resting.engaged() && !resting.finished());
    assert!(matches!(
        f.yard.resting("nobody"),
        Err(Error::UnknownBranch(_))
    ));
}

#[test]
fn parking_a_branch_that_holds_nothing_records_the_event_anyway() {
    let f = Fixture::new();
    let branch = f
        .task("WHOAMI")
        .name("assistant")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let before = f.yard.resting("assistant").unwrap();
    // Not a state a branch is parked in.
    assert!(matches!(
        f.yard
            .park("assistant", LifecycleState::Deleted, "no turn for 30m"),
        Err(Error::Denied(_))
    ));
    let parked = f
        .yard
        .park("assistant", LifecycleState::Idle, "no turn for 30m")
        .unwrap();
    assert!(parked.released.is_empty(), "{parked:?}");
    let events = branch.events().unwrap();
    let said = lifecycle(&events);
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0].0, LifecycleState::Idle);
    assert_eq!(said[0].1, "no turn for 30m; it held nothing to release");
    assert_eq!(said[0].2, before.last_turn_end_ms);
    // Parking is not a turn: the log's last turn end, and the status, stand.
    let after = f.yard.resting("assistant").unwrap();
    assert_eq!(after.last_turn_end_ms, before.last_turn_end_ms);
    assert_eq!(after.state, Some(LifecycleState::Idle));
    assert_eq!(after.status, before.status);
    // A turn after it ends the rest.
    let again = send(&branch, &f, "WHOAMI");
    assert!(reply(&again).contains("resumed=true"), "{}", reply(&again));
    let ended = f.yard.resting("assistant").unwrap();
    assert_eq!(ended.state, None);
    assert!(ended.last_turn_end_ms >= before.last_turn_end_ms);
    assert_eq!(again.info().turns, 2);
}

#[test]
fn an_idle_branch_gives_up_its_kept_sandbox_and_its_next_turn_gets_a_fresh_one() {
    let f = Fixture::new();
    let root = f.dir.join("fake-provider");
    let fake = Arc::new(FakeProvider::live(Box::new(LocalProvider::new()), root));
    f.yard.use_sandbox_provider(fake.clone());
    let kept = TaskOptions {
        provider: Some(Provider::Microsandbox(SandboxOptions {
            image: "registry.example/harness:1".into(),
            keep: SandboxKeep::Pause,
            snapshots: Some(0),
            ..SandboxOptions::default()
        })),
        policy: Policy::allow_all(),
        ..f.options()
    };
    let branch = f
        .yard
        .task("WRITE a.txt=1")
        .options(kept.clone())
        .name("warm")
        .run()
        .unwrap();
    assert_eq!(paused(&fake).len(), 1, "the turn kept its sandbox");
    let parked = f
        .yard
        .park("warm", LifecycleState::Idle, "no turn for 30m")
        .unwrap();
    assert_eq!(parked.released.len(), 1, "{parked:?}");
    assert!(
        parked.released[0].contains("the branch went idle"),
        "{parked:?}"
    );
    assert!(paused(&fake).is_empty(), "{:?}", fake.sandboxes());
    let events = branch.events().unwrap();
    let said = lifecycle(&events);
    assert_eq!(said.len(), 1);
    assert!(
        said[0]
            .1
            .starts_with("no turn for 30m; sandbox: not kept (the branch went idle)"),
        "{said:?}"
    );
    // The branch continues as before, in a fresh sandbox, and keeps that.
    let next = branch.send("WHOAMI", kept).unwrap();
    assert!(reply(&next).contains("resumed=true"), "{}", reply(&next));
    assert_eq!(next.info().turns, 2);
    assert_eq!(paused(&fake).len(), 1);
}

#[test]
fn a_branch_that_runs_a_turn_is_not_resting_and_cannot_be_parked() {
    let f = Fixture::new();
    let branch = f
        .task("WRITE a.txt=1")
        .name("busy")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let running = {
        let branch = branch.clone();
        let options = TaskOptions {
            policy: Policy::allow_all(),
            ..f.options()
        };
        std::thread::spawn(move || branch.send("AWAIT_STEER", options))
    };
    wait::until("the turn to run", || {
        f.yard.branch("busy").unwrap().info().status == BranchStatus::Running
    });
    assert!(f.yard.resting("busy").unwrap().engaged());
    assert!(matches!(
        f.yard.park("busy", LifecycleState::Idle, "no turn"),
        Err(Error::Running(_))
    ));
    assert!(matches!(
        f.yard
            .branch("busy")
            .unwrap()
            .reincarnate_in_place("no turn"),
        Err(Error::Running(_))
    ));
    assert!(matches!(
        f.yard.retire("busy", "no turn"),
        Err(Error::Running(_))
    ));
    f.yard.branch("busy").unwrap().steer("go on").unwrap();
    running.join().unwrap().unwrap();
    assert!(lifecycle(&branch.events().unwrap()).is_empty());
    assert!(!f.yard.resting("busy").unwrap().engaged());
}

#[test]
fn a_reincarnated_branch_keeps_its_name_and_worktree_and_starts_a_fresh_session() {
    let f = Fixture::new();
    let branch = f
        .task("keep the notes")
        .name("assistant")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let branch = send(&branch, &f, "add a second note");
    let old = f.yard.branch("assistant").unwrap().info().clone();
    assert_eq!((old.turns, old.superseded_by.as_deref()), (2, None));
    assert!(old.session.is_some());
    f.yard
        .park("assistant", LifecycleState::Dormant, "no turn for 7d")
        .unwrap();
    assert_eq!(
        f.yard.resting("assistant").unwrap().state,
        Some(LifecycleState::Dormant)
    );
    branch
        .reincarnate_in_place("it was dormant for 7d")
        .unwrap();
    // Not a new branch: the same one, with its session retired.
    assert_eq!(f.yard.branches().unwrap().len(), 1);
    let retired = f.yard.branch("assistant").unwrap().info().clone();
    assert_eq!(retired.session, None);
    assert_eq!(retired.worktree, old.worktree);
    assert_eq!(retired.prompt, old.prompt);
    assert_eq!(retired.superseded_by, None);
    // A reincarnation is not rest: the clock restarts from it.
    let resting = f.yard.resting("assistant").unwrap();
    assert_eq!(resting.state, None);
    let events = branch.events().unwrap();
    let said = lifecycle(&events);
    assert_eq!(
        said.iter().map(|s| s.0).collect::<Vec<_>>(),
        [LifecycleState::Dormant, LifecycleState::Reincarnated]
    );
    assert!(said[1].1.starts_with("it was dormant for 7d"), "{said:?}");
    // The next send starts a fresh session whose prompt begins with the
    // handoff, then runs the prompt it was sent.
    let next = send(&branch, &f, "WHOAMI again");
    let text = reply(&next);
    assert!(text.contains("resumed=false"), "{text}");
    let events = next.events().unwrap();
    let prompt = events
        .iter()
        .rev()
        .find_map(|e| match &e.activity {
            Activity::Prompt(p) => Some(p.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        prompt.contains("This continues the work of branch assistant"),
        "{prompt}"
    );
    assert!(
        prompt.contains("### The original task\nkeep the notes"),
        "{prompt}"
    );
    assert!(prompt.contains("Asked: add a second note"), "{prompt}");
    assert!(
        prompt.ends_with("## Your task now\nWHOAMI again"),
        "{prompt}"
    );
    assert_eq!(next.info().turns, 3);
    // The turn ended after the reincarnation, so the branch is awake.
    let after = f.yard.resting("assistant").unwrap();
    assert_eq!(after.state, None);
    assert!(after.last_turn_end_ms >= resting.last_turn_end_ms);
    // And continues that session from then on, without the handoff.
    let more = send(&next, &f, "WHOAMI");
    assert!(reply(&more).contains("resumed=true"), "{}", reply(&more));
    let events = more.events().unwrap();
    let prompt = events
        .iter()
        .rev()
        .find_map(|e| match &e.activity {
            Activity::Prompt(p) => Some(p.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(prompt, "WHOAMI");
}

#[test]
fn a_retired_branch_is_removed_and_its_last_word_stays_in_the_feed() {
    let f = Fixture::new();
    let branch = f
        .task("WRITE a.txt=1")
        .name("assistant")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let idle_since = f.yard.resting("assistant").unwrap().last_turn_end_ms;
    f.yard.retire("assistant", "no turn for 30d").unwrap();
    assert!(matches!(
        f.yard.branch("assistant"),
        Err(Error::UnknownBranch(_))
    ));
    assert!(!branch.info().worktree.exists());
    // The repository's feed still has it: the branch's log is gone, the
    // feed is the record of what happened to the repository.
    let page = f.yard.events_since(0, 1000).unwrap();
    let deleted: Vec<_> = page
        .events
        .iter()
        .filter(|e| e.branch == "assistant")
        .filter_map(|e| match &e.event.activity {
            Activity::Lifecycle {
                state,
                idle_since_ms,
                ..
            } => Some((*state, *idle_since_ms)),
            _ => None,
        })
        .collect();
    assert_eq!(deleted, [(LifecycleState::Deleted, idle_since)]);
    // The name is free: a run creates the branch again from its task.
    let again = f
        .task("WRITE a.txt=2")
        .name("assistant")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    assert_eq!(again.info().turns, 1);
    assert!(lifecycle(&again.events().unwrap()).is_empty());
}
