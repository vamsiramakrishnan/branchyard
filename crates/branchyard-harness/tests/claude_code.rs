//! Claude Code driver against a recorded Claude Code 2.1.283 session and
//! frames shaped by the Agent SDK's published stdout protocol types.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
use branchyard_harness::claude_code::{mcp_config, ClaudeCode};
use branchyard_harness::conformance::{decode, feed, handshake, Replay, Transcript};
use branchyard_harness::{
    Driver, Event, Instructions, McpServer, NativeSession, Open, Opened, PermissionDecision,
    PermissionKey, Rejected, RemoteMcpServer, RemoteTransport, SessionMode, TurnOutcome,
};
use serde_json::{json, Value};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/claude-code-2.1.283-stream-json-turn.jsonl"
);

fn open_with(mode: SessionMode) -> (ClaudeCode, Opened) {
    let mut driver = ClaudeCode::new(vec!["claude".into()]);
    let opened = driver
        .open(Open {
            mode,
            cwd: "/workspace".into(),
            model: None,
            max_budget_usd: None,
            mcp_servers: Vec::new(),
            instructions: None,
            mcp_config_file: None,
            remote_mcp_servers: Vec::new(),
        })
        .unwrap();
    assert_eq!(opened.launch.cwd, "/workspace");
    (driver, opened)
}

fn open(mode: SessionMode) -> (ClaudeCode, Vec<String>, Value) {
    let (driver, opened) = open_with(mode);
    let initialize = decode(&opened.frames[0]);
    (driver, opened.launch.argv, initialize)
}

/// Play Claude Code's side of the handshake.
fn answer(frame: &Value) -> Vec<Value> {
    match frame["request"]["subtype"].as_str() {
        Some("initialize") => vec![
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": frame["request_id"], "response": {}}}),
        ],
        _ => Vec::new(),
    }
}

fn ready(mode: SessionMode) -> ClaudeCode {
    let (mut driver, opened) = open_with(mode);
    let events = handshake(&mut driver, &opened.frames, answer);
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
    let recorded = Transcript::load(FIXTURE);
    let (mut driver, opened) = open_with(SessionMode::Fresh);
    // The request ID and the user message's random UUID are chosen per run;
    // every other field equals the recorded frame.
    let replayed = Replay::new(&recorded)
        .alias("/request_id")
        .alias("/uuid")
        .prompt("Say hello.")
        .run(&mut driver, &opened);
    assert_eq!(replayed.sent, 2, "initialize and the user message");
    assert!(replayed.unsent.is_empty());
    let recorded_uuid = &recorded.rows[2].frame["uuid"];
    let our_uuid = replayed.ours(recorded_uuid).unwrap().as_str().unwrap();
    assert_ne!(
        Some(our_uuid),
        recorded_uuid.as_str(),
        "a fresh UUID per run"
    );
    let our_uuid = our_uuid.to_owned();
    let events = replayed.events;

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
            Event::UsageObserved { usage, .. } if usage.cumulative => Some(usage.clone()),
            _ => None,
        })
        .unwrap();
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

