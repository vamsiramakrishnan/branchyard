//! ACP driver against a recorded claude-agent-acp 0.81.2 handshake. Every
//! frame the driver writes is checked against the official
//! `agent-client-protocol-schema` request types.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
use agent_client_protocol_schema::v1::{
    AuthenticateRequest, CancelNotification, InitializeRequest, LoadSessionRequest,
    McpServer as AcpMcpServer, NewSessionRequest, PromptRequest, RequestPermissionResponse,
    ResumeSessionRequest,
};
use branchyard_harness::acp::{Acp, REPLAY_FENCE};
use branchyard_harness::conformance::{
    assert_conforms, decode, decode_all, feed, handshake, Replay, Transcript,
};
use branchyard_harness::{
    Driver, Event, Instructions, McpServer, NativeSession, Open, Opened, PermissionDecision,
    Rejected, RemoteMcpServer, RemoteTransport, SessionMode, TurnOutcome,
};
use serde_json::{json, Value};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/claude-agent-acp-0.81.2-initialize.jsonl"
);

fn fresh() -> Open {
    Open {
        mode: SessionMode::Fresh,
        cwd: "/workspace".into(),
        model: None,
        max_budget_usd: None,
        mcp_servers: Vec::new(),
        instructions: None,
        mcp_config_file: None,
        remote_mcp_servers: Vec::new(),
    }
}

fn open_with(mode: SessionMode) -> (Acp, Opened) {
    let mut driver = Acp::new(vec!["gemini".into(), "--acp".into()]);
    let opened = driver.open(Open { mode, ..fresh() }).unwrap();
    assert_eq!(opened.launch.argv, ["gemini", "--acp"]);
    (driver, opened)
}

fn open(mode: SessionMode) -> (Acp, Value) {
    let (driver, opened) = open_with(mode);
    (driver, decode(&opened.frames[0]))
}

/// Initialize with `capabilities`, returning the session request it sends.
fn initialized(mode: SessionMode, capabilities: Value) -> (Acp, Vec<Event>, Value) {
    let (mut driver, initialize) = open(mode);
    let (events, frames) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1, "agentCapabilities": capabilities}}),
    );
    (
        driver,
        events,
        frames.into_iter().next().unwrap_or(Value::Null),
    )
}

/// Play an ACP agent's side of a fresh handshake.
fn answer(frame: &Value) -> Vec<Value> {
    match frame["method"].as_str() {
        Some("initialize") => vec![
            json!({"jsonrpc": "2.0", "id": frame["id"], "result": {"protocolVersion": 1, "agentCapabilities": {}}}),
        ],
        Some("session/new") => {
            vec![json!({"jsonrpc": "2.0", "id": frame["id"], "result": {"sessionId": "s1"}})]
        }
        _ => Vec::new(),
    }
}

fn ready() -> Acp {
    let (mut driver, opened) = open_with(SessionMode::Fresh);
    let events = handshake(&mut driver, &opened.frames, answer);
    assert_eq!(events[0], Event::Ready);
    driver
}

fn session(id: &str) -> NativeSession {
    NativeSession::new(id).unwrap()
}

#[test]
fn initialize_matches_a_recorded_handshake_and_opens_a_session() {
    let recorded = Transcript::load(FIXTURE);
    let (mut driver, opened) = open_with(SessionMode::Fresh);
    let initialize = decode(&opened.frames[0]);
    assert_conforms::<InitializeRequest>(&initialize, "/params");
    // The adapter accepted exactly this frame, apart from the request ID and
    // Branchyard's own version (the recording says 0.0.1).
    let replayed = Replay::new(&recorded)
        .alias("/id")
        .ignore("/params/clientInfo/version")
        .run(&mut driver, &opened);
    assert_eq!(replayed.sent, 1);
    assert!(replayed.events.is_empty());
    let frames = replayed.unsent;
    assert_eq!(frames[0]["method"], "session/new");
    assert_conforms::<NewSessionRequest>(&frames[0], "/params");

    let (events, _) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": frames[0]["id"], "result": {"sessionId": "sess-1"}}),
    );
    assert_eq!(
        events,
        vec![
            Event::Ready,
            Event::SessionStarted {
                session: session("sess-1"),
                forked_from: None
            }
        ]
    );
}

#[test]
fn resume_prefers_session_resume_then_load_and_never_starts_fresh() {
    let (_, _, request) = initialized(
        SessionMode::Resume(session("old")),
        json!({"loadSession": true, "sessionCapabilities": {"resume": {}}}),
    );
    assert_eq!(request["method"], "session/resume");
    assert_conforms::<ResumeSessionRequest>(&request, "/params");

    let (mut driver, _, request) = initialized(
        SessionMode::Resume(session("old")),
        json!({"loadSession": true}),
    );
    assert_eq!(request["method"], "session/load");
    assert_conforms::<LoadSessionRequest>(&request, "/params");
    // History replayed during load is not reported.
    let replay = json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "old",
        "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "earlier"}}}});
    assert_eq!(feed(&mut driver, &replay).0, vec![]);
    let (events, frames) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": request["id"], "result": {}}),
    );
    // The load's answer is fenced before the session opens.
    assert_eq!(events, vec![]);
    assert_eq!(frames[0]["method"], REPLAY_FENCE);
    let (events, _) = feed(&mut driver, &method_not_found(&frames[0]));
    assert_eq!(
        events[1],
        Event::SessionStarted {
            session: session("old"),
            forked_from: None
        }
    );

    let (_, events, request) = initialized(SessionMode::Resume(session("old")), json!({}));
    assert_eq!(request, Value::Null);
    assert!(matches!(&events[0], Event::OpenFailed { reason } if reason.contains("neither")));
}

