//! Pi driver against RPC transcripts recorded from pi 0.87.1 without a
//! model call, and against the RPC documentation and `rpc-mode.js`.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
use branchyard_harness::conformance::{decode, feed, handshake, Replay, Transcript};
use branchyard_harness::pi::Pi;
use branchyard_harness::{
    Driver, Event, NativeSession, Open, Opened, PermissionDecision, PermissionKey, Rejected,
    SessionMode, TurnOutcome, Usage,
};
use serde_json::{json, Value};

const TURN_SESSION: &str = "01a0e06b-dd5c-72ea-b1ed-34c97e06d4cd";

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

fn open_with(open: Open) -> (Pi, Opened) {
    let mut driver = Pi::new(vec!["pi".into()]);
    let opened = driver.open(open).unwrap();
    (driver, opened)
}

fn open(mode: SessionMode) -> (Pi, Opened) {
    open_with(Open { mode, ..fresh() })
}

/// Complete the handshake with `session_id` and return the driver.
fn ready(mode: SessionMode, session_id: &str) -> (Pi, Vec<Event>) {
    let (mut driver, opened) = open(mode);
    let events = handshake(&mut driver, &opened.frames, |frame| {
        vec![
            json!({"id": frame["id"], "type": "response", "command": "get_state",
            "success": true, "data": {"sessionId": session_id}}),
        ]
    });
    (driver, events)
}

fn response(id: &Value, command: &str) -> Value {
    json!({"id": id, "type": "response", "command": command, "success": true})
}

fn assistant(stop: &str, error: Option<&str>) -> Value {
    let mut message = json!({"role": "assistant", "content": [], "stopReason": stop,
        "usage": {"input": 12, "output": 3, "cacheRead": 5, "cacheWrite": 0, "totalTokens": 20,
            "cost": {"input": 0.1, "output": 0.2, "cacheRead": 0, "cacheWrite": 0, "total": 0.3}}});
    if let Some(error) = error {
        message["errorMessage"] = json!(error);
    }
    json!({"type": "message_end", "message": message})
}

#[test]
fn replays_a_recorded_turn_with_retries() {
    let (mut driver, opened) = open_with(Open {
        model: Some("dead/none".into()),
        ..fresh()
    });
    assert_eq!(
        opened.launch.argv,
        ["pi", "--mode", "rpc", "--model", "dead/none"]
    );
    let replayed = Replay::new(&fixture("pi-0.87.1-rpc-turn.jsonl"))
        .prompt("Say hello.")
        .run(&mut driver, &opened);
    assert_eq!(replayed.sent, 2, "get_state and prompt");
    assert!(replayed.unsent.is_empty());
    let events = replayed.events;
    assert_eq!(events[0], Event::Ready);
    assert_eq!(
        events[1],
        Event::SessionStarted {
            session: session(TURN_SESSION),
            forked_from: None
        }
    );
    assert_eq!(
        events[2],
        Event::TurnAccepted {
            turn: 1,
            native: Some("branchyard-2".into())
        }
    );
    let retries = events
        .iter()
        .filter(|e| matches!(e, Event::Warning { message } if message.starts_with("retry")))
        .count();
    assert_eq!(retries, 3);
    let usage: Vec<&Usage> = events
        .iter()
        .filter_map(|e| match e {
            Event::UsageObserved { usage, .. } => Some(usage),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 4, "one per assistant response");
    assert!(usage
        .iter()
        .all(|u| !u.cumulative && u.input_tokens == Some(0)));
    assert_eq!(
        events.last(),
        Some(&Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Failed {
                message: "Connection error.".into()
            }
        })
    );
    assert!(!events.iter().any(|e| matches!(
        e,
        Event::ProtocolViolation { .. } | Event::Unrecognized { .. }
    )));
}

