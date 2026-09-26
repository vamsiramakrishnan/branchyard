//! Session lifecycle against the fake ACP agent.

#![cfg(unix)]

mod common;

use std::time::Duration;

use branchyard_harness::{
    Event, NativeSession, PermissionDecision, PermissionRequest, Rejected, SessionMode, TurnOutcome,
};
use branchyard_runtime::{Environment, RuntimeError, Session};
use common::{alive, background_pid, open, start, start_with, workdir, WAIT};
use serde_json::Value;

fn allow(_: &PermissionRequest) -> PermissionDecision {
    PermissionDecision::Allow
}

fn deny(_: &PermissionRequest) -> PermissionDecision {
    PermissionDecision::Deny {
        message: "no".into(),
    }
}

fn is_turn_end(turn: u64) -> impl Fn(&Event) -> bool {
    move |e| matches!(e, Event::TurnEnded { turn: t, .. } if *t == turn)
}

#[test]
fn a_fresh_turn_returns_its_text_and_the_transcript_records_both_directions() {
    let (mut session, dir) = start("fresh");
    assert_eq!(
        session.session_id(),
        NativeSession::new("fake-session-1").as_ref()
    );
    let report = session.run_turn("hello", &mut allow, WAIT).unwrap();
    assert_eq!(report.turn, 1);
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.text, "echo: hello");
    assert_eq!(report.usage, None, "ACP reports no usage");
    assert!(matches!(
        report.events.last(),
        Some(Event::TurnEnded { .. })
    ));
    assert!(matches!(
        report.events[..],
        [Event::MessageDelta { .. }, Event::TurnEnded { .. }]
    ));

    let second = session.run_turn("again", &mut allow, WAIT).unwrap();
    assert_eq!((second.turn, second.text.as_str()), (2, "echo: again"));

    let closed = session.close(WAIT).unwrap();
    assert!(!closed.forced);
    assert!(closed.survivors.is_empty(), "{:?}", closed.survivors);
    assert_eq!(closed.cost_usd, None);
    assert_eq!(closed.events.last(), Some(&Event::SessionClosed));

    let transcript = std::fs::read_to_string(dir.join("transcript.jsonl")).unwrap();
    let entries: Vec<Value> = transcript
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(entries[0]["dir"], "out");
    assert!(entries[0]["line"].as_str().unwrap().contains("initialize"));
    assert!(entries
        .iter()
        .any(|e| e["dir"] == "in" && e["line"].as_str().unwrap().contains("echo: again")));
}

#[test]
fn the_policy_answers_permission_requests() {
    let (mut session, _) = start("permission");
    let mut asked = Vec::new();
    let report = session
        .run_turn(
            "PERMISSION please",
            &mut |request| {
                asked.push(request.tool.clone());
                PermissionDecision::Allow
            },
            WAIT,
        )
        .unwrap();
    assert_eq!(asked, ["write marker"]);
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.text, "allowed");
    assert!(matches!(
        report.events[0],
        Event::PermissionRequested { turn: Some(1), .. }
    ));

    let report = session.run_turn("PERMISSION", &mut deny, WAIT).unwrap();
    assert_eq!(report.text, "denied");
    session.close(WAIT).unwrap();
}

#[test]
fn an_interrupt_ends_the_turn_as_interrupted() {
    let (mut session, _) = start("interrupt");
    let turn = session.submit("HANG").unwrap();
    session.interrupt().unwrap();
    let ended = session.wait_for(WAIT, is_turn_end(turn)).unwrap();
    assert_eq!(
        ended,
        Event::TurnEnded {
            turn,
            outcome: TurnOutcome::Interrupted
        }
    );

    // During a permission wait, the pending request is answered as cancelled.
    let turn = session.submit("PERMISSION").unwrap();
    session
        .wait_for(WAIT, |e| matches!(e, Event::PermissionRequested { .. }))
        .unwrap();
    session.interrupt().unwrap();
    let ended = session.wait_for(WAIT, is_turn_end(turn)).unwrap();
    assert!(matches!(
        ended,
        Event::TurnEnded {
            outcome: TurnOutcome::Interrupted,
            ..
        }
    ));
    assert!(matches!(
        session.interrupt(),
        Err(RuntimeError::Rejected(Rejected::NoTurn))
    ));
    session.close(WAIT).unwrap();
}

#[test]
fn close_names_and_kills_descendants_that_outlive_the_harness() {
    let (mut session, _) = start("survivors");
    let report = session.run_turn("BACKGROUND", &mut allow, WAIT).unwrap();
    let pid = background_pid(&report.text);
    assert!(alive(pid));
    let closed = session.close(WAIT).unwrap();
    assert!(!closed.forced);
    assert_eq!(closed.survivors, ["sleep"]);
    assert!(!alive(pid), "the survivor is still running");
}