/// What every agent answers a request it does not know.
fn method_not_found(request: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32601,
        "message": format!("\"Method not found\": {}", request["method"])}})
}

/// A `session/update` of the session `old`.
fn old_update(update: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "old", "update": update}})
}

/// Gemini CLI 0.65.0 answers `session/load` after the first update of the
/// history it replays and streams the rest after the answer
/// (`streamHistory` is not awaited). Everything until the fence's answer is
/// history, so the next turn reports only what it does itself.
#[test]
fn history_replayed_after_the_load_answer_is_not_the_next_turns_output() {
    let (mut driver, _, load) = initialized(
        SessionMode::Resume(session("old")),
        json!({"loadSession": true}),
    );
    let history = [
        json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "Create a.txt"}}),
        json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "**Planning**"}}),
        json!({"sessionUpdate": "agent_message_chunk",
               "content": {"type": "text", "text": "[Thought: true][Function Call: write_file]"}}),
        json!({"sessionUpdate": "tool_call", "toolCallId": "write_file-1", "status": "completed",
               "title": "WriteFile", "content": [], "kind": "edit"}),
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Created a.txt."}}),
    ];
    let (events, frames) = feed(&mut driver, &old_update(history[0].clone()));
    assert_eq!((events, frames), (vec![], vec![]));
    let (events, frames) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": load["id"], "result": {"modes": {}}}),
    );
    assert_eq!(events, vec![]);
    assert_eq!(frames.len(), 1);
    let fence = frames[0].clone();
    assert_eq!(fence["method"], REPLAY_FENCE);
    assert_ne!(fence["id"], load["id"]);
    // Not ready yet: the history is still coming.
    assert!(matches!(driver.submit("next"), Err(Rejected::NotReady)));
    for update in &history[1..] {
        let (events, frames) = feed(&mut driver, &old_update(update.clone()));
        assert_eq!((events, frames), (vec![], vec![]), "{update}");
    }
    let (events, frames) = feed(&mut driver, &method_not_found(&fence));
    assert!(frames.is_empty());
    assert_eq!(
        events,
        vec![
            Event::Ready,
            Event::SessionStarted {
                session: session("old"),
                forked_from: None
            }
        ]
    );
    let submitted = driver.submit("Now create b.txt.").unwrap();
    let live = [
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Creating b.txt."}}),
        json!({"sessionUpdate": "tool_call", "toolCallId": "write_file-2", "status": "in_progress",
               "title": "WriteFile", "kind": "edit"}),
    ];
    let events: Vec<Event> = live
        .into_iter()
        .flat_map(|update| feed(&mut driver, &old_update(update)).0)
        .collect();
    assert_eq!(
        events,
        vec![
            Event::MessageDelta {
                turn: submitted.turn,
                text: "Creating b.txt.".into()
            },
            Event::ToolStarted {
                turn: submitted.turn,
                call_id: "write_file-2".into(),
                name: "WriteFile".into()
            }
        ]
    );
}

/// An agent that answers the fence with a result, not an error, is fenced
/// all the same; a fresh session and a `session/resume` need no fence.
#[test]
fn only_a_load_is_fenced_and_any_answer_ends_the_fence() {
    let (mut driver, _, load) = initialized(
        SessionMode::Resume(session("old")),
        json!({"loadSession": true}),
    );
    let (_, frames) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": load["id"], "result": {}}),
    );
    let (events, _) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": frames[0]["id"], "result": {}}),
    );
    assert_eq!(events[0], Event::Ready);

    let (mut driver, _, resume) = initialized(
        SessionMode::Resume(session("old")),
        json!({"loadSession": true, "sessionCapabilities": {"resume": {}}}),
    );
    let (events, frames) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": resume["id"], "result": {}}),
    );
    assert!(frames.is_empty());
    assert_eq!(events[0], Event::Ready);

    let (mut driver, _, new) = initialized(SessionMode::Fresh, json!({"loadSession": true}));
    let (events, frames) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": new["id"], "result": {"sessionId": "s1"}}),
    );
    assert!(frames.is_empty());
    assert_eq!(events[0], Event::Ready);
}

#[test]
fn fork_and_model_selection_are_rejected_before_launch() {
    let mut driver = Acp::new(vec!["agent".into()]);
    let open = |mode, model| Open {
        mode,
        cwd: "/workspace".into(),
        model,
        max_budget_usd: None,
        mcp_servers: Vec::new(),
        instructions: None,
        mcp_config_file: None,
        remote_mcp_servers: Vec::new(),
    };
    assert!(matches!(
        driver.open(open(SessionMode::Fork(session("p")), None)),
        Err(Rejected::Unsupported(_))
    ));
    assert!(matches!(
        driver.open(open(SessionMode::Fresh, Some("m".into()))),
        Err(Rejected::Unsupported(_))
    ));
}