#[test]
fn replays_a_recorded_abort_settling_before_its_response() {
    let (mut driver, opened) = open(SessionMode::Fresh);
    let transcript = fixture("pi-0.87.1-rpc-abort.jsonl");
    // The abort is written by `interrupt`, not by a prompt, so replay the
    // exchange up to it, interrupt, then replay the rest.
    let split = transcript
        .rows
        .iter()
        .position(|r| r.frame["type"] == "abort")
        .unwrap();
    let before = Transcript {
        note: None,
        rows: transcript.rows[..split].to_vec(),
    };
    let replayed = Replay::new(&before)
        .prompt("Say hello.")
        .run(&mut driver, &opened);
    assert_eq!(replayed.sent, 2);
    let abort = decode(&driver.interrupt().unwrap()[0]);
    assert_eq!(abort, transcript.rows[split].frame);
    let after: Vec<Event> = transcript.rows[split + 1..]
        .iter()
        .flat_map(|r| feed(&mut driver, &r.frame).0)
        .collect();
    assert_eq!(
        after,
        vec![
            Event::TurnEnded {
                turn: 1,
                outcome: TurnOutcome::Interrupted
            },
            Event::InterruptAcknowledged { turn: 1 },
        ]
    );
}

#[test]
fn resume_and_fork_check_the_recorded_session() {
    let (mut driver, opened) = open(SessionMode::Resume(session(TURN_SESSION)));
    assert_eq!(opened.launch.argv[3..], ["--session", TURN_SESSION]);
    let replayed = Replay::new(&fixture("pi-0.87.1-rpc-resume.jsonl")).run(&mut driver, &opened);
    assert_eq!(replayed.sent, 1);
    assert_eq!(
        replayed.events,
        vec![
            Event::Ready,
            Event::SessionStarted {
                session: session(TURN_SESSION),
                forked_from: None
            }
        ]
    );

    let (mut driver, opened) = open(SessionMode::Fork(session(TURN_SESSION)));
    assert_eq!(opened.launch.argv[3..], ["--fork", TURN_SESSION]);
    let replayed = Replay::new(&fixture("pi-0.87.1-rpc-fork.jsonl")).run(&mut driver, &opened);
    assert_eq!(
        replayed.events[1],
        Event::SessionStarted {
            session: session("01a0e06c-65c8-76c2-abf0-d81d5b4e4080"),
            forked_from: Some(session(TURN_SESSION))
        }
    );

    // `--session` also matches an ID prefix; a different full ID fails.
    let (_, events) = ready(SessionMode::Resume(session("01a0e06b")), TURN_SESSION);
    assert!(
        matches!(&events[..], [Event::OpenFailed { reason }] if reason.contains("resume of 01a0e06b")),
        "{events:?}"
    );
    let (_, events) = ready(SessionMode::Fork(session("s1")), "s1");
    assert!(
        matches!(&events[..], [Event::OpenFailed { reason }] if reason.contains("kept the parent"))
    );
}

#[test]
fn session_file_references_are_refused() {
    for reference in [
        "../other/session.jsonl",
        "/home/agent/x",
        "a\\b",
        "abc.jsonl",
    ] {
        for mode in [
            SessionMode::Resume(session(reference)),
            SessionMode::Fork(session(reference)),
        ] {
            let mut driver = Pi::new(vec!["pi".into()]);
            assert!(
                matches!(
                    driver.open(Open { mode, ..fresh() }),
                    Err(Rejected::InvalidOpen(_))
                ),
                "{reference}"
            );
        }
    }
}

#[test]
fn a_rejected_prompt_ends_the_turn() {
    let (mut driver, opened) = open(SessionMode::Fresh);
    let replayed = Replay::new(&fixture("pi-0.87.1-rpc-no-credentials.jsonl"))
        .prompt("Say hello.")
        .run(&mut driver, &opened);
    let Some(Event::TurnEnded {
        turn: 1,
        outcome: TurnOutcome::Failed { message },
    }) = replayed.events.last()
    else {
        panic!("{:?}", replayed.events)
    };
    assert!(message.starts_with("No API key found"));
    driver.submit("again").unwrap();
}

