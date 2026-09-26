//! Codex driver against a recorded codex-cli 0.157.1 app-server session and
//! frames shaped by the schema `codex app-server generate-json-schema` emits.

mod common;

use branchyard_harness::codex::Codex;
use branchyard_harness::{
    Driver, Event, NativeSession, Open, PermissionDecision, PermissionKey, Rejected, SessionMode,
    TurnOutcome,
};
use common::{decode, feed, transcript};
use serde_json::{json, Value};

const FIXTURE: &str = "codex-0.157.1-unauthenticated-turn.jsonl";

fn open(mode: SessionMode) -> (Codex, Vec<Value>) {
    let mut driver = Codex::new(vec!["codex".into()]);
    let opened = driver
        .open(Open {
            mode,
            cwd: "/workspace".into(),
            model: None,
        })
        .unwrap();
    assert_eq!(opened.launch.argv, ["codex", "app-server"]);
    (driver, opened.frames.iter().map(decode).collect())
}

/// Complete the handshake, answering the thread request with `thread_id`.
fn ready(mode: SessionMode, thread_id: &str) -> (Codex, Vec<Event>, Value) {
    let (mut driver, _) = open(mode);
    let (_, frames) = feed(&mut driver, &json!({"id": 1, "result": {}}));
    let thread_request = frames[1].clone();
    let (events, _) = feed(
        &mut driver,
        &json!({"id": 2, "result": {"thread": {"id": thread_id}}}),
    );
    (driver, events, thread_request)
}

fn session(id: &str) -> NativeSession {
    NativeSession::new(id).unwrap()
}

/// Replay the recorded session: every frame the driver writes equals the
/// frame codex-cli 0.157.1 accepted, and the recorded output maps to the
/// expected events, ending in the 401 failure.
#[test]
fn replays_a_recorded_codex_session() {
    let recorded = transcript(FIXTURE);
    let (mut driver, mut written) = open(SessionMode::Fresh);
    let mut events = Vec::new();
    let mut sent = 0;
    for row in recorded {
        if row.outgoing {
            if written.is_empty() {
                // The only frame not written in response to output.
                let submitted = driver.submit("Say hello.").unwrap();
                written = submitted.frames.iter().map(decode).collect();
            }
            assert_eq!(written.remove(0), row.frame, "outgoing frame {sent}");
            sent += 1;
            continue;
        }
        let (mut received, frames) = feed(&mut driver, &row.frame);
        events.append(&mut received);
        written.extend(frames);
    }
    assert_eq!(
        sent, 4,
        "initialize, initialized, thread/start and turn/start"
    );

    let thread = session("01a0dfe1-467d-7ae1-b734-3917620eaeb6");
    assert!(events.contains(&Event::Ready));
    assert!(events.contains(&Event::SessionStarted {
        session: thread,
        forked_from: None
    }));
    assert!(events.contains(&Event::TurnAccepted {
        turn: 1,
        native: Some("01a0dfe1-4695-7001-aeff-5a2e38e5a3e1".into())
    }));
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::Warning { message } if message.starts_with("Reconnecting"))));
    let Some(Event::TurnEnded {
        turn: 1,
        outcome: TurnOutcome::Failed { message },
    }) = events.last()
    else {
        panic!("{:?}", events.last())
    };
    assert!(message.contains("401 Unauthorized"));
    assert!(!events
        .iter()
        .any(|e| matches!(e, Event::ProtocolViolation { .. })));
}