#[test]
fn prompts_stream_updates_and_end_with_the_stop_reason() {
    let mut driver = ready();
    let submitted = driver.submit("Say hello.").unwrap();
    let prompt = decode(&submitted.frames[0]);
    assert_conforms::<PromptRequest>(&prompt, "/params");

    let updates = [
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hello"}}),
        json!({"sessionUpdate": "tool_call", "toolCallId": "call-1", "title": "Run tests", "kind": "execute"}),
        json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}}),
    ];
    let events: Vec<Event> = updates
        .into_iter()
        .flat_map(|update| {
            feed(
                &mut driver,
                &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": update}}),
            )
            .0
        })
        .collect();
    assert_eq!(
        events,
        vec![
            Event::MessageDelta {
                turn: 1,
                text: "Hello".into()
            },
            Event::ToolStarted {
                turn: 1,
                call_id: "call-1".into(),
                name: "Run tests".into()
            },
        ]
    );
    let (events, _) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "max_tokens"}}),
    );
    assert_eq!(
        events,
        vec![Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::LimitReached {
                limit: "max_tokens".into()
            }
        }]
    );
}

fn permission_request(id: u64) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
    "sessionId": "s1",
    "toolCall": {"toolCallId": "call-1", "title": "Edit src/lib.rs", "rawInput": {"path": "src/lib.rs"}},
    "options": [
        {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
        {"optionId": "once", "name": "Allow", "kind": "allow_once"},
        {"optionId": "no", "name": "Reject", "kind": "reject_once"}
    ]}})
}

#[test]
fn permissions_select_one_time_options_only() {
    let mut driver = ready();
    driver.submit("edit").unwrap();
    let (events, _) = feed(&mut driver, &permission_request(40));
    let Event::PermissionRequested {
        turn: Some(1),
        request,
    } = &events[0]
    else {
        panic!("{events:?}")
    };
    assert_eq!(request.tool, "Edit src/lib.rs");
    assert_eq!(request.input, json!({"path": "src/lib.rs"}));
    let reply = decode(
        &driver
            .respond(&request.key, PermissionDecision::Allow)
            .unwrap()[0],
    );
    assert_conforms::<RequestPermissionResponse>(&reply, "/result");
    // allow_once, never the standing allow_always rule.
    assert_eq!(
        reply["result"]["outcome"],
        json!({"outcome": "selected", "optionId": "once"})
    );

    let mut only_always = permission_request(41);
    only_always["params"]["options"] =
        json!([{"optionId": "always", "name": "Always", "kind": "allow_always"}]);
    let (events, _) = feed(&mut driver, &only_always);
    let Event::PermissionRequested { request, .. } = &events[0] else {
        panic!()
    };
    assert!(matches!(
        driver.respond(&request.key, PermissionDecision::Allow),
        Err(Rejected::Unsupported(_))
    ));
}

#[test]
fn cancel_answers_outstanding_permissions_and_awaits_the_stop_reason() {
    let mut driver = ready();
    let prompt = decode(&driver.submit("edit").unwrap().frames[0]);
    feed(&mut driver, &permission_request(50));
    let frames = decode_all(&driver.interrupt().unwrap());
    assert_eq!(frames[0]["method"], "session/cancel");
    assert_conforms::<CancelNotification>(&frames[0], "/params");
    assert_eq!(frames[1]["id"], 50);
    assert_conforms::<RequestPermissionResponse>(&frames[1], "/result");
    assert_eq!(
        frames[1]["result"]["outcome"],
        json!({"outcome": "cancelled"})
    );

    let (events, _) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "cancelled"}}),
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
fn client_filesystem_and_terminal_requests_are_refused() {
    let mut driver = ready();
    for method in ["fs/read_text_file", "terminal/create"] {
        let (events, frames) = feed(
            &mut driver,
            &json!({"jsonrpc": "2.0", "id": 60, "method": method, "params": {"path": "/etc/passwd"}}),
        );
        assert_eq!(
            events,
            vec![Event::UnsupportedRequest {
                method: method.into()
            }]
        );
        assert_eq!(frames[0]["error"]["code"], -32601);
    }
}

/// Qwen Code 0.26.0 asks for input queued mid-turn once per session and
/// carries on at "method not found": declined as any request, but not
/// reported as an unsupported one on every branch.
#[test]
fn an_optional_agent_extension_is_declined_without_an_event() {
    let mut driver = ready();
    let (events, frames) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": 61, "method": "craft/drainMidTurnQueue",
                "params": {"sessionId": "s1", "promptId": "p1"}}),
    );
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(frames[0]["id"], 61);
    assert_eq!(frames[0]["error"]["code"], -32601);
}

#[test]
fn protocol_mismatches_fail_the_open() {
    let (mut driver, initialize) = open(SessionMode::Fresh);
    let (events, frames) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 2}}),
    );
    assert!(frames.is_empty());
    assert!(matches!(&events[0], Event::OpenFailed { .. }));
    let (events, _) = feed(&mut driver, &json!({"id": 9, "result": {}}));
    assert!(matches!(&events[0], Event::ProtocolViolation { .. }));
}

