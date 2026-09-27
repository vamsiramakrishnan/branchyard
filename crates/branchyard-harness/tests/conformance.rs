//! The conformance kit itself, and the driver contract for every profile.

use branchyard_harness::claude_code::ClaudeCode;
use branchyard_harness::conformance::{
    assert_contract_greeted, check_frame, Direction, Replay, Transcript,
};
use branchyard_harness::profiles::{Protocol, PROFILES};
use branchyard_harness::{Driver, Event, Open, PermissionDecision, SessionMode, TurnOutcome};
use serde_json::{json, Value};

fn fresh() -> Open {
    Open {
        mode: SessionMode::Fresh,
        cwd: "/workspace".into(),
        model: None,
        mcp_servers: Vec::new(),
        instructions: None,
        mcp_config_file: None,
    }
}

/// Play the harness side of a fresh handshake for `protocol`.
fn answer(protocol: Protocol) -> impl Fn(&Value) -> Vec<Value> {
    move |frame| {
        let id = &frame["id"];
        let result = match (protocol, frame["method"].as_str()) {
            (Protocol::ClaudeStreamJson, _) if frame["request"]["subtype"] == "initialize" => {
                return vec![json!({"type": "control_response",
                    "response": {"subtype": "success", "request_id": frame["request_id"], "response": {}}})]
            }
            (Protocol::CodexAppServer, Some("initialize")) => json!({}),
            (Protocol::CodexAppServer, Some("thread/start")) => json!({"thread": {"id": "t1"}}),
            (Protocol::Acp, Some("initialize")) => json!({"protocolVersion": 1}),
            (Protocol::Acp, Some("session/new")) => json!({"sessionId": "s1"}),
            (Protocol::PiRpc, _) if frame["type"] == "get_state" => {
                return vec![
                    json!({"id": frame["id"], "type": "response", "command": "get_state",
                    "success": true, "data": {"sessionId": "s1"}}),
                ]
            }
            _ => return Vec::new(),
        };
        let mut reply = json!({"id": id, "result": result});
        if protocol == Protocol::Acp {
            reply["jsonrpc"] = json!("2.0");
        }
        vec![reply]
    }
}

/// What `protocol`'s harness prints before reading anything.
fn greeting(protocol: Protocol) -> Vec<Value> {
    match protocol {
        Protocol::AntigravityStreamJson => {
            vec![json!({"event": "init", "conversation_id": "c1", "init": {"cwd": "/workspace"}})]
        }
        Protocol::AmpStreamJson => {
            vec![
                json!({"type": "system", "subtype": "init", "session_id": "T-1", "cwd": "/workspace"}),
            ]
        }
        _ => Vec::new(),
    }
}

#[test]
fn every_profile_follows_the_driver_contract() {
    for profile in PROFILES {
        let mut driver = profile.driver();
        assert_contract_greeted(
            driver.as_mut(),
            fresh(),
            &greeting(profile.protocol),
            answer(profile.protocol),
        );
    }
}

#[test]
fn transcripts_parse_notes_and_rows_and_refuse_other_shapes() {
    let transcript = Transcript::parse(
        "{\"note\":\"tool 1.0\"}\n\n{\"dir\":\"out\",\"frame\":{\"a\":1}}\n{\"dir\":\"in\",\"frame\":{\"b\":2}}\n",
    )
    .unwrap();
    assert_eq!(transcript.note.as_deref(), Some("tool 1.0"));
    assert_eq!(
        transcript
            .rows
            .iter()
            .map(|r| (r.direction, r.frame.clone()))
            .collect::<Vec<_>>(),
        [
            (Direction::Out, json!({"a": 1})),
            (Direction::In, json!({"b": 2}))
        ]
    );
    for (text, error) in [
        (
            "{\"dir\":\"out\",\"frame\":{}}\n{\"note\":\"late\"}",
            "first row",
        ),
        ("{\"dir\":\"sideways\",\"frame\":{}}", "\"dir\""),
        ("{\"dir\":\"in\"}", "no \"frame\""),
        ("not json", "line 1"),
    ] {
        let message = Transcript::parse(text).unwrap_err();
        assert!(message.contains(error), "{message}");
    }
}

#[test]
fn frames_are_one_json_value_on_one_terminated_line() {
    assert_eq!(check_frame(b"{\"a\":1}\n"), Ok(json!({"a": 1})));
    for bad in [&b"{\"a\":1}"[..], b"{}\n{}\n", b"{} {}\n", b"Loading\n"] {
        assert!(
            check_frame(bad).is_err(),
            "{:?}",
            String::from_utf8_lossy(bad)
        );
    }
}