#[test]
fn approvals_round_trip_with_codex_decisions() {
    let (mut driver, _, _) = ready(SessionMode::Fresh, "t1");
    driver.submit("run the tests").unwrap();
    let approval = json!({
        "id": 7,
        "method": "item/commandExecution/requestApproval",
        "params": {"itemId": "i1", "threadId": "t1", "turnId": "u1", "startedAtMs": 0, "command": "cargo test"},
    });
    let (events, _) = feed(&mut driver, &approval);
    let Event::PermissionRequested {
        turn: Some(1),
        request,
    } = &events[0]
    else {
        panic!("{events:?}")
    };
    assert_eq!(request.tool, "commandExecution");
    assert_eq!(request.input["command"], "cargo test");
    let frames = driver
        .respond(&request.key, PermissionDecision::Allow)
        .unwrap();
    assert_eq!(
        decode(&frames[0]),
        json!({"id": 7, "result": {"decision": "accept"}})
    );

    feed(
        &mut driver,
        &json!({"id": "s-8", "method": "item/fileChange/requestApproval",
            "params": {"itemId": "i2", "threadId": "t1", "turnId": "u1", "startedAtMs": 0}}),
    );
    let key = PermissionKey("\"s-8\"".into());
    let frames = driver
        .respond(
            &key,
            PermissionDecision::Deny {
                message: "outside scope".into(),
            },
        )
        .unwrap();
    // String request IDs are echoed back unchanged.
    assert_eq!(
        decode(&frames[0]),
        json!({"id": "s-8", "result": {"decision": "decline"}})
    );

    feed(
        &mut driver,
        &json!({"id": 9, "method": "item/commandExecution/requestApproval",
            "params": {"itemId": "i3", "threadId": "t1", "turnId": "u1", "startedAtMs": 0}}),
    );
    let (events, _) = feed(
        &mut driver,
        &json!({"method": "serverRequest/resolved", "params": {"threadId": "t1", "requestId": 9}}),
    );
    assert_eq!(
        events,
        vec![Event::PermissionWithdrawn {
            key: PermissionKey("9".into())
        }]
    );
}

#[test]
fn unimplemented_server_requests_get_errors() {
    let (mut driver, _, _) = ready(SessionMode::Fresh, "t1");
    let (events, frames) = feed(
        &mut driver,
        &json!({"id": 3, "method": "item/tool/requestUserInput", "params": {}}),
    );
    assert_eq!(
        events,
        vec![Event::UnsupportedRequest {
            method: "item/tool/requestUserInput".into()
        }]
    );
    assert_eq!(frames[0]["id"], 3);
    assert_eq!(frames[0]["error"]["code"], -32601);
}

#[test]
fn interrupt_names_the_native_turn_once_known() {
    let (mut driver, _, _) = ready(SessionMode::Fresh, "t1");
    let submitted = driver.submit("long task").unwrap();
    let turn_start = decode(&submitted.frames[0]);
    // The native turn ID is unknown until turn/start answers.
    assert_eq!(driver.interrupt(), Err(Rejected::NotReady));
    feed(
        &mut driver,
        &json!({"id": turn_start["id"], "result": {"turn": {"id": "u1", "items": [], "status": "inProgress"}}}),
    );
    let interrupt = decode(&driver.interrupt().unwrap()[0]);
    assert_eq!(interrupt["method"], "turn/interrupt");
    assert_eq!(
        interrupt["params"],
        json!({"threadId": "t1", "turnId": "u1"})
    );
    let (events, _) = feed(&mut driver, &json!({"id": interrupt["id"], "result": {}}));
    assert_eq!(events, vec![Event::InterruptAcknowledged { turn: 1 }]);
    let (events, _) = feed(
        &mut driver,
        &json!({"method": "turn/completed", "params": {"threadId": "t1", "turn": {"id": "u1", "items": [], "status": "interrupted"}}}),
    );
    assert_eq!(
        events,
        vec![Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Interrupted
        }]
    );
}