#[test]
fn open_failures_keep_the_error_data() {
    let (mut driver, _, request) = initialized(SessionMode::Fresh, json!({}));
    let (events, _) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32603, "message": "Internal error",
            "data": {"details": "Claude Code process exited with code 1"}}}),
    );
    assert_eq!(
        events,
        vec![Event::OpenFailed {
            reason: "Internal error: Claude Code process exited with code 1".into()
        }]
    );
}

#[test]
fn the_claude_acp_profile_keeps_permission_bypass_unavailable() {
    use branchyard_harness::profiles;
    let mut driver = profiles::by_id("claude-code-acp").unwrap().driver();
    let opened = driver
        .open(Open {
            mode: SessionMode::Fresh,
            cwd: "/workspace".into(),
            model: None,
            max_budget_usd: None,
            mcp_servers: Vec::new(),
            instructions: None,
            mcp_config_file: None,
            remote_mcp_servers: Vec::new(),
        })
        .unwrap();
    let initialize = decode(&opened.frames[0]);
    let (_, frames) = feed(
        driver.as_mut(),
        &json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
    );
    assert_eq!(
        frames[0]["params"]["_meta"],
        json!({"claudeCode": {"options": {"allowDangerouslySkipPermissions": false}}})
    );
    assert_conforms::<NewSessionRequest>(&frames[0], "/params");
    // Other ACP profiles send no agent-specific options.
    let mut driver = profiles::by_id("gemini-cli-acp").unwrap().driver();
    let opened = driver
        .open(Open {
            mode: SessionMode::Fresh,
            cwd: "/workspace".into(),
            model: None,
            max_budget_usd: None,
            mcp_servers: Vec::new(),
            instructions: None,
            mcp_config_file: None,
            remote_mcp_servers: Vec::new(),
        })
        .unwrap();
    let initialize = decode(&opened.frames[0]);
    let (_, frames) = feed(
        driver.as_mut(),
        &json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
    );
    assert!(frames[0]["params"].get("_meta").is_none());
}

#[test]
fn updates_for_another_session_are_violations() {
    let mut driver = ready();
    driver.submit("Say hello.").unwrap();
    let (events, _) = feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "other",
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "stale"}}}}),
    );
    assert!(
        matches!(&events[..], [Event::ProtocolViolation { detail }] if detail.contains("other"))
    );
}

#[test]
fn mcp_servers_are_stdio_servers_on_new_resume_and_load() {
    let server = McpServer {
        name: "branchyard".into(),
        command: "/usr/local/bin/by".into(),
        args: vec!["mcp".into()],
        env: vec![("BRANCHYARD_TOKEN".into(), "t0k".into())],
    };
    let cases = [
        (SessionMode::Fresh, json!({}), "session/new"),
        (
            SessionMode::Resume(session("old")),
            json!({"sessionCapabilities": {"resume": {}}}),
            "session/resume",
        ),
        (
            SessionMode::Resume(session("old")),
            json!({"loadSession": true}),
            "session/load",
        ),
    ];
    for (mode, capabilities, method) in cases {
        let mut driver = Acp::new(vec!["agent".into()]);
        let opened = driver
            .open(Open {
                mcp_servers: vec![server.clone()],
                mode,
                ..fresh()
            })
            .unwrap();
        let initialize = decode(&opened.frames[0]);
        let (_, frames) = feed(
            &mut driver,
            &json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1, "agentCapabilities": capabilities}}),
        );
        let request = &frames[0];
        assert_eq!(request["method"], method);
        match method {
            "session/new" => {
                let _: NewSessionRequest = assert_conforms(request, "/params");
            }
            "session/resume" => {
                let _: ResumeSessionRequest = assert_conforms(request, "/params");
            }
            _ => {
                let _: LoadSessionRequest = assert_conforms(request, "/params");
            }
        }
        let servers = &request["params"]["mcpServers"];
        assert_eq!(
            servers,
            &json!([{"name": "branchyard", "command": "/usr/local/bin/by", "args": ["mcp"],
                "env": [{"name": "BRANCHYARD_TOKEN", "value": "t0k"}]}])
        );
        let parsed: AcpMcpServer = serde_json::from_value(servers[0].clone()).unwrap();
        let AcpMcpServer::Stdio(stdio) = parsed else {
            panic!("not a stdio server: {parsed:?}")
        };
        assert_eq!(
            (
                stdio.name.as_str(),
                stdio.command.to_str(),
                stdio.env[0].value.as_str()
            ),
            ("branchyard", Some("/usr/local/bin/by"), "t0k")
        );
    }
    let mut driver = Acp::new(vec!["agent".into()]);
    let relative = Open {
        mcp_servers: vec![McpServer {
            command: "by".into(),
            ..server
        }],
        ..fresh()
    };
    assert!(matches!(
        driver.open(relative),
        Err(Rejected::InvalidOpen(_))
    ));
}