#[test]
fn outcomes_follow_the_last_assistant_stop_reason() {
    for (stop, error, outcome) in [
        ("stop", None, TurnOutcome::Completed),
        ("aborted", None, TurnOutcome::Interrupted),
        (
            "length",
            None,
            TurnOutcome::LimitReached {
                limit: "max_tokens".into(),
            },
        ),
        (
            "error",
            Some("529 overloaded"),
            TurnOutcome::Failed {
                message: "529 overloaded".into(),
            },
        ),
    ] {
        let (mut driver, _) = ready(SessionMode::Fresh, "s1");
        driver.submit("go").unwrap();
        let (events, _) = feed(&mut driver, &assistant(stop, error));
        assert_eq!(
            events,
            vec![Event::UsageObserved {
                turn: Some(1),
                usage: Usage {
                    cumulative: false,
                    input_tokens: Some(12),
                    output_tokens: Some(3),
                    cached_input_tokens: Some(5),
                    cost_usd: Some(0.3),
                    ..Usage::default()
                }
            }]
        );
        feed(
            &mut driver,
            &json!({"type": "agent_end", "messages": [], "willRetry": false}),
        );
        let (events, _) = feed(&mut driver, &json!({"type": "agent_settled"}));
        assert_eq!(
            events,
            vec![Event::TurnEnded { turn: 1, outcome }],
            "{stop}"
        );
    }
}

#[test]
fn streams_text_and_tools() {
    let (mut driver, _) = ready(SessionMode::Fresh, "s1");
    let submitted = driver.submit("list files").unwrap();
    let prompt = decode(&submitted.frames[0]);
    assert_eq!(prompt["type"], "prompt");
    assert_eq!(prompt["message"], "list files");
    let events: Vec<Event> = [
        response(&prompt["id"], "prompt"),
        json!({"type": "message_update", "usage": {}, "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": "Hello"}}),
        json!({"type": "message_update", "usage": {}, "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 1, "delta": "hmm"}}),
        json!({"type": "tool_execution_start", "toolCallId": "call_1", "toolName": "bash", "args": {"command": "ls"}}),
        json!({"type": "tool_execution_end", "toolCallId": "call_1", "toolName": "bash", "result": {}, "isError": false}),
    ]
    .iter()
    .flat_map(|m| feed(&mut driver, m).0)
    .collect();
    assert_eq!(
        events[1..],
        [
            Event::MessageDelta {
                turn: 1,
                text: "Hello".into()
            },
            Event::ToolStarted {
                turn: 1,
                call_id: "call_1".into(),
                name: "bash".into()
            }
        ]
    );
}

#[test]
fn extension_dialogs_are_cancelled_and_notifications_do_not_block() {
    let (mut driver, _) = ready(SessionMode::Fresh, "s1");
    let (events, frames) = feed(
        &mut driver,
        &json!({"type": "extension_ui_request", "id": "uuid-1", "method": "confirm", "title": "Allow?", "message": "rm -rf"}),
    );
    assert_eq!(
        events,
        vec![Event::UnsupportedRequest {
            method: "extension_ui_request/confirm".into()
        }]
    );
    assert_eq!(
        frames,
        vec![json!({"type": "extension_ui_response", "id": "uuid-1", "cancelled": true})]
    );
    let (events, frames) = feed(
        &mut driver,
        &json!({"type": "extension_ui_request", "id": "uuid-2", "method": "setStatus", "statusKey": "k"}),
    );
    assert!(events.is_empty() && frames.is_empty());
    let (events, _) = feed(
        &mut driver,
        &json!({"type": "extension_ui_request", "id": "uuid-3", "method": "notify", "message": "blocked", "notifyType": "warning"}),
    );
    assert_eq!(
        events,
        vec![Event::Warning {
            message: "blocked".into()
        }]
    );
}

#[test]
fn slash_prompts_permissions_and_transport_loss() {
    let (mut driver, _) = ready(SessionMode::Fresh, "s1");
    assert!(matches!(
        driver.submit("/skill:deploy"),
        Err(Rejected::Unsupported(_))
    ));
    assert_eq!(
        driver.respond(&PermissionKey("1".into()), PermissionDecision::Allow),
        Err(Rejected::UnknownPermission)
    );
    let (events, _) = feed(
        &mut driver,
        &json!({"type": "response", "command": "parse", "success": false, "error": "Failed to parse command"}),
    );
    assert!(
        matches!(&events[..], [Event::ProtocolViolation { detail }] if detail.contains("Failed to parse"))
    );
    driver.submit("go").unwrap();
    assert!(matches!(
        driver.transport_closed()[..],
        [Event::OutcomeUnknown { turn: 1, .. }, Event::SessionClosed]
    ));
    assert_eq!(driver.submit("again"), Err(Rejected::NotReady));

    // A missing session makes pi 0.87.1 exit 1 before any output.
    let (mut driver, _) = open(SessionMode::Resume(session(
        "01a0e06b-0000-0000-0000-000000000000",
    )));
    assert_eq!(driver.transport_closed(), vec![Event::SessionClosed]);
}

/// Recorded from pi 0.87.1 and a stand-in API: a steered message is queued
/// and delivered before the next model call, in the same run.
#[test]
fn a_steered_message_joins_the_run() {
    let (mut driver, opened) = open(SessionMode::Fresh);
    let replayed = Replay::new(&fixture("pi-0.87.1-rpc-steer.jsonl"))
        .prompt("TEXTTURN: please start")
        .steer("STEER-MESSAGE: bananas")
        .run(&mut driver, &opened);
    assert_eq!(replayed.sent, 3, "get_state, prompt and steer");
    assert!(replayed.unsent.is_empty(), "nothing is left to clear");
    let events = replayed.events;
    assert!(events.contains(&Event::SteerAccepted { turn: 1, steer: 1 }));
    assert!(!events
        .iter()
        .any(|e| matches!(e, Event::SteerRejected { .. })));
    assert!(events.iter().any(|e| matches!(e,
        Event::MessageDelta { text, .. } if text.contains("STEER-MESSAGE: bananas"))));
    let ends = events
        .iter()
        .filter(|e| matches!(e, Event::TurnEnded { .. }))
        .count();
    assert_eq!(ends, 1);
    assert_eq!(
        events.last(),
        Some(&Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Completed
        })
    );
}

