//! Antigravity driver against transcripts recorded from Antigravity CLI
//! 1.2.11 without a model call, and against the headless-mode documentation.

use branchyard_harness::antigravity::Antigravity;
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

fn open(mode: SessionMode) -> (Antigravity, Opened) {
    let mut driver = Antigravity::new(vec!["agy".into()]);
    let opened = driver.open(Open { mode, ..fresh() }).unwrap();
    (driver, opened)
}

fn session(id: &str) -> NativeSession {
    NativeSession::new(id).unwrap()
}

fn init(id: &str) -> Value {
    json!({"event": "init", "conversation_id": id, "init": {"cwd": "/workspace", "tools": [], "permission_mode": "request-review"}})
}

fn step(index: u64, fields: Value) -> Value {
    let mut step = json!({"conversation_id": "c1", "step_index": index, "state": "DONE"});
    step.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    json!({"event": "step_update", "step_update": step})
}

fn ready() -> Antigravity {
    let (mut driver, _) = open(SessionMode::Fresh);
    let (events, _) = feed(&mut driver, &init("c1"));
    assert_eq!(events[0], Event::Ready);
    driver
}

#[test]
fn replays_a_recorded_turn() {
    let (mut driver, opened) = open(SessionMode::Fresh);
    assert_eq!(
        opened.launch.argv,
        [
            "agy",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--disable-slash-commands"
        ]
    );
    assert!(opened.frames.is_empty(), "the CLI speaks first");
    let replayed = Replay::new(&fixture("antigravity-1.2.11-stream-json-turn.jsonl"))
        .prompt("Say hello.")
        .run(&mut driver, &opened);
    assert_eq!(replayed.sent, 1);
    assert!(replayed.unsent.is_empty());
    let events = replayed.events;
    assert_eq!(events[0], Event::Ready);
    assert_eq!(
        events[1],
        Event::SessionStarted {
            session: session("df5c8744-2e4e-4b77-aced-e5525bb54af9"),
            forked_from: None
        }
    );
    assert_eq!(
        events[2],
        Event::TurnAccepted {
            turn: 1,
            native: Some("step 0".into())
        }
    );
    assert!(matches!(events[3], Event::Warning { .. }));
    assert!(matches!(events[4], Event::Warning { .. }));
    assert_eq!(
        events[5],
        Event::UsageObserved {
            turn: Some(1),
            usage: Usage {
                cumulative: true,
                input_tokens: Some(0),
                output_tokens: Some(0),
                cached_input_tokens: Some(0),
                cost_usd: None
            }
        }
    );
    let Event::TurnEnded {
        turn: 1,
        outcome: TurnOutcome::Failed { message },
    } = &events[6]
    else {
        panic!("{:?}", events[6])
    };
    assert!(message.contains("connection refused"));
    assert_eq!(events.len(), 7);
}

#[test]
fn resume_checks_the_recorded_conversation_id() {
    let (mut driver, opened) = open(SessionMode::Resume(session(
        "df5c8744-2e4e-4b77-aced-e5525bb54af9",
    )));
    assert_eq!(
        opened.launch.argv[6..],
        ["--conversation", "df5c8744-2e4e-4b77-aced-e5525bb54af9"]
    );
    let replayed =
        Replay::new(&fixture("antigravity-1.2.11-resume.jsonl")).run(&mut driver, &opened);
    assert_eq!(replayed.events[0], Event::Ready);

    // 1.2.11 starts a new conversation when the ID is unknown; the driver
    // refuses it rather than treat it as the resumed one.
    let (mut driver, opened) = open(SessionMode::Resume(session(
        "00000000-0000-0000-0000-000000000000",
    )));
    let replayed =
        Replay::new(&fixture("antigravity-1.2.11-resume-unknown.jsonl")).run(&mut driver, &opened);
    assert!(
        matches!(&replayed.events[..], [Event::OpenFailed { reason }] if reason.contains("opened conversation 1040ed66")),
        "{:?}",
        replayed.events
    );
    assert_eq!(driver.submit("go"), Err(Rejected::NotReady));
}

#[test]
fn a_result_before_init_fails_the_open() {
    let (mut driver, opened) = open(SessionMode::Fresh);
    let replayed =
        Replay::new(&fixture("antigravity-1.2.11-unauthenticated.jsonl")).run(&mut driver, &opened);
    assert_eq!(
        replayed.events,
        vec![Event::OpenFailed {
            reason: "authentication failed or timed out".into()
        }]
    );
}