#[test]
fn instructions_precede_only_the_first_prompt_in_delimiters() {
    let mut driver = Acp::new(vec!["agent".into()]);
    let opened = driver
        .open(Open {
            instructions: Some(Instructions {
                text: "Delegate with by.".into(),
                plugin_dir: None,
            }),
            ..fresh()
        })
        .unwrap();
    let events = handshake(&mut driver, &opened.frames, answer);
    assert!(events.contains(&Event::Ready));
    let first = decode(&driver.submit("Fix it.").unwrap().frames[0]);
    let _: PromptRequest = assert_conforms(&first, "/params");
    assert_eq!(
        first["params"]["prompt"][0]["text"],
        "<branchyard-instructions>\nDelegate with by.\n</branchyard-instructions>\n\nFix it."
    );
    feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "id": first["id"], "result": {"stopReason": "end_turn"}}),
    );
    let second = decode(&driver.submit("Again.").unwrap().frames[0]);
    assert_eq!(second["params"]["prompt"][0]["text"], "Again.");
}

#[test]
fn remote_mcp_servers_go_to_agents_that_advertise_them() {
    let server = RemoteMcpServer {
        name: "search".into(),
        transport: RemoteTransport::Http,
        url: "https://mcp.example.com/mcp".into(),
        headers: vec![("Authorization".into(), "Bearer h3ader".into())],
    };
    let sse = RemoteMcpServer {
        name: "events".into(),
        transport: RemoteTransport::Sse,
        ..server.clone()
    };
    let session_new = |capabilities: Value| {
        let mut driver = Acp::new(vec!["agent".into()]);
        let opened = driver
            .open(Open {
                remote_mcp_servers: vec![server.clone(), sse.clone()],
                ..fresh()
            })
            .unwrap();
        let initialize = decode(&opened.frames[0]);
        feed(
            &mut driver,
            &json!({"jsonrpc": "2.0", "id": initialize["id"],
                    "result": {"protocolVersion": 1, "agentCapabilities": capabilities}}),
        )
    };
    // claude-agent-acp 0.81.2 advertises both.
    let (events, frames) = session_new(json!({"mcpCapabilities": {"http": true, "sse": true}}));
    assert!(events.is_empty(), "{events:?}");
    let request = &frames[0];
    let _: NewSessionRequest = assert_conforms(request, "/params");
    let servers = &request["params"]["mcpServers"];
    assert_eq!(
        servers,
        &json!([
            {"type": "http", "name": "search", "url": "https://mcp.example.com/mcp",
             "headers": [{"name": "Authorization", "value": "Bearer h3ader"}]},
            {"type": "sse", "name": "events", "url": "https://mcp.example.com/mcp",
             "headers": [{"name": "Authorization", "value": "Bearer h3ader"}]},
        ])
    );
    assert!(matches!(
        serde_json::from_value::<AcpMcpServer>(servers[0].clone()).unwrap(),
        AcpMcpServer::Http(_)
    ));
    assert!(matches!(
        serde_json::from_value::<AcpMcpServer>(servers[1].clone()).unwrap(),
        AcpMcpServer::Sse(_)
    ));
    // An agent without SSE fails the open, naming the server.
    let (events, frames) = session_new(json!({"mcpCapabilities": {"http": true}}));
    assert!(frames.is_empty());
    assert!(
        matches!(&events[..], [Event::OpenFailed { reason }] if reason.contains("SSE") && reason.contains("events")),
        "{events:?}"
    );
}

fn steer_fixture(name: &str) -> Transcript {
    Transcript::load(format!(
        "{}/tests/fixtures/claude-agent-acp-0.81.2-{name}.jsonl",
        env!("CARGO_MANIFEST_DIR")
    ))
}

fn claude_code_acp() -> (Box<dyn Driver>, Opened) {
    let mut driver = branchyard_harness::profiles::by_id("claude-code-acp")
        .unwrap()
        .driver();
    let opened = driver.open(fresh()).unwrap();
    (driver, opened)
}

/// Recorded against claude-agent-acp 0.81.2 and a stand-in API: the agent
/// advertises `_meta.steering`, injects the input into the running prompt,
/// and answers the prompt once.
#[test]
fn steering_is_injected_where_the_agent_advertises_it() {
    let recorded = steer_fixture("steer");
    let (mut driver, opened) = claude_code_acp();
    let replayed = Replay::new(&recorded)
        .alias("/id")
        .ignore("/params/clientInfo/version")
        .prompt("TEXTTURN: please start")
        .steer("STEER-MESSAGE: bananas")
        .run(driver.as_mut(), &opened);
    assert_eq!(
        replayed.sent, 4,
        "initialize, session/new, prompt and steer"
    );
    let steer = recorded
        .rows
        .iter()
        .find(|r| r.frame["method"] == "_session/steering")
        .unwrap();
    assert_conforms::<PromptRequest>(&steer.frame, "/params");
    let events = replayed.events;
    assert!(events.contains(&Event::SteerAccepted { turn: 1, steer: 1 }));
    assert!(events.iter().any(|e| matches!(e,
        Event::MessageDelta { text, .. } if text.contains("STEER-MESSAGE: bananas"))));
    let ends: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::TurnEnded { .. }))
        .collect();
    assert_eq!(
        ends,
        [&Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Completed
        }]
    );
}