/// Pi keeps a queued message through an abort; the driver clears the queue
/// first, and reports the message undelivered.
#[test]
fn an_abort_clears_queued_steered_messages_first() {
    let (mut driver, opened) = open(SessionMode::Fresh);
    let replayed = Replay::new(&fixture("pi-0.87.1-rpc-steer-abort.jsonl"))
        .prompt("TEXTTURN: please start")
        .steer("STEER-MESSAGE: bananas")
        .interrupt()
        .run(&mut driver, &opened);
    assert_eq!(
        replayed.sent, 5,
        "get_state, prompt, steer, clear_queue, abort"
    );
    let events = replayed.events;
    assert!(events.contains(&Event::SteerRejected {
        turn: 1,
        steer: 1,
        reason: "cleared from Pi's queue before it was delivered".into()
    }));
    assert!(events.contains(&Event::TurnEnded {
        turn: 1,
        outcome: TurnOutcome::Interrupted
    }));
}

/// A message still queued when the run settles would join the next prompt:
/// the driver clears it and reports it undelivered.
#[test]
fn a_message_left_queued_at_settle_is_cleared() {
    let (mut driver, _) = ready(SessionMode::Fresh, "s1");
    let submitted = driver.submit("go").unwrap();
    let prompt = decode(&submitted.frames[0]);
    feed(&mut driver, &response(&prompt["id"], "prompt"));
    assert_eq!(
        driver.steer("/skill:x"),
        Err(Rejected::Unsupported(
            "a steered message beginning with / can run a Pi command".into()
        ))
    );
    let steer = decode(&driver.steer("late").unwrap()[0]);
    assert_eq!(steer["type"], "steer");
    assert_eq!(steer["message"], "late");
    feed(
        &mut driver,
        &json!({"type": "queue_update", "steering": ["late"], "followUp": []}),
    );
    let (events, _) = feed(&mut driver, &response(&steer["id"], "steer"));
    assert_eq!(events, [Event::SteerAccepted { turn: 1, steer: 1 }]);
    let (events, frames) = feed(&mut driver, &json!({"type": "agent_settled"}));
    assert_eq!(
        events,
        [
            Event::SteerRejected {
                turn: 1,
                steer: 1,
                reason: "the run settled before Pi delivered it".into()
            },
            Event::TurnEnded {
                turn: 1,
                outcome: TurnOutcome::Completed
            }
        ]
    );
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["type"], "clear_queue");
}