#[test]
fn fork_is_refused_and_model_is_passed() {
    let mut driver = Antigravity::new(vec!["agy".into()]);
    assert!(matches!(
        driver.open(Open {
            mode: SessionMode::Fork(session("c1")),
            ..fresh()
        }),
        Err(Rejected::Unsupported(_))
    ));
    let (_, opened) = {
        let mut driver = Antigravity::new(vec!["agy".into()]);
        let opened = driver
            .open(Open {
                model: Some("gemini-3.8-flash-high".into()),
                ..fresh()
            })
            .unwrap();
        (driver, opened)
    };
    assert_eq!(
        opened.launch.argv[6..],
        ["--model", "gemini-3.8-flash-high"]
    );
    let capabilities = driver.capabilities();
    assert!(capabilities.resume && !capabilities.fork);
    assert!(!capabilities.tool_approvals && !capabilities.cancellation);
}

/// The documented two-turn session: text deltas, tools once per step,
/// cumulative usage per result.
#[test]
fn streams_text_tools_and_cumulative_usage() {
    let mut driver = ready();
    let submitted = driver
        .submit("Reply with exactly the word: apple.")
        .unwrap();
    assert_eq!(
        decode(&submitted.frames[0]),
        json!({"event": "user", "message": {"content": "Reply with exactly the word: apple."}})
    );
    let messages = [
        step(0, json!({"step_type": "user_input"})),
        step(
            2,
            json!({"state": "ACTIVE", "step_type": "agent_response", "text_delta": "apple"}),
        ),
        step(
            3,
            json!({"state": "ACTIVE", "step_type": "tool", "tool_name": "run_command"}),
        ),
        step(
            3,
            json!({"step_type": "tool", "tool_name": "run_command", "tool_info": {"name": "run_command", "parameters": {"CommandLine": "echo hi"}, "output": "hi"}}),
        ),
        step(4, json!({"step_type": "checkpoint"})),
        json!({"event": "result", "result": {"conversation_id": "c1", "status": "SUCCESS", "response": "apple\n", "num_turns": 1,
            "usage": {"input_tokens": 30384, "output_tokens": 4, "thinking_tokens": 0, "cache_read_tokens": 0, "total_tokens": 30388}}}),
    ];
    let events: Vec<Event> = messages
        .iter()
        .flat_map(|m| feed(&mut driver, m).0)
        .collect();
    assert_eq!(
        events[1],
        Event::MessageDelta {
            turn: 1,
            text: "apple".into()
        }
    );
    assert_eq!(
        events[2],
        Event::ToolStarted {
            turn: 1,
            call_id: "3".into(),
            name: "run_command".into()
        }
    );
    assert!(
        matches!(events[3], Event::UsageObserved { turn: Some(1), ref usage } if usage.cumulative && usage.input_tokens == Some(30384))
    );
    assert_eq!(
        events[4],
        Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Completed
        }
    );
    assert_eq!(events.len(), 5, "a tool step is started once");

    driver.submit("Again.").unwrap();
    let (events, _) = feed(
        &mut driver,
        &json!({"event": "result", "result": {"conversation_id": "c1", "status": "CANCELED", "response": ""}}),
    );
    assert_eq!(
        events,
        vec![Event::TurnEnded {
            turn: 2,
            outcome: TurnOutcome::Interrupted
        }]
    );
}

#[test]
fn foreign_results_and_stray_output_are_violations() {
    let mut driver = ready();
    let (events, _) = feed(
        &mut driver,
        &json!({"event": "result", "result": {"conversation_id": "c1", "status": "ERROR", "error": "/model is answered by the CLI itself"}}),
    );
    assert!(
        matches!(&events[..], [Event::ProtocolViolation { detail }] if detail.contains("without a turn"))
    );
    driver.submit("go").unwrap();
    let (events, _) = feed(
        &mut driver,
        &json!({"event": "result", "result": {"conversation_id": "other", "status": "SUCCESS"}}),
    );
    assert!(matches!(&events[..], [Event::ProtocolViolation { .. }]));
    let (events, _) = feed(&mut driver, &init("c2"));
    assert!(matches!(&events[..], [Event::ProtocolViolation { .. }]));
    let (events, _) = feed(&mut driver, &json!({"message": {}}));
    assert!(matches!(&events[..], [Event::ProtocolViolation { .. }]));
    let (events, _) = feed(&mut driver, &json!({"event": "future_thing"}));
    assert_eq!(
        events,
        vec![Event::Unrecognized {
            kind: "future_thing".into()
        }]
    );
}

#[test]
fn no_cancellation_or_permissions_and_loss_is_unknown() {
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
    assert_eq!(driver.submit("again"), Err(Rejected::NotReady));
}