/// Input that reaches the agent after its prompt ended comes back
/// undelivered (`promptRequired`), never as a turn of its own.
#[test]
fn steering_after_the_prompt_ended_is_returned_undelivered() {
    let recorded = steer_fixture("steer-idle");
    let (mut driver, opened) = claude_code_acp();
    // Up to the prompt response: the driver steers while it still thinks
    // the prompt runs, as it would had the response been in flight.
    let prompt_ended = recorded
        .rows
        .iter()
        .position(|r| r.frame["result"]["stopReason"].is_string())
        .unwrap();
    let before = Transcript {
        note: None,
        rows: recorded.rows[..prompt_ended].to_vec(),
    };
    let replayed = Replay::new(&before)
        .alias("/id")
        .ignore("/params/clientInfo/version")
        .prompt("TEXTTURN: please start")
        .run(driver.as_mut(), &opened);
    assert_eq!(replayed.sent, 3);
    let steer = decode(&driver.steer("STEER-MESSAGE: bananas").unwrap()[0]);
    assert_eq!(
        steer["params"]["_meta"],
        json!({"steering": {"idleBehavior": "promptRequired"}})
    );
    let mut events = Vec::new();
    for row in &recorded.rows[prompt_ended..] {
        if row.direction != branchyard_harness::conformance::Direction::In {
            continue;
        }
        let mut frame = row.frame.clone();
        if frame["result"]["outcome"].is_string() {
            frame["id"] = steer["id"].clone();
        } else if frame["result"]["stopReason"].is_string() {
            frame["id"] = replayed.ours(&frame["id"]).cloned().unwrap_or(json!(3));
        }
        events.extend(feed(driver.as_mut(), &frame).0);
    }
    assert!(events.contains(&Event::SteerRejected {
        turn: 1,
        steer: 1,
        reason: "the agent's prompt had already ended".into()
    }));
}

#[test]
fn an_agent_without_the_steering_extension_refuses_it() {
    let mut driver = ready();
    driver.submit("go").unwrap();
    assert_eq!(
        driver.steer("x"),
        Err(Rejected::Unsupported(
            "the agent does not advertise the _session/steering extension".into()
        ))
    );
}

#[test]
fn permission_requests_name_the_tool_by_name_kind_or_title() {
    // A preset matches tool names, so a request carries the agent's name
    // for the tool when it gives one (in the request, or in the call's
    // earlier update), else its kind under the `acp:` names, and only then
    // its title.
    let mut driver = ready();
    driver.submit("edit").unwrap();
    let request = |id: u64, tool_call: Value| {
        json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
            "sessionId": "s1", "toolCall": tool_call,
            "options": [{"optionId": "once", "name": "Allow", "kind": "allow_once"}]}})
    };
    let tool_of = |driver: &mut Acp, message: &Value| {
        let (events, _) = feed(driver, message);
        let Event::PermissionRequested { request, .. } = &events[0] else {
            panic!("{events:?}")
        };
        request.tool.clone()
    };
    let named = request(
        50,
        json!({"toolCallId": "c-1", "title": "Writing to notes/x.md", "name": "write_file", "kind": "edit"}),
    );
    assert_eq!(tool_of(&mut driver, &named), "write_file");
    let by_kind = request(
        51,
        json!({"toolCallId": "c-2", "title": "Running tests", "kind": "execute"}),
    );
    assert_eq!(tool_of(&mut driver, &by_kind), "acp:execute");
    // The call's update named it, the request does not: the name wins.
    feed(
        &mut driver,
        &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1",
            "update": {"sessionUpdate": "tool_call", "toolCallId": "c-3", "title": "Edit", "kind": "edit", "name": "replace"}}}),
    );
    let announced = request(
        52,
        json!({"toolCallId": "c-3", "title": "Editing src/lib.rs"}),
    );
    assert_eq!(tool_of(&mut driver, &announced), "replace");
    let only_title = request(53, json!({"toolCallId": "c-4", "title": "Edit src/lib.rs"}));
    assert_eq!(tool_of(&mut driver, &only_title), "Edit src/lib.rs");
}

fn update_s1(update: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": update}})
}

fn ask(id: u64, tool_call: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
        "sessionId": "s1", "toolCall": tool_call,
        "options": [{"optionId": "once", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "no", "name": "Reject", "kind": "reject_once"}]}})
}

/// The permission request `message` raises; the test fails without one.
fn asked(driver: &mut Acp, message: &Value) -> branchyard_harness::PermissionRequest {
    let (events, _) = feed(driver, message);
    events
        .into_iter()
        .find_map(|event| match event {
            Event::PermissionRequested { request, .. } => Some(request),
            _ => None,
        })
        .unwrap()
}