#[test]
fn streaming_tools_and_usage_are_normalized() {
    let (mut driver, _, _) = ready(SessionMode::Fresh, "t1");
    driver.submit("go").unwrap();
    let events: Vec<Event> = [
        json!({"method": "item/agentMessage/delta", "params": {"delta": "Hel", "itemId": "m", "threadId": "t1", "turnId": "u1"}}),
        json!({"method": "item/started", "params": {"item": {"type": "commandExecution", "id": "c1"}, "startedAtMs": 0, "threadId": "t1", "turnId": "u1"}}),
        json!({"method": "item/started", "params": {"item": {"type": "reasoning", "id": "r1"}, "startedAtMs": 0, "threadId": "t1", "turnId": "u1"}}),
        json!({"method": "thread/tokenUsage/updated", "params": {"threadId": "t1", "turnId": "u1", "tokenUsage": {
            "total": {"inputTokens": 10, "outputTokens": 5, "cachedInputTokens": 2, "reasoningOutputTokens": 0, "totalTokens": 15},
            "last": {"inputTokens": 10, "outputTokens": 5, "cachedInputTokens": 2, "reasoningOutputTokens": 0, "totalTokens": 15}}}}),
    ]
    .iter()
    .flat_map(|m| feed(&mut driver, m).0)
    .collect();
    assert_eq!(
        events[0],
        Event::MessageDelta {
            turn: 1,
            text: "Hel".into()
        }
    );
    assert_eq!(
        events[1],
        Event::ToolStarted {
            turn: 1,
            call_id: "c1".into(),
            name: "commandExecution".into()
        }
    );
    let Event::UsageObserved { usage, .. } = &events[2] else {
        panic!("{events:?}")
    };
    assert!(usage.cumulative);
    assert_eq!(
        (usage.input_tokens, usage.output_tokens, usage.cost_usd),
        (Some(10), Some(5), None)
    );
    assert_eq!(events.len(), 3, "reasoning items are not tool starts");
}

#[test]
fn resume_and_fork_identities_are_verified() {
    let (_, events, request) = ready(SessionMode::Resume(session("t-old")), "t-other");
    assert_eq!(request["method"], "thread/resume");
    assert_eq!(request["params"]["threadId"], "t-old");
    assert!(
        matches!(&events[0], Event::OpenFailed { reason } if reason.contains("resume of t-old"))
    );

    let (_, events, _) = ready(SessionMode::Resume(session("t-old")), "t-old");
    assert_eq!(events[0], Event::Ready);

    let (_, events, request) = ready(SessionMode::Fork(session("t-old")), "t-old");
    assert_eq!(request["method"], "thread/fork");
    assert!(
        matches!(&events[0], Event::OpenFailed { reason } if reason.contains("kept the parent"))
    );

    let (_, events, _) = ready(SessionMode::Fork(session("t-old")), "t-new");
    assert_eq!(
        events[1],
        Event::SessionStarted {
            session: session("t-new"),
            forked_from: Some(session("t-old"))
        }
    );
}

#[test]
fn a_rejected_turn_start_ends_the_turn_and_closing_mid_turn_is_unknown() {
    let (mut driver, _, _) = ready(SessionMode::Fresh, "t1");
    let submitted = driver.submit("go").unwrap();
    let id = decode(&submitted.frames[0])["id"].clone();
    let (events, _) = feed(
        &mut driver,
        &json!({"id": id, "error": {"code": -32600, "message": "bad input"}}),
    );
    assert_eq!(
        events,
        vec![Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Failed {
                message: "bad input".into()
            }
        }]
    );
    driver.submit("again").unwrap();
    assert!(matches!(
        driver.transport_closed()[..],
        [Event::OutcomeUnknown { turn: 2, .. }, Event::SessionClosed]
    ));
}

#[test]
fn errors_outside_a_turn_are_warnings() {
    let (mut driver, _, _) = ready(SessionMode::Fresh, "t1");
    let (events, _) = feed(
        &mut driver,
        &json!({"method": "error", "params": {"error": {"message": "stream reset"}, "threadId": "t1", "turnId": "u0", "willRetry": true}}),
    );
    assert_eq!(
        events,
        vec![Event::Warning {
            message: "stream reset".into()
        }]
    );
}