/// A spending limit is passed as `--max-budget-usd`, and the turn Claude
/// Code stops at it ends at that limit; a limit that is not a positive
/// number is refused before launch.
#[test]
fn a_spending_limit_is_passed_and_stopping_at_it_is_a_limit() {
    let mut driver = ClaudeCode::new(vec!["claude".into()]);
    let opened = driver
        .open(Open {
            max_budget_usd: Some(0.0125),
            ..Open::new(SessionMode::Fresh, "/workspace")
        })
        .unwrap();
    let argv = opened.launch.argv;
    let at = argv.iter().position(|a| a == "--max-budget-usd").unwrap();
    assert_eq!(argv[at + 1], "0.0125");
    for usd in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let refused = ClaudeCode::new(vec!["claude".into()]).open(Open {
            max_budget_usd: Some(usd),
            ..Open::new(SessionMode::Fresh, "/workspace")
        });
        assert!(
            matches!(refused, Err(Rejected::InvalidOpen(_))),
            "{usd}: {refused:?}"
        );
    }
    let (_, argv, _) = open(SessionMode::Fresh);
    assert!(!argv.contains(&"--max-budget-usd".to_owned()));

    let mut driver = ready(SessionMode::Fresh);
    let turn = driver.submit("go").unwrap().turn;
    let result = json!({"type": "result", "subtype": "error_max_budget_usd", "is_error": true, "total_cost_usd": 0.013});
    let (events, _) = feed(&mut driver, &result);
    assert_eq!(
        events.last(),
        Some(&Event::TurnEnded {
            turn,
            outcome: TurnOutcome::LimitReached {
                limit: "max_budget_usd".into()
            }
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

fn branchyard_server() -> McpServer {
    McpServer {
        name: "branchyard".into(),
        command: "/usr/local/bin/by".into(),
        args: vec!["mcp".into(), "--branch".into(), "b".into()],
        env: vec![("BRANCHYARD_TOKEN".into(), "t0k".into())],
    }
}

#[test]
fn mcp_servers_are_one_mcp_config_argument_in_the_agent_sdk_shape() {
    // The Agent SDK passes `--mcp-config JSON.stringify({mcpServers})`, each
    // value an McpStdioServerConfig; that is also the file's content.
    assert_eq!(
        serde_json::from_str::<Value>(&mcp_config(&[branchyard_server()], &[])).unwrap(),
        json!({"mcpServers": {"branchyard": {
            "type": "stdio",
            "command": "/usr/local/bin/by",
            "args": ["mcp", "--branch", "b"],
            "env": {"BRANCHYARD_TOKEN": "t0k"},
        }}})
    );
    // Given a file, the argument is its path: nothing of the servers is on
    // the command line.
    let mut driver = ClaudeCode::new(vec!["claude".into()]);
    let opened = driver
        .open(Open {
            mcp_servers: vec![branchyard_server()],
            mcp_config_file: Some("/home/b/.branchyard/claude-mcp.json".into()),
            ..Open::new(SessionMode::Resume(session("s1")), "/workspace")
        })
        .unwrap();
    let argv = &opened.launch.argv;
    let at = argv.iter().position(|a| a == "--mcp-config").unwrap();
    assert_eq!(argv[at + 1], "/home/b/.branchyard/claude-mcp.json");
    assert_eq!(argv.iter().filter(|a| *a == "--mcp-config").count(), 1);
    assert!(!argv.iter().any(|a| a.contains("t0k")), "{argv:?}");
    assert!(argv.contains(&"--resume".to_owned()));
    // Without a file, variables would be on the command line: refused.
    let mut driver = ClaudeCode::new(vec!["claude".into()]);
    let inline = driver.open(Open {
        mcp_servers: vec![branchyard_server()],
        ..Open::new(SessionMode::Fresh, "/workspace")
    });
    assert!(
        matches!(&inline, Err(Rejected::InvalidOpen(why)) if why.contains("mcp_config_file")),
        "{inline:?}"
    );
    // A relative file is refused too.
    let mut driver = ClaudeCode::new(vec!["claude".into()]);
    let relative = driver.open(Open {
        mcp_config_file: Some("claude-mcp.json".into()),
        ..Open::new(SessionMode::Fresh, "/workspace")
    });
    assert!(matches!(relative, Err(Rejected::InvalidOpen(_))));
    // Servers without variables may still go inline.
    let plain = McpServer {
        env: Vec::new(),
        ..branchyard_server()
    };
    let mut driver = ClaudeCode::new(vec!["claude".into()]);
    let opened = driver
        .open(Open {
            mcp_servers: vec![plain.clone()],
            ..Open::new(SessionMode::Fresh, "/workspace")
        })
        .unwrap();
    let argv = &opened.launch.argv;
    let at = argv.iter().position(|a| a == "--mcp-config").unwrap();
    assert_eq!(argv[at + 1], mcp_config(&[plain], &[]));
    // No servers, no flag.
    let (_, argv, _) = open(SessionMode::Fresh);
    assert!(!argv.contains(&"--mcp-config".to_owned()));
}

fn remote() -> RemoteMcpServer {
    RemoteMcpServer {
        name: "search".into(),
        transport: RemoteTransport::Http,
        url: "https://mcp.example.com/mcp".into(),
        headers: vec![("Authorization".into(), "Bearer h3ader".into())],
    }
}

#[test]
fn remote_mcp_servers_go_in_the_file_with_their_headers() {
    let sse = RemoteMcpServer {
        name: "events".into(),
        transport: RemoteTransport::Sse,
        headers: Vec::new(),
        ..remote()
    };
    // The shape Claude Code 2.1.283 connected to, headers included, when
    // checked against a local MCP server.
    assert_eq!(
        serde_json::from_str::<Value>(&mcp_config(&[], &[remote(), sse.clone()])).unwrap(),
        json!({"mcpServers": {
            "search": {"type": "http", "url": "https://mcp.example.com/mcp",
                       "headers": {"Authorization": "Bearer h3ader"}},
            "events": {"type": "sse", "url": "https://mcp.example.com/mcp", "headers": {}},
        }})
    );
    assert!(!format!("{:?}", remote()).contains("h3ader"));
    // Headers never go on the command line.
    let mut driver = ClaudeCode::new(vec!["claude".into()]);
    let inline = driver.open(Open {
        remote_mcp_servers: vec![remote()],
        ..Open::new(SessionMode::Fresh, "/workspace")
    });
    assert!(matches!(inline, Err(Rejected::InvalidOpen(_))));
    let mut driver = ClaudeCode::new(vec!["claude".into()]);
    let opened = driver
        .open(Open {
            remote_mcp_servers: vec![remote()],
            mcp_config_file: Some("/home/b/mcp.json".into()),
            ..Open::new(SessionMode::Fresh, "/workspace")
        })
        .unwrap();
    assert!(!opened.launch.argv.iter().any(|a| a.contains("h3ader")));
    // Bad URLs, headers and repeated names are refused.
    for bad in [
        RemoteMcpServer {
            url: "ftp://x".into(),
            ..remote()
        },
        RemoteMcpServer {
            headers: vec![("Bad Header".into(), "x".into())],
            ..remote()
        },
        RemoteMcpServer {
            headers: vec![("X".into(), "a\r\nInjected: 1".into())],
            ..remote()
        },
    ] {
        let mut driver = ClaudeCode::new(vec!["claude".into()]);
        let open = Open {
            remote_mcp_servers: vec![bad.clone()],
            mcp_config_file: Some("/home/b/mcp.json".into()),
            ..Open::new(SessionMode::Fresh, "/workspace")
        };
        assert!(
            matches!(driver.open(open), Err(Rejected::InvalidOpen(_))),
            "{bad:?}"
        );
    }
    let mut driver = ClaudeCode::new(vec!["claude".into()]);
    let twice = driver.open(Open {
        mcp_servers: vec![McpServer {
            name: "search".into(),
            ..branchyard_server()
        }],
        remote_mcp_servers: vec![remote()],
        mcp_config_file: Some("/home/b/mcp.json".into()),
        ..Open::new(SessionMode::Fresh, "/workspace")
    });
    assert!(matches!(twice, Err(Rejected::InvalidOpen(why)) if why.contains("twice")));
}

#[test]
fn unusable_mcp_servers_are_rejected_before_launch() {
    let cases = [
        McpServer {
            command: "by".into(),
            ..branchyard_server()
        },
        McpServer {
            name: "has space".into(),
            ..branchyard_server()
        },
        McpServer {
            env: vec![("A=B".into(), "c".into())],
            ..branchyard_server()
        },
    ];
    for server in cases {
        let mut driver = ClaudeCode::new(vec!["claude".into()]);
        let open = Open {
            mcp_servers: vec![server.clone()],
            mcp_config_file: Some("/home/b/mcp.json".into()),
            ..Open::new(SessionMode::Fresh, "/workspace")
        };
        assert!(
            matches!(driver.open(open), Err(Rejected::InvalidOpen(_))),
            "{server:?}"
        );
    }
    let mut driver = ClaudeCode::new(vec!["claude".into()]);
    let twice = Open {
        mcp_servers: vec![branchyard_server(), branchyard_server()],
        mcp_config_file: Some("/home/b/mcp.json".into()),
        ..Open::new(SessionMode::Fresh, "/workspace")
    };
    assert!(matches!(driver.open(twice), Err(Rejected::InvalidOpen(why)) if why.contains("twice")));
}

#[test]
fn instructions_load_as_a_plugin_or_append_to_the_system_prompt() {
    let launch = |instructions: Instructions| {
        let mut driver = ClaudeCode::new(vec!["claude".into()]);
        let opened = driver
            .open(Open {
                instructions: Some(instructions),
                ..Open::new(SessionMode::Fresh, "/workspace")
            })
            .unwrap();
        opened.launch.argv
    };
    let argv = launch(Instructions {
        text: "Delegate.".into(),
        plugin_dir: Some("/repo/.branchyard/plugin".into()),
    });
    let at = argv.iter().position(|a| a == "--plugin-dir").unwrap();
    assert_eq!(argv[at + 1], "/repo/.branchyard/plugin");
    assert!(!argv.contains(&"--append-system-prompt".to_owned()));
    let argv = launch(Instructions {
        text: "Delegate.".into(),
        plugin_dir: None,
    });
    let at = argv
        .iter()
        .position(|a| a == "--append-system-prompt")
        .unwrap();
    assert_eq!(argv[at + 1], "Delegate.");
    let (_, argv, _) = open(SessionMode::Fresh);
    assert!(!argv
        .iter()
        .any(|a| a == "--plugin-dir" || a == "--append-system-prompt"));
}

fn steer_fixture(name: &str) -> Transcript {
    Transcript::load(format!(
        "{}/tests/fixtures/claude-code-2.1.283-steer-{name}.jsonl",
        env!("CARGO_MANIFEST_DIR")
    ))
}

const STEER_PROMPT: &str = "STEER-MESSAGE: also mention bananas";

fn turn_ends(events: &[Event]) -> Vec<&TurnOutcome> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::TurnEnded { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect()
}

/// Recorded against Claude Code 2.1.283 and a stand-in API: a message
/// written mid-turn is queued, delivered with the tool result into the
/// turn's next model call, and answered by the turn's one result.
#[test]
fn steered_input_joins_the_turn_at_its_next_model_call() {
    let recorded = steer_fixture("tool-boundary");
    let (mut driver, opened) = open_with(SessionMode::Fresh);
    let replayed = Replay::new(&recorded)
        .alias("/request_id")
        .alias("/uuid")
        .prompt("TOOLTURN: please start")
        .steer(STEER_PROMPT)
        .run(&mut driver, &opened);
    assert_eq!(replayed.sent, 3, "initialize, the prompt and the steer");
    assert!(replayed.unsent.is_empty());
    let events = replayed.events;
    assert!(events.contains(&Event::SteerAccepted { turn: 1, steer: 1 }));
    assert!(events.iter().any(|e| matches!(e,
        Event::MessageDelta { text, .. } if text.contains("sent a new message while you were working")
            && text.contains(STEER_PROMPT))));
    assert_eq!(turn_ends(&events), [&TurnOutcome::Completed]);
    assert!(matches!(
        events.last(),
        Some(Event::TurnEnded { turn: 1, .. })
    ));
    assert!(!events.iter().any(|e| matches!(
        e,
        Event::ProtocolViolation { .. } | Event::SteerRejected { .. }
    )));
}

/// When the turn would end without another model call, the CLI runs the
/// queued message as a follow-up with its own result: the turn stays open
/// until that result, and ends once.
#[test]
fn a_steered_follow_up_keeps_the_turn_open_until_it_is_answered() {
    let recorded = steer_fixture("follow-up");
    let (mut driver, opened) = open_with(SessionMode::Fresh);
    let replayed = Replay::new(&recorded)
        .alias("/request_id")
        .alias("/uuid")
        .prompt("TEXTTURN: please start")
        .steer(STEER_PROMPT)
        .run(&mut driver, &opened);
    assert_eq!(replayed.sent, 3);
    let events = replayed.events;
    let usage = events
        .iter()
        .filter(|e| matches!(e, Event::UsageObserved { turn: Some(1), usage } if usage.cumulative))
        .count();
    assert_eq!(usage, 2, "one per result");
    assert_eq!(turn_ends(&events), [&TurnOutcome::Completed]);
    let ended = events
        .iter()
        .position(|e| matches!(e, Event::TurnEnded { .. }))
        .unwrap();
    let answered = events
        .iter()
        .position(|e| matches!(e, Event::MessageDelta { text, .. } if text.contains(STEER_PROMPT)))
        .unwrap();
    assert!(answered < ended, "the turn ends after the steered answer");
    assert!(!events
        .iter()
        .any(|e| matches!(e, Event::ProtocolViolation { .. })));
    assert_eq!(driver.submit("next").map(|s| s.turn), Ok(2));
}

/// An interrupt with a steered message still queued cancels it with the
/// turn, instead of letting it run afterwards.
#[test]
fn an_interrupt_cancels_queued_steered_input() {
    let recorded = steer_fixture("interrupt");
    let (mut driver, opened) = open_with(SessionMode::Fresh);
    let replayed = Replay::new(&recorded)
        .alias("/request_id")
        .alias("/uuid")
        .prompt("TEXTTURN: please start")
        .steer(STEER_PROMPT)
        .interrupt()
        .run(&mut driver, &opened);
    assert_eq!(replayed.sent, 4, "initialize, prompt, steer and interrupt");
    let interrupt = recorded
        .rows
        .iter()
        .find(|r| r.frame["request"]["subtype"] == "interrupt")
        .unwrap();
    assert_eq!(interrupt.frame["request"]["cancel_queued"], true);
    let events = replayed.events;
    assert!(events.contains(&Event::SteerAccepted { turn: 1, steer: 1 }));
    assert!(events.contains(&Event::SteerRejected {
        turn: 1,
        steer: 1,
        reason: "Claude Code reported the message cancelled".into()
    }));
    assert_eq!(turn_ends(&events), [&TurnOutcome::Interrupted]);
}

#[test]
fn a_plain_interrupt_is_unchanged_without_queued_steers() {
    let mut driver = ready(SessionMode::Fresh);
    driver.submit("go").unwrap();
    let frame = decode(&driver.interrupt().unwrap()[0]);
    assert_eq!(frame["request"], json!({"subtype": "interrupt"}));
}

#[test]
fn steering_needs_a_handshake_and_a_turn_in_flight() {
    let (mut driver, _) = open_with(SessionMode::Fresh);
    assert_eq!(driver.steer("x"), Err(Rejected::NotReady));
    let mut driver = ready(SessionMode::Fresh);
    assert_eq!(driver.steer("x"), Err(Rejected::NoTurn));
    driver.submit("go").unwrap();
    let steered = decode(&driver.steer("more").unwrap()[0]);
    assert_eq!(steered["type"], "user");
    assert_eq!(steered["message"]["content"][0]["text"], "more");
    assert_ne!(steered["uuid"], Value::Null);
}

fn fixture_2_1_293(name: &str) -> Transcript {
    Transcript::load(format!(
        "{}/tests/fixtures/claude-code-2.1.293-{name}.jsonl",
        env!("CARGO_MANIFEST_DIR")
    ))
}

/// Replay a recorded 2.1.293 session, answering permissions, and closing
/// it where the recording does.
fn replay_2_1_293(name: &str, prompt: &str) -> Vec<Event> {
    let recorded = fixture_2_1_293(name);
    let (mut driver, opened) = open_with(SessionMode::Fresh);
    let mut replay = Replay::new(&recorded)
        .alias("/request_id")
        .alias("/uuid")
        .answer_permissions(PermissionDecision::Allow)
        .prompt(prompt);
    if name == "background-task" {
        replay = replay.close();
    }
    let replayed = replay.run(&mut driver, &opened);
    assert!(replayed.unsent.is_empty(), "{:?}", replayed.unsent);
    replayed.events
}

const BACKGROUND_PROMPT: &str = "Use the Bash tool with run_in_background set to true to run: \
     sleep 40; echo done. Do not wait for it or check on it. Immediately reply with the single \
     word started and end your turn.";
const PROGRESS_PROMPT: &str = "Use the Bash tool in the foreground (not in the background, \
     timeout 60000) to run exactly: timeout 34 tail -f /dev/null; echo finished. Then reply with \
     the word finished.";
const THINKING_PROMPT: &str = "Think step by step briefly about what 17*23 is. Then use the Bash \
     tool in the foreground (not in the background) to run: sleep 33; echo 391. Then reply with \
     the number.";

/// Real Claude Code 2.1.293 output: rate limits, session settings, task
/// lifecycles, tool progress, thinking tokens and turn summaries are each
/// mapped to an event or ignored on purpose, never "unrecognized".
#[test]
fn every_frame_claude_code_2_1_293_printed_is_mapped_or_ignored_on_purpose() {
    for (name, prompt) in [
        ("background-task", BACKGROUND_PROMPT),
        ("tool-progress", PROGRESS_PROMPT),
        ("thinking", THINKING_PROMPT),
    ] {
        let events = replay_2_1_293(name, prompt);
        let odd: Vec<&Event> = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    Event::Unrecognized { .. }
                        | Event::ProtocolViolation { .. }
                        | Event::Warning { .. }
                )
            })
            .collect();
        assert!(odd.is_empty(), "{name}: {odd:?}");
        assert_eq!(turn_ends(&events), [&TurnOutcome::Completed], "{name}");
    }
}