/// DeepSeek Harness asks with `toolCall: {toolCallId}` alone, after a
/// `tool_call` update that described the call (title `bash`, kind
/// `other`, its input): the request is read with what the update said, so
/// the policy, `--ask` and the log see `bash` and its command, not
/// `acp:other` and an id.
#[test]
fn a_permission_request_naming_only_its_call_is_joined_to_the_calls_update() {
    let mut driver = ready();
    driver.submit("touch a file").unwrap();
    feed(
        &mut driver,
        &update_s1(
            json!({"sessionUpdate": "tool_call", "toolCallId": "call_943d", "title": "bash",
            "kind": "other", "status": "in_progress", "rawInput": {"command": "touch marker.txt"}}),
        ),
    );
    let request = asked(&mut driver, &ask(60, json!({"toolCallId": "call_943d"})));
    assert_eq!(request.tool, "bash");
    assert_eq!(request.input, json!({"command": "touch marker.txt"}));

    // A later `tool_call_update` adds to what the call's update said.
    feed(
        &mut driver,
        &update_s1(json!({"sessionUpdate": "tool_call", "toolCallId": "c-2",
            "title": "Writing to notes/x.md", "kind": "edit", "status": "pending"})),
    );
    feed(
        &mut driver,
        &update_s1(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "c-2",
            "rawInput": {"file_path": "notes/x.md", "content": "x"}}),
        ),
    );
    let request = asked(&mut driver, &ask(61, json!({"toolCallId": "c-2"})));
    assert_eq!(request.tool, "acp:edit");
    assert_eq!(
        request.input,
        json!({"file_path": "notes/x.md", "content": "x"})
    );

    // The request's own fields win over the update's.
    feed(
        &mut driver,
        &update_s1(
            json!({"sessionUpdate": "tool_call", "toolCallId": "c-3", "title": "bash",
            "kind": "other", "rawInput": {"command": "ls"}}),
        ),
    );
    let request = asked(
        &mut driver,
        &ask(
            62,
            json!({"toolCallId": "c-3", "rawInput": {"command": "ls -la"}}),
        ),
    );
    assert_eq!(request.input, json!({"command": "ls -la"}));

    // With no input anywhere, the input is the call as described: its
    // title and the paths it touches (Gemini CLI asks so).
    let gemini = json!({"toolCallId": "write_file-9", "status": "pending",
        "title": "Writing to notes/y.md", "kind": "edit",
        "locations": [{"path": "/workspace/notes/y.md"}],
        "content": [{"type": "diff", "path": "/workspace/notes/y.md", "oldText": null, "newText": "y\n"}]});
    let request = asked(&mut driver, &ask(63, gemini.clone()));
    assert_eq!(request.tool, "acp:edit");
    assert_eq!(request.input, gemini);

    // An id the turn never described stays an id.
    let request = asked(&mut driver, &ask(64, json!({"toolCallId": "unknown-1"})));
    assert_eq!(request.tool, "unknown-1");
    assert_eq!(request.input, json!({"toolCallId": "unknown-1"}));
}

/// A call of kind `other` (or none) whose title is one word is named by
/// it: that title is the tool's name. A title with spaces is a description,
/// so the kind's `acp:other` stands.
#[test]
fn a_one_word_title_names_a_call_of_kind_other() {
    let mut driver = ready();
    driver.submit("go").unwrap();
    let cases = [
        (
            json!({"toolCallId": "a", "title": "write", "kind": "other"}),
            "write",
        ),
        (
            json!({"toolCallId": "b", "title": "run_shell_command"}),
            "run_shell_command",
        ),
        (
            json!({"toolCallId": "c", "title": "Run the tests", "kind": "other"}),
            "acp:other",
        ),
        (
            json!({"toolCallId": "d", "title": "notes/x.md", "kind": "other"}),
            "acp:other",
        ),
        (
            json!({"toolCallId": "e", "title": "bash", "kind": "execute"}),
            "acp:execute",
        ),
    ];
    for (n, (tool_call, tool)) in cases.into_iter().enumerate() {
        let request = asked(&mut driver, &ask(70 + n as u64, tool_call.clone()));
        assert_eq!(request.tool, tool, "{tool_call}");
    }
}

/// A resume of Gemini CLI's profile, up to the agent's `initialize` answer
/// with `auth_methods`; returns the driver and the `session/load` it sends.
fn gemini_resume(mode: SessionMode, auth_methods: Value) -> (Box<dyn Driver>, Value) {
    use branchyard_harness::profiles;
    let mut driver = profiles::by_id("gemini-cli-acp").unwrap().driver();
    let opened = driver.open(Open { mode, ..fresh() }).unwrap();
    let initialize = decode(&opened.frames[0]);
    let (_, frames) = feed(
        driver.as_mut(),
        &json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1,
            "agentCapabilities": {"loadSession": true}, "authMethods": auth_methods}}),
    );
    (driver, frames.into_iter().next().unwrap())
}

fn gemini_auth_methods() -> Value {
    json!([
        {"id": "oauth-personal", "name": "Log in with Google"},
        {"id": "gemini-api-key", "name": "Gemini API key"},
        {"id": "vertex-ai", "name": "Vertex AI"}
    ])
}

fn auth_required(request: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "Authentication required"}})
}