fn claude() -> (ClaudeCode, branchyard_harness::Opened) {
    let mut driver = ClaudeCode::new(vec!["claude".into()]);
    let opened = driver.open(fresh()).unwrap();
    (driver, opened)
}

fn rows(rows: &[(&str, Value)]) -> Transcript {
    let text: Vec<String> = rows
        .iter()
        .map(|(dir, frame)| json!({"dir": dir, "frame": frame}).to_string())
        .collect();
    Transcript::parse(&text.join("\n")).unwrap()
}

/// A permission round trip: aliases carry the driver's request ID and UUID
/// into later incoming frames, and the recorded answer must match the
/// driver's.
#[test]
fn replay_answers_permissions_and_substitutes_aliases() {
    let transcript = rows(&[
        (
            "out",
            json!({"type": "control_request", "request_id": "req-1", "request": {"subtype": "initialize"}}),
        ),
        (
            "in",
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": "req-1", "response": {}}}),
        ),
        (
            "out",
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "ls"}]},
                "parent_tool_use_id": null, "session_id": "", "uuid": "recorded-uuid"}),
        ),
        (
            "in",
            json!({"type": "control_request", "request_id": "perm-1",
                "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "ls"}}}),
        ),
        (
            "out",
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": "perm-1",
                "response": {"behavior": "allow", "updatedInput": {"command": "ls"}}}}),
        ),
        (
            "in",
            json!({"type": "result", "subtype": "success", "is_error": false, "user_message_uuid": "recorded-uuid"}),
        ),
    ]);
    let (mut driver, opened) = claude();
    let replayed = Replay::new(&transcript)
        .alias("/request_id")
        .alias("/uuid")
        .prompt("ls")
        .answer_permissions(PermissionDecision::Allow)
        .run(&mut driver, &opened);
    assert_eq!(replayed.sent, 3);
    assert!(replayed.ours(&json!("recorded-uuid")).is_some());
    assert_eq!(
        replayed.events.last(),
        Some(&Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Completed
        })
    );
}

#[test]
#[should_panic(expected = "outgoing frame 0 differs")]
fn replay_rejects_a_differing_outgoing_frame() {
    let transcript = rows(&[(
        "out",
        json!({"type": "control_request", "request_id": "req-1", "request": {"subtype": "hello"}}),
    )]);
    let (mut driver, opened) = claude();
    Replay::new(&transcript)
        .alias("/request_id")
        .run(&mut driver, &opened);
}

#[test]
#[should_panic(expected = "did not write")]
fn replay_rejects_a_recorded_frame_the_driver_never_writes() {
    let transcript = rows(&[
        (
            "out",
            json!({"type": "control_request", "request_id": "req-1", "request": {"subtype": "initialize"}}),
        ),
        ("out", json!({"type": "user"})),
    ]);
    let (mut driver, opened) = claude();
    Replay::new(&transcript)
        .alias("/request_id")
        .run(&mut driver, &opened);
}

#[test]
fn drivers_without_a_verified_projection_refuse_servers_and_instructions() {
    let server = branchyard_harness::McpServer {
        name: "branchyard".into(),
        command: "/usr/local/bin/by".into(),
        args: vec!["mcp".into()],
        env: Vec::new(),
    };
    let instructions = branchyard_harness::Instructions {
        text: "Delegate with by spawn.".into(),
        plugin_dir: None,
    };
    let unprojected = [
        Protocol::AntigravityStreamJson,
        Protocol::PiRpc,
        Protocol::AmpStreamJson,
    ];
    let mut checked = 0;
    for profile in PROFILES
        .iter()
        .filter(|p| unprojected.contains(&p.protocol))
    {
        for open in [
            Open {
                mcp_servers: vec![server.clone()],
                ..fresh()
            },
            Open {
                instructions: Some(instructions.clone()),
                ..fresh()
            },
        ] {
            let refused = profile.driver().open(open).err();
            assert!(
                matches!(refused, Some(branchyard_harness::Rejected::Unsupported(_))),
                "{} must refuse, not drop, what it cannot pass: {refused:?}",
                profile.id
            );
        }
        profile.driver().open(fresh()).expect("a plain open works");
        checked += 1;
    }
    assert_eq!(checked, 3);
}
