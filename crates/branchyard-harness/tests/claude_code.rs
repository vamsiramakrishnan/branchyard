//! Claude Code driver against a recorded Claude Code 2.1.283 session and
//! frames shaped by the Agent SDK's published stdout protocol types.

mod common;

use branchyard_harness::claude_code::ClaudeCode;
use branchyard_harness::{
    Driver, Event, NativeSession, Open, PermissionDecision, PermissionKey, Rejected, SessionMode,
    TurnOutcome,
};
use common::{decode, feed, transcript};
use serde_json::{json, Value};

const FIXTURE: &str = "claude-code-2.1.283-stream-json-turn.jsonl";

fn open(mode: SessionMode) -> (ClaudeCode, Vec<String>, Value) {
    let mut driver = ClaudeCode::new(vec!["claude".into()]);
    let opened = driver
        .open(Open {
            mode,
            cwd: "/workspace".into(),
            model: None,
        })
        .unwrap();
    assert_eq!(opened.launch.cwd, "/workspace");
    let initialize = decode(&opened.frames[0]);
    (driver, opened.launch.argv, initialize)
}

fn ready(mode: SessionMode) -> ClaudeCode {
    let (mut driver, _, initialize) = open(mode);
    let id = initialize["request_id"].clone();
    let (events, _) = feed(
        &mut driver,
        &json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": {}}}),
    );
    assert_eq!(events, vec![Event::Ready]);
    driver
}

fn session(id: &str) -> NativeSession {
    NativeSession::new(id).unwrap()
}

fn init(id: &str) -> Value {
    json!({"type": "system", "subtype": "init", "session_id": id})
}

#[test]
fn launch_matches_the_agent_sdk_invocation() {
    let (_, argv, initialize) = open(SessionMode::Fresh);
    assert_eq!(
        argv,
        [
            "claude",
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-prompt-tool",
            "stdio"
        ]
    );
    assert_eq!(initialize["type"], "control_request");
    assert_eq!(initialize["request"], json!({"subtype": "initialize"}));

    let (_, argv, _) = open(SessionMode::Resume(session("parent")));
    assert!(argv.ends_with(&["--resume".into(), "parent".into()]));
    let (_, argv, _) = open(SessionMode::Fork(session("parent")));
    assert!(argv.ends_with(&["--resume".into(), "parent".into(), "--fork-session".into()]));
}

/// Replay the recorded session: the driver's frames match what Claude Code
/// accepted, and the recorded output maps to the expected events.
#[test]
fn replays_a_recorded_claude_code_turn() {
    let recorded = transcript(FIXTURE);
    let (mut driver, _, initialize) = open(SessionMode::Fresh);
    let sent_initialize = &recorded[0].frame;
    assert!(recorded[0].outgoing);
    assert_eq!(initialize["request"], sent_initialize["request"]);

    let mut events = Vec::new();
    let mut our_uuid = String::new();
    let mut recorded_uuid = String::new();
    for row in &recorded[1..] {
        if row.outgoing {
            // The recorded user message, apart from its random UUID, is the
            // frame the driver writes.
            let submitted = driver.submit("Say hello.").unwrap();
            let mut ours = decode(&submitted.frames[0]);
            our_uuid = ours["uuid"].as_str().unwrap().to_owned();
            recorded_uuid = row.frame["uuid"].as_str().unwrap().to_owned();
            ours["uuid"] = row.frame["uuid"].clone();
            assert_eq!(ours, row.frame);
            continue;
        }
        let text = row
            .frame
            .to_string()
            .replace("\"req-1\"", &format!("{}", initialize["request_id"]))
            .replace(&recorded_uuid, &our_uuid);
        let (mut received, frames) = feed(&mut driver, &serde_json::from_str(&text).unwrap());
        assert!(frames.is_empty());
        events.append(&mut received);
    }

    let session = session("73f1902f-b14d-4d40-8751-169fd42fec8c");
    assert_eq!(events[0], Event::Ready);
    assert_eq!(
        events[1],
        Event::TurnAccepted {
            turn: 1,
            native: Some(our_uuid)
        }
    );
    assert!(events.contains(&Event::SessionStarted {
        session,
        forked_from: None
    }));
    assert!(events.contains(&Event::MessageDelta {
        turn: 1,
        text: "Hello! What can I help you with today?".into()
    }));
    let usage = events
        .iter()
        .find_map(|e| match e {
            Event::UsageObserved { usage, .. } => Some(usage.clone()),
            _ => None,
        })
        .unwrap();
    assert!(usage.cumulative);
    assert_eq!(usage.cost_usd, Some(0.0404016));
    assert!(usage.output_tokens.unwrap() > 0);
    assert_eq!(
        events.last(),
        Some(&Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Completed
        })
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e, Event::ProtocolViolation { .. })));
}