#[test]
fn a_resume_refused_for_authentication_authenticates_once_and_asks_again() {
    let (mut driver, load) =
        gemini_resume(SessionMode::Resume(session("old")), gemini_auth_methods());
    assert_eq!(load["method"], "session/load");
    let (events, frames) = feed(driver.as_mut(), &auth_required(&load));
    assert_eq!(events, vec![]);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["method"], "authenticate");
    assert_eq!(frames[0]["params"], json!({"methodId": "gemini-api-key"}));
    assert_conforms::<AuthenticateRequest>(&frames[0], "/params");

    let (events, frames) = feed(
        driver.as_mut(),
        &json!({"jsonrpc": "2.0", "id": frames[0]["id"], "result": {}}),
    );
    assert_eq!(events, vec![]);
    assert_eq!(frames[0]["method"], "session/load");
    assert_eq!(frames[0]["params"], load["params"]);
    assert_ne!(frames[0]["id"], load["id"]);

    let (events, frames) = feed(
        driver.as_mut(),
        &json!({"jsonrpc": "2.0", "id": frames[0]["id"], "result": {}}),
    );
    assert_eq!(events, vec![]);
    assert_eq!(frames[0]["method"], REPLAY_FENCE);
    let (events, _) = feed(driver.as_mut(), &method_not_found(&frames[0]));
    assert_eq!(events[0], Event::Ready);
    assert_eq!(
        events[1],
        Event::SessionStarted {
            session: session("old"),
            forked_from: None
        }
    );
}

/// Gemini CLI's profile launches `gemini --acp`: 0.65.0's `--help` calls
/// `--experimental-acp` deprecated.
#[test]
fn gemini_cli_starts_with_the_acp_flag() {
    use branchyard_harness::profiles;
    let profile = profiles::by_id("gemini-cli-acp").unwrap();
    assert_eq!(profile.command, ["gemini", "--acp"]);
}

#[test]
fn a_resume_refused_again_after_authenticating_fails_the_open() {
    let (mut driver, load) =
        gemini_resume(SessionMode::Resume(session("old")), gemini_auth_methods());
    let (_, frames) = feed(driver.as_mut(), &auth_required(&load));
    let (_, frames) = feed(
        driver.as_mut(),
        &json!({"jsonrpc": "2.0", "id": frames[0]["id"], "result": {}}),
    );
    let (events, frames) = feed(driver.as_mut(), &auth_required(&frames[0]));
    assert!(frames.is_empty());
    assert_eq!(
        events,
        vec![Event::OpenFailed {
            reason: "Authentication required".into()
        }]
    );
}

#[test]
fn a_failed_authenticate_says_which_method_and_why() {
    let (mut driver, load) =
        gemini_resume(SessionMode::Resume(session("old")), gemini_auth_methods());
    let (_, frames) = feed(driver.as_mut(), &auth_required(&load));
    let (events, frames) = feed(
        driver.as_mut(),
        &json!({"jsonrpc": "2.0", "id": frames[0]["id"], "error": {"code": -32000,
            "message": "Gemini API key is missing or not configured."}}),
    );
    assert!(frames.is_empty());
    assert_eq!(
        events,
        vec![Event::OpenFailed {
            reason: "authenticate with gemini-api-key failed: Gemini API key is missing or not configured."
                .into()
        }]
    );
}

#[test]
fn authentication_is_tried_only_for_a_resume_the_agent_offers_a_method_for() {
    // A fresh session that is refused stays refused: no switching the login.
    let (mut driver, new) = gemini_resume(SessionMode::Fresh, gemini_auth_methods());
    assert_eq!(new["method"], "session/new");
    let (events, frames) = feed(driver.as_mut(), &auth_required(&new));
    assert!(frames.is_empty());
    assert!(
        matches!(&events[0], Event::OpenFailed { reason } if reason == "Authentication required")
    );

    // The agent does not advertise the pinned method.
    let (mut driver, load) = gemini_resume(
        SessionMode::Resume(session("old")),
        json!([{"id": "oauth-personal", "name": "Log in with Google"}]),
    );
    let (events, frames) = feed(driver.as_mut(), &auth_required(&load));
    assert!(frames.is_empty());
    assert!(matches!(&events[0], Event::OpenFailed { .. }));

    // Another error code is not about authentication.
    let (mut driver, load) =
        gemini_resume(SessionMode::Resume(session("old")), gemini_auth_methods());
    let (events, frames) = feed(
        driver.as_mut(),
        &json!({"jsonrpc": "2.0", "id": load["id"], "error": {"code": -32603, "message": "Internal error"}}),
    );
    assert!(frames.is_empty());
    assert!(matches!(&events[0], Event::OpenFailed { reason } if reason == "Internal error"));

    // A profile that pins no method never authenticates.
    use branchyard_harness::profiles;
    let mut driver = profiles::by_id("claude-code-acp").unwrap().driver();
    let opened = driver
        .open(Open {
            mode: SessionMode::Resume(session("old")),
            ..fresh()
        })
        .unwrap();
    let initialize = decode(&opened.frames[0]);
    let (_, frames) = feed(
        driver.as_mut(),
        &json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1,
            "agentCapabilities": {"loadSession": true}, "authMethods": gemini_auth_methods()}}),
    );
    let (events, frames) = feed(driver.as_mut(), &auth_required(&frames[0]));
    assert!(frames.is_empty());
    assert!(matches!(&events[0], Event::OpenFailed { .. }));
}
