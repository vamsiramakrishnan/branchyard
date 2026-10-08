//! Amp driver against transcripts derived from Amp's streaming JSON
//! documentation. None is recorded: Amp prints no protocol line without an
//! account and its server.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
use branchyard_harness::amp::Amp;
use branchyard_harness::conformance::{decode, feed, Replay, Transcript};
use branchyard_harness::{
    Driver, Event, NativeSession, Open, Opened, PermissionDecision, PermissionKey, Rejected,
    SessionMode, TurnOutcome, Usage,
};
use serde_json::{json, Value};

fn fixture(name: &str) -> Transcript {
    Transcript::load(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
}

fn fresh() -> Open {
    Open::new(SessionMode::Fresh, "/workspace")
}

fn session(id: &str) -> NativeSession {
    NativeSession::new(id).unwrap()
}

fn open(mode: SessionMode) -> (Amp, Opened) {
    let mut driver = Amp::new(vec!["amp".into()]);
    let opened = driver.open(Open { mode, ..fresh() }).unwrap();
    (driver, opened)
}

fn init(id: &str) -> Value {
    json!({"type": "system", "subtype": "init", "cwd": "/workspace", "session_id": id, "tools": [], "mcp_servers": []})
}

fn ready() -> Amp {
    let (mut driver, _) = open(SessionMode::Fresh);
    let (events, _) = feed(&mut driver, &init("T-1"));
    assert_eq!(events[0], Event::Ready);
    driver
}

fn assistant(content: Value, stop: Option<&str>) -> Value {
    json!({"type": "assistant", "message": {"type": "message", "role": "assistant", "content": content,
        "stop_reason": stop}, "parent_tool_use_id": null, "session_id": "T-1"})
}

#[test]
fn replays_the_documented_multi_turn_input_session() {
    let (mut driver, opened) = open(SessionMode::Fresh);
    assert_eq!(
        opened.launch.argv,
        ["amp", "--execute", "--stream-json", "--stream-json-input"]
    );
    assert!(opened.frames.is_empty(), "Amp speaks first");
    let replayed = Replay::new(&fixture("amp-docs-20260927-stream-json-input.jsonl"))
        .prompt("what's 2+2?")
        .prompt("now add 8 to that")
        .prompt("now add 5 to that")
        .run(&mut driver, &opened);
    assert_eq!(replayed.sent, 3);
    let events = replayed.events;
    assert_eq!(
        events[1],
        Event::SessionStarted {
            session: session("T-addfb7a4-61d9-41e1-890b-7330aa54087a"),
            forked_from: None
        }
    );
    let ended: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::TurnEnded { .. }))
        .collect();
    assert_eq!(
        ended.len(),
        3,
        "one per end_turn; the final result ends none"
    );
    assert_eq!(
        events[2..6],
        [
            Event::TurnAccepted {
                turn: 1,
                native: None
            },
            Event::MessageDelta {
                turn: 1,
                text: "4".into()
            },
            Event::UsageObserved {
                turn: Some(1),
                usage: Usage {
                    cumulative: false,
                    input_tokens: Some(10),
                    output_tokens: Some(67),
                    cached_input_tokens: Some(0),
                    cost_usd: None,
                    ..Usage::default()
                }
            },
            Event::TurnEnded {
                turn: 1,
                outcome: TurnOutcome::Completed
            },
        ]
    );
    assert_eq!(
        events.last(),
        Some(&Event::TurnEnded {
            turn: 3,
            outcome: TurnOutcome::Completed
        })
    );
}

#[test]
fn replays_the_documented_tool_turn() {
    let (mut driver, opened) = open(SessionMode::Fresh);
    let replayed = Replay::new(&fixture("amp-docs-20260927-stream-json-tools.jsonl"))
        .prompt("list files using a tool")
        .run(&mut driver, &opened);
    assert_eq!(replayed.sent, 1);
    let events = replayed.events;
    assert!(events.contains(&Event::ToolStarted {
        turn: 1,
        call_id: "toolu_019cyniPYrSgaJitUSMyxyNV".into(),
        name: "read".into()
    }));
    let accepted = events
        .iter()
        .filter(|e| matches!(e, Event::TurnAccepted { .. }))
        .count();
    assert_eq!(accepted, 1, "a tool_result user message is not an echo");
    let ends: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::TurnEnded { .. }))
        .collect();
    assert_eq!(
        ends,
        [&Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Completed
        }],
        "tool_use does not end the turn"
    );
}

#[test]
fn prompts_use_the_documented_input_shape() {
    let mut driver = ready();
    let submitted = driver.submit("hello").unwrap();
    assert_eq!(
        decode(&submitted.frames[0]),
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "hello"}]}})
    );
}