#[test]
fn permission_prompts_round_trip() {
    let mut driver = ready(SessionMode::Fresh);
    driver.submit("edit the file").unwrap();
    let request = json!({
        "type": "control_request",
        "request_id": "perm-1",
        "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "ls"}},
    });
    let (events, _) = feed(&mut driver, &request);
    let Event::PermissionRequested {
        turn: Some(1),
        request,
    } = &events[0]
    else {
        panic!("{events:?}")
    };
    assert_eq!(
        (request.tool.as_str(), &request.input),
        ("Bash", &json!({"command": "ls"}))
    );

    let frames = driver
        .respond(&request.key, PermissionDecision::Allow)
        .unwrap();
    assert_eq!(
        decode(&frames[0]),
        json!({"type": "control_response", "response": {"subtype": "success", "request_id": "perm-1",
            "response": {"behavior": "allow", "updatedInput": {"command": "ls"}}}})
    );
    assert_eq!(
        driver.respond(&request.key, PermissionDecision::Allow),
        Err(Rejected::UnknownPermission)
    );

    feed(
        &mut driver,
        &json!({"type": "control_request", "request_id": "perm-2",
            "request": {"subtype": "can_use_tool", "tool_name": "Write", "input": {}}}),
    );
    let frames = driver
        .respond(
            &PermissionKey("perm-2".into()),
            PermissionDecision::Deny {
                message: "not in scope".into(),
            },
        )
        .unwrap();
    assert_eq!(
        decode(&frames[0])["response"]["response"],
        json!({"behavior": "deny", "message": "not in scope"})
    );

    feed(
        &mut driver,
        &json!({"type": "control_request", "request_id": "perm-3",
            "request": {"subtype": "can_use_tool", "tool_name": "Write", "input": {}}}),
    );
    let (events, _) = feed(
        &mut driver,
        &json!({"type": "control_cancel_request", "request_id": "perm-3"}),
    );
    assert_eq!(
        events,
        vec![Event::PermissionWithdrawn {
            key: PermissionKey("perm-3".into())
        }]
    );
}

#[test]
fn unsupported_control_requests_are_answered_not_left_waiting() {
    let mut driver = ready(SessionMode::Fresh);
    let (events, frames) = feed(
        &mut driver,
        &json!({"type": "control_request", "request_id": "hook-1", "request": {"subtype": "hook_callback"}}),
    );
    assert_eq!(
        events,
        vec![Event::UnsupportedRequest {
            method: "control_request/hook_callback".into()
        }]
    );
    assert_eq!(frames[0]["response"]["subtype"], "error");
    assert_eq!(frames[0]["response"]["request_id"], "hook-1");
}