/// A background task is reported as it starts, in the set of running
/// tasks at the turn's end, and as it ends; `end_session` is what stops
/// it, and its answer is no event.
#[test]
fn background_tasks_are_reported_and_end_session_stops_them() {
    let events = replay_2_1_293("background-task", BACKGROUND_PROMPT);
    let (task, background) = events
        .iter()
        .find_map(|e| match e {
            Event::HarnessTaskStarted { task, background } => Some((task.clone(), *background)),
            _ => None,
        })
        .unwrap();
    assert!(background, "run_in_background is a background task");
    assert_eq!(task.kind.as_deref(), Some("local_bash"));
    let ended = events
        .iter()
        .position(|e| matches!(e, Event::TurnEnded { .. }))
        .unwrap();
    let running_at_end = events[..ended]
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::BackgroundTasks { running } => Some(running.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(running_at_end, std::slice::from_ref(&task));
    assert!(events[ended..].iter().any(|e| matches!(e,
        Event::HarnessTaskEnded { task_id, status, .. }
            if *task_id == task.task_id && status == "stopped")));
    assert_eq!(
        events.last(),
        Some(&Event::BackgroundTasks {
            running: Vec::new()
        })
    );
}

#[test]
fn closing_asks_claude_code_to_end_its_session() {
    let (mut driver, _) = open_with(SessionMode::Fresh);
    assert!(
        driver.close().is_empty(),
        "nothing to end before the handshake"
    );
    let mut driver = ready(SessionMode::Fresh);
    let frames = driver.close();
    assert_eq!(frames.len(), 1);
    let frame = decode(&frames[0]);
    assert_eq!(frame["type"], "control_request");
    assert_eq!(frame["request"], json!({"subtype": "end_session"}));
    let refused = json!({"type": "control_response", "response": {"subtype": "error", "request_id": frame["request_id"], "error": "no"}});
    let (events, _) = feed(&mut driver, &refused);
    assert!(matches!(&events[..], [Event::Warning { message }] if message.ends_with("no")));
}

/// A foreground command is a task too, not a background one; its
/// progress and the model's thinking are liveness, nothing more.
#[test]
fn tool_progress_and_thinking_are_progress() {
    let events = replay_2_1_293("tool-progress", PROGRESS_PROMPT);
    assert!(events.iter().any(|e| matches!(
        e,
        Event::HarnessTaskStarted {
            background: false,
            ..
        }
    )));
    let progress = |events: &[Event]| {
        events
            .iter()
            .filter(|e| matches!(e, Event::Progress { turn: Some(1) }))
            .count()
    };
    assert_eq!(progress(&events), 3, "one per tool_progress");
    let events = replay_2_1_293("thinking", THINKING_PROMPT);
    assert_eq!(progress(&events), 2, "one per thinking_tokens");
}

/// Each model call's usage is reported once as it grows, with its model,
/// so the turn can be priced before its result.
#[test]
fn each_model_calls_usage_is_reported_during_the_turn() {
    let events = replay_2_1_293("thinking", THINKING_PROMPT);
    let ended = events
        .iter()
        .position(|e| matches!(e, Event::TurnEnded { .. }))
        .unwrap();
    let calls: Vec<&branchyard_harness::Usage> = events[..ended]
        .iter()
        .filter_map(|e| match e {
            Event::UsageObserved { usage, .. } if !usage.cumulative => Some(usage),
            _ => None,
        })
        .collect();
    assert!(calls.len() >= 2, "{calls:?}");
    assert!(calls
        .iter()
        .all(|u| u.model.as_deref() == Some("claude-sonnet-5-5") && u.cost_usd.is_none()));
    let written: u64 = calls
        .iter()
        .map(|u| u.cache_write_tokens.unwrap_or(0) + u.cache_write_1h_tokens.unwrap_or(0))
        .sum();
    assert_eq!(written, 35146, "the result's cache writes, call by call");
}

/// Claude Code prints whole text blocks, not fragments: a block that
/// follows another with no tool call between starts a new paragraph.
#[test]
fn text_blocks_in_a_row_are_paragraphs() {
    let mut driver = ready(SessionMode::Fresh);
    driver.submit("go").unwrap();
    let text = |id: &str, text: &str| json!({"type": "assistant", "message": {"id": id, "content": [{"type": "text", "text": text}]}});
    let tool = json!({"type": "assistant", "message": {"id": "m2", "content": [{"type": "tool_use", "id": "t", "name": "Bash"}]}});
    let mut said = Vec::new();
    for message in [
        text("m1", "First."),
        text("m1", "Second."),
        tool,
        text("m3", "Third."),
    ] {
        let (events, _) = feed(&mut driver, &message);
        said.extend(events.into_iter().filter_map(|e| match e {
            Event::MessageDelta { text, .. } => Some(text),
            _ => None,
        }));
    }
    assert_eq!(said, ["First.", "\n\nSecond.", "Third."]);
}