#[test]
fn stop_reasons_and_errors_map_to_outcomes() {
    for (stop, outcome) in [
        (
            "max_tokens",
            TurnOutcome::LimitReached {
                limit: "max_tokens".into(),
            },
        ),
        ("refusal", TurnOutcome::Refused),
        ("stop_sequence", TurnOutcome::Completed),
    ] {
        let mut driver = ready();
        driver.submit("go").unwrap();
        let (events, _) = feed(&mut driver, &assistant(json!([]), Some(stop)));
        assert_eq!(events, vec![Event::TurnEnded { turn: 1, outcome }]);
    }

    let mut driver = ready();
    driver.submit("go").unwrap();
    for stop in [Some("tool_use"), Some("pause_turn"), None] {
        let (events, _) = feed(&mut driver, &assistant(json!([]), stop));
        assert!(events.is_empty(), "{stop:?}");
    }
    // A subagent's end_turn does not end the parent's turn.
    let mut subagent = assistant(json!([{"type": "text", "text": "sub"}]), Some("end_turn"));
    subagent["parent_tool_use_id"] = json!("toolu_1");
    assert!(feed(&mut driver, &subagent).0.is_empty());
    let (events, _) = feed(
        &mut driver,
        &json!({"type": "system", "subtype": "error_max_turns", "error": "too many turns", "session_id": "T-1"}),
    );
    assert_eq!(
        events,
        vec![Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::LimitReached {
                limit: "max_turns".into()
            }
        }]
    );
    driver.submit("again").unwrap();
    let (events, _) = feed(
        &mut driver,
        &json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "num_turns": 1,
            "error": "provider failed", "session_id": "T-1", "duration_ms": 1}),
    );
    assert_eq!(
        events,
        vec![Event::TurnEnded {
            turn: 2,
            outcome: TurnOutcome::Failed {
                message: "provider failed".into()
            }
        }]
    );
}

#[test]
fn resume_continues_the_thread_and_checks_it() {
    let (mut driver, opened) = open(SessionMode::Resume(session("T-old")));
    assert_eq!(
        opened.launch.argv,
        [
            "amp",
            "threads",
            "continue",
            "T-old",
            "--execute",
            "--stream-json",
            "--stream-json-input"
        ]
    );
    let (events, _) = feed(&mut driver, &init("T-new"));
    assert!(
        matches!(&events[..], [Event::OpenFailed { reason }] if reason.contains("resume of T-old"))
    );
    assert_eq!(driver.submit("go"), Err(Rejected::NotReady));

    let (mut driver, _) = open(SessionMode::Resume(session("T-old")));
    let (events, _) = feed(&mut driver, &init("T-old"));
    assert_eq!(events[0], Event::Ready);
    assert!(feed(&mut driver, &init("T-old")).0.is_empty());
    assert!(matches!(
        &feed(&mut driver, &init("T-other")).0[..],
        [Event::ProtocolViolation { .. }]
    ));
}

#[test]
fn fork_model_cancellation_and_permissions_are_unsupported() {
    let mut driver = Amp::new(vec!["amp".into()]);
    assert!(matches!(
        driver.open(Open {
            mode: SessionMode::Fork(session("T-1")),
            ..fresh()
        }),
        Err(Rejected::Unsupported(_))
    ));
    assert!(matches!(
        driver.open(Open {
            model: Some("claude".into()),
            ..fresh()
        }),
        Err(Rejected::Unsupported(_))
    ));
    let capabilities = driver.capabilities();
    assert!(capabilities.resume && !capabilities.fork);
    assert!(!capabilities.cancellation && !capabilities.tool_approvals);

    let mut driver = ready();
    assert_eq!(driver.interrupt(), Err(Rejected::NoTurn));
    driver.submit("go").unwrap();
    assert!(matches!(driver.interrupt(), Err(Rejected::Unsupported(_))));
    assert_eq!(
        driver.respond(&PermissionKey("1".into()), PermissionDecision::Allow),
        Err(Rejected::UnknownPermission)
    );
    assert!(matches!(
        driver.transport_closed()[..],
        [Event::OutcomeUnknown { turn: 1, .. }, Event::SessionClosed]
    ));
}

/// Recorded from amp 0.0.1790467310-ge147a9 with an isolated home and no
/// credentials: it starts a device login on stdout (code redacted) instead
/// of the protocol. With a dummy key and no reachable server it exits 1
/// with nothing on stdout.
#[test]
fn a_login_flow_on_stdout_is_a_violation() {
    let (mut driver, _) = open(SessionMode::Fresh);
    let output = driver.receive(b"No API key found. Starting login flow...\n");
    assert!(matches!(
        output.events[..],
        [Event::ProtocolViolation { .. }]
    ));
    assert!(
        driver
            .receive(b"https://ampcode.com/auth/cli/device?user_code=REDACTED\n")
            .events
            .len()
            == 1
    );
    assert_eq!(driver.transport_closed(), vec![Event::SessionClosed]);
}