#[test]
fn interrupts_are_acknowledged_then_end_the_turn() {
    let mut driver = ready(SessionMode::Fresh);
    assert_eq!(driver.interrupt(), Err(Rejected::NoTurn));
    driver.submit("long task").unwrap();
    let frames = driver.interrupt().unwrap();
    let interrupt = decode(&frames[0]);
    assert_eq!(interrupt["request"], json!({"subtype": "interrupt"}));

    let (events, _) = feed(
        &mut driver,
        &json!({"type": "control_response", "response": {"subtype": "success", "request_id": interrupt["request_id"]}}),
    );
    assert_eq!(events, vec![Event::InterruptAcknowledged { turn: 1 }]);
    let (events, _) = feed(
        &mut driver,
        &json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_streaming", "errors": []}),
    );
    assert_eq!(
        events.last(),
        Some(&Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Interrupted
        })
    );
}

#[test]
fn limits_and_errors_are_distinct_outcomes() {
    let mut driver = ready(SessionMode::Fresh);
    for (result, expected) in [
        (
            json!({"type": "result", "subtype": "error_max_turns", "is_error": true}),
            TurnOutcome::LimitReached {
                limit: "max_turns".into(),
            },
        ),
        (
            json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "errors": ["boom"]}),
            TurnOutcome::Failed {
                message: "boom".into(),
            },
        ),
        (
            json!({"type": "result", "subtype": "success", "is_error": true, "result": "API Error: 529"}),
            TurnOutcome::Failed {
                message: "API Error: 529".into(),
            },
        ),
    ] {
        let turn = driver.submit("go").unwrap().turn;
        let (events, _) = feed(&mut driver, &result);
        assert_eq!(
            events.last(),
            Some(&Event::TurnEnded {
                turn,
                outcome: expected
            })
        );
        let usage = events.iter().find_map(|e| match e {
            Event::UsageObserved { usage, .. } => Some(usage),
            _ => None,
        });
        // Missing usage is unknown, never zero.
        assert_eq!(usage.unwrap().input_tokens, None);
    }
}

#[test]
fn resume_and_fork_identities_are_verified() {
    let mut driver = ready(SessionMode::Resume(session("original")));
    driver.submit("continue").unwrap();
    let (events, _) = feed(&mut driver, &init("someone-else"));
    assert!(
        matches!(&events[0], Event::ProtocolViolation { detail } if detail.contains("resume of original"))
    );

    let mut driver = ready(SessionMode::Fork(session("parent")));
    driver.submit("branch").unwrap();
    let (events, _) = feed(&mut driver, &init("parent"));
    assert!(
        matches!(&events[0], Event::ProtocolViolation { detail } if detail.contains("kept the parent"))
    );

    let mut driver = ready(SessionMode::Fork(session("parent")));
    driver.submit("branch").unwrap();
    let (events, _) = feed(&mut driver, &init("child"));
    assert_eq!(
        events,
        vec![Event::SessionStarted {
            session: session("child"),
            forked_from: Some(session("parent"))
        }]
    );
    // Later turns repeat init for the same session without new events.
    assert_eq!(feed(&mut driver, &init("child")).0, vec![]);
}

#[test]
fn submitting_requires_a_handshake_and_one_turn_at_a_time() {
    let (mut driver, _, _) = open(SessionMode::Fresh);
    assert_eq!(driver.submit("early"), Err(Rejected::NotReady));
    let mut driver = ready(SessionMode::Fresh);
    driver.submit("one").unwrap();
    assert_eq!(driver.submit("two"), Err(Rejected::TurnInProgress));
}

#[test]
fn a_result_for_another_turn_is_rejected() {
    let mut driver = ready(SessionMode::Fresh);
    driver.submit("mine").unwrap();
    let (events, _) = feed(
        &mut driver,
        &json!({"type": "result", "subtype": "success", "is_error": false, "user_message_uuid": "not-ours"}),
    );
    assert!(matches!(&events[0], Event::ProtocolViolation { .. }));
    let closed = driver.transport_closed();
    assert!(matches!(
        closed[..],
        [Event::OutcomeUnknown { turn: 1, .. }, Event::SessionClosed]
    ));
}
