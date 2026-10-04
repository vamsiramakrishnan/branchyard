//! `branchyard-mcp` over stdio, speaking JSON-RPC as an MCP client would.

#![allow(clippy::let_underscore_must_use, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::process::{Command, Stdio};

use branchyard_mcp::TOOLS;
use common::{Client, SERVER};
use serde_json::{json, Value};

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "branchyard-mcp-protocol-{tag}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn initialize_lists_the_tools_and_calls_answer_without_an_engine() {
    let root = temp_root("init");
    let mut client = Client::start(&root, "b", "t0k");
    let init = client.initialize();
    let result = &init["result"];
    assert_eq!(result["protocolVersion"], "2025-06-18");
    assert_eq!(result["serverInfo"]["name"], "branchyard");
    assert!(result["capabilities"]["tools"].is_object());
    assert!(result["instructions"]
        .as_str()
        .unwrap()
        .contains("descendants"));

    let listed = client.request("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, TOOLS);
    for tool in tools {
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object", "{tool}");
        assert_eq!(schema["additionalProperties"], false, "{tool}");
        assert!(tool["description"].as_str().unwrap().len() > 20);
    }
    let spawn = &tools[0];
    assert_eq!(spawn["inputSchema"]["required"], json!(["prompt"]));
    assert_eq!(tools[1]["annotations"]["readOnlyHint"], true);

    // No turn is running under this root, so every call is refused as a
    // tool error the model can read, not a protocol error.
    let (error, text) = client.call("inspect", json!({}));
    assert!(error);
    assert!(text.contains("no running turn"), "{text}");
    let (error, _) = client.call("spawn", json!({"prompt": "x"}));
    assert!(error);

    let unknown = client.request("tools/call", json!({"name": "rm", "arguments": {}}));
    assert!(unknown["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no tool named rm"));
    let _ = std::fs::remove_dir_all(&root);
    assert!(client.finish().success());
}

#[test]
fn a_token_that_matches_no_running_turn_or_another_branch_is_refused_locally() {
    let root = temp_root("forged");
    let dir = root.join(".branchyard/delegation");
    std::fs::create_dir_all(&dir).unwrap();
    // A token file as the engine writes it, for a broker nobody listens on.
    std::fs::write(
        dir.join("b.json"),
        json!({"branch": "b", "token": "real", "broker": dir.join("none.sock"), "pid": 1})
            .to_string(),
    )
    .unwrap();
    let mut forged = Client::start(&root, "b", "forged");
    forged.initialize();
    let (error, text) = forged.call("inspect", json!({}));
    assert!(error);
    assert!(text.contains("no running turn"), "{text}");
    // The real token, claimed for another branch.
    let mut claimed = Client::start(&root, "other", "real");
    claimed.initialize();
    let (error, text) = claimed.call("inspect", json!({}));
    assert!(error);
    assert!(text.contains("issued to b, not other"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn usage_errors_exit_2_before_speaking_mcp() {
    let run = |args: &[&str], token: Option<&str>| {
        let mut command = Command::new(SERVER);
        command
            .args(args)
            .env_remove("BRANCHYARD_DELEGATION")
            .stdin(Stdio::null());
        if let Some(token) = token {
            command.env("BRANCHYARD_DELEGATION", token);
        }
        command.output().unwrap()
    };
    let no_token = run(&["--root", "/tmp", "--branch", "b"], None);
    assert_eq!(no_token.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&no_token.stderr).contains("BRANCHYARD_DELEGATION is not set"));
    let no_branch = run(&["--root", "/tmp"], Some("t"));
    assert_eq!(no_branch.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&no_branch.stderr).contains("--branch <NAME>"));
    let extra = run(&["--root=/tmp", "--branch=b", "--bogus"], Some("t"));
    assert_eq!(extra.status.code(), Some(2));
    let help = run(&["--help"], None);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: branchyard-mcp"));
    let _: Value = json!(null);
}