#[test]
fn dropping_a_session_kills_its_process_group() {
    let (mut session, _) = start("drop");
    let report = session.run_turn("BACKGROUND", &mut allow, WAIT).unwrap();
    let pid = background_pid(&report.text);
    drop(session);
    assert!(!alive(pid), "the descendant is still running");
}

#[test]
fn killing_mid_turn_reports_an_unknown_outcome() {
    let (mut session, _) = start("kill");
    let turn = session.submit("HANG").unwrap();
    let events = session.kill().unwrap();
    assert!(
        matches!(events[..], [Event::OutcomeUnknown { turn: t, .. }, Event::SessionClosed] if t == turn),
        "{events:?}"
    );
}

#[test]
fn a_harness_that_exits_mid_turn_surfaces_its_stderr() {
    let (mut session, _) = start("exit");
    let error = session.run_turn("EXIT", &mut allow, WAIT).unwrap_err();
    let RuntimeError::HarnessExited { stderr } = &error else {
        panic!("{error}");
    };
    assert!(stderr.contains("exiting mid-turn"), "{stderr:?}");
    assert!(error.to_string().starts_with("harness exited: "));
    let events = session.events();
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::OutcomeUnknown { turn: 1, .. })));
    assert_eq!(events.last(), Some(&Event::SessionClosed));
    // Every event has been delivered; further reads report the exit again.
    assert!(matches!(
        session.next_event(Duration::ZERO),
        Err(RuntimeError::HarnessExited { .. })
    ));
    let closed = session.close(WAIT).unwrap();
    assert!(closed.events.is_empty());
}

#[test]
fn non_json_output_is_a_protocol_violation_event() {
    let (mut session, _) = start("garbage");
    let report = session.run_turn("GARBAGE", &mut allow, WAIT).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert!(report
        .events
        .iter()
        .any(|e| matches!(e, Event::ProtocolViolation { .. })));
    session.close(WAIT).unwrap();
}

#[test]
fn timeouts_leave_the_session_usable() {
    let (mut session, _) = start("timeout");
    // Left over from the handshake batch that `wait_ready` stopped in.
    assert!(matches!(
        session.next_event(Duration::ZERO).unwrap(),
        Some(Event::SessionStarted { .. })
    ));
    assert_eq!(
        session.next_event(Duration::from_millis(100)).unwrap(),
        None
    );
    let turn = session.submit("HANG").unwrap();
    let error = session
        .wait_for(Duration::from_millis(200), is_turn_end(turn))
        .unwrap_err();
    assert!(matches!(error, RuntimeError::Timeout(_)), "{error}");
    let error = session
        .run_turn("again", &mut allow, Duration::from_millis(200))
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::Rejected(Rejected::TurnInProgress)),
        "{error}"
    );
    session.interrupt().unwrap();
    session.wait_for(WAIT, is_turn_end(turn)).unwrap();
    session.close(WAIT).unwrap();
}

#[test]
fn resume_keeps_the_native_session() {
    let dir = workdir("resume");
    let prior = NativeSession::new("prior-7").unwrap();
    let mut session = start_with(
        &dir,
        SessionMode::Resume(prior.clone()),
        &Environment::new(dir.join("home")),
    );
    assert_eq!(session.session_id(), Some(&prior));
    let report = session.run_turn("hi", &mut allow, WAIT).unwrap();
    assert_eq!(report.text, "echo: hi");
    session.close(WAIT).unwrap();
}

#[test]
fn launch_and_open_failures_are_errors() {
    let dir = workdir("failures");
    let env = Environment::new(dir.join("home"));
    let missing =
        branchyard_harness::acp::Acp::new(vec![dir.join("no-such-agent").display().to_string()]);
    let error = Session::start(
        Box::new(missing),
        open(&dir, SessionMode::Fresh),
        &env,
        None,
    )
    .unwrap_err();
    assert!(matches!(error, RuntimeError::Spawn { .. }), "{error}");

    let fork = SessionMode::Fork(NativeSession::new("parent").unwrap());
    let error = Session::start(common::agent(), open(&dir, fork), &env, None).unwrap_err();
    assert!(
        matches!(error, RuntimeError::Rejected(Rejected::Unsupported(_))),
        "{error}"
    );

    let mut session =
        Session::start(common::agent(), open(&dir, SessionMode::Fresh), &env, None).unwrap();
    assert!(matches!(
        session.submit("too early"),
        Err(RuntimeError::Rejected(Rejected::NotReady))
    ));
    session.wait_ready(WAIT).unwrap();
    // Ready and SessionStarted were one batch; both were recorded.
    assert!(matches!(
        session.events(),
        [Event::Ready, Event::SessionStarted { .. }]
    ));
    assert!(matches!(
        session.next_event(Duration::ZERO).unwrap(),
        Some(Event::SessionStarted { .. })
    ));
    session.kill().unwrap();
}
