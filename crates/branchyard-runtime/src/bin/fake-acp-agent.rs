//! A fake ACP v1 agent for this crate's tests. Not a harness.
//!
//! It answers `initialize`, `session/new`, `session/resume`, `session/load`
//! and `session/prompt` over newline-delimited JSON-RPC on stdio, and exits
//! when stdin closes. Resuming or loading a session whose ID starts with
//! `missing` fails as not found. A prompt replies `echo: <prompt>` unless it contains a
//! keyword:
//!
//! - `PERMISSION`: asks `session/request_permission` and replies `allowed` or
//!   `denied` by the option selected. Any `WRITE` in the prompt happens only
//!   if allowed.
//! - `WRITE path=content`: writes `content` and a newline to `path` in its
//!   working directory, creating parent directories; may repeat. Replies
//!   `wrote <path>` for each.
//! - `WHOAMI`: replies `session <id> resumed=<bool>`, where `resumed` says
//!   whether this process opened the session with `session/resume` or
//!   `session/load`.
//! - `HANG`: replies nothing until `session/cancel`, then ends `cancelled`.
//! - `BACKGROUND`: starts `sleep 30` in its process group and replies with its
//!   pid.
//! - `EXIT`: writes to stderr and exits mid-turn.
//! - `GARBAGE`: writes a non-JSON line before the normal reply.
//! - `ENV A B`: replies `NAME=value` or `NAME unset` for `HOME` and each name
//!   after the keyword.

use std::io::{self, BufRead, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

fn send(message: &Value) {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{message}");
    let _ = stdout.flush();
}

fn reply(id: &Value, result: Value) {
    send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn chunk(session: &str, text: &str) {
    send(&json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session,
            "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}},
        },
    }));
}

/// The prompt in flight: its request id, and the permission request it
/// waits on, if any.
struct Active {
    id: Value,
    permission: Option<u64>,
    /// The prompt, for writes that wait on the permission.
    text: String,
}

fn main() {
    let mut session = "fake-session-1".to_owned();
    let mut resumed = false;
    let mut active: Option<Active> = None;
    let mut next_request = 1000;
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            eprintln!("fake-acp-agent: unreadable line {line:?}");
            continue;
        };
        let id = message.get("id").cloned();
        let params = &message["params"];
        match (message["method"].as_str(), id) {
            (Some("initialize"), Some(id)) => reply(
                &id,
                json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {"loadSession": true, "sessionCapabilities": {"resume": {}}},
                }),
            ),
            (Some("session/new"), Some(id)) => reply(&id, json!({"sessionId": session})),
            (Some("session/resume" | "session/load"), Some(id))
                if params["sessionId"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("missing")) =>
            {
                send(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32002, "message": "Resource not found", "data": params["sessionId"]},
                }));
            }
            (Some("session/resume" | "session/load"), Some(id)) => {
                session = params["sessionId"].as_str().unwrap_or_default().to_owned();
                resumed = true;
                reply(&id, json!({}));
            }
            (Some("session/prompt"), Some(id)) => {
                let text = params["prompt"][0]["text"].as_str().unwrap_or_default();
                active = prompt(&session, resumed, id, text, &mut next_request);
            }
            (Some("session/cancel"), None) => {
                if let Some(turn) = active.take() {
                    reply(&turn.id, json!({"stopReason": "cancelled"}));
                }
            }
            (Some(method), Some(id)) => send(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("fake agent does not handle {method}")},
            })),
            (None, Some(id)) => {
                let waiting = active
                    .as_ref()
                    .is_some_and(|turn| turn.permission.is_some_and(|p| id == json!(p)));
                let outcome = &message["result"]["outcome"];
                // A cancelled answer comes with session/cancel, which ends
                // the turn.
                if waiting && outcome["outcome"] == "selected" {
                    let turn = active.take().expect("checked above");
                    let answer = match outcome["optionId"].as_str() {
                        Some("allow") => {
                            write_files(&session, &turn.text);
                            "allowed"
                        }
                        _ => "denied",
                    };
                    chunk(&session, answer);
                    reply(&turn.id, json!({"stopReason": "end_turn"}));
                }
            }
            _ => {}
        }
    }
}

/// Carry out every `WRITE path=content` in `text`, relative to the current
/// directory.
fn write_files(session: &str, text: &str) {
    let mut words = text.split_whitespace();
    while let Some(word) = words.next() {
        if word != "WRITE" {
            continue;
        }
        let Some((path, content)) = words.next().and_then(|w| w.split_once('=')) else {
            chunk(session, "WRITE needs path=content\n");
            continue;
        };
        let path = std::path::Path::new(path);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(path, format!("{content}\n")) {
            Ok(()) => chunk(session, &format!("wrote {}\n", path.display())),
            Err(error) => chunk(
                session,
                &format!("write {} failed: {error}\n", path.display()),
            ),
        }
    }
}

/// Act on a prompt; returns the turn if it stays open.
fn prompt(
    session: &str,
    resumed: bool,
    id: Value,
    text: &str,
    next_request: &mut u64,
) -> Option<Active> {
    if text.contains("GARBAGE") {
        println!("this is not JSON");
    }
    if text.contains("EXIT") {
        chunk(session, "exiting");
        eprintln!("fake-acp-agent: exiting mid-turn as asked");
        std::process::exit(3);
    }
    if text.contains("HANG") {
        return Some(Active {
            id,
            permission: None,
            text: text.to_owned(),
        });
    }
    if text.contains("PERMISSION") {
        *next_request += 1;
        send(&json!({
            "jsonrpc": "2.0",
            "id": *next_request,
            "method": "session/request_permission",
            "params": {
                "sessionId": session,
                "toolCall": {"toolCallId": "call-1", "title": "write marker", "rawInput": {"path": "marker.txt"}},
                "options": [
                    {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                ],
            },
        }));
        return Some(Active {
            id,
            permission: Some(*next_request),
            text: text.to_owned(),
        });
    }
    write_files(session, text);
    let answer = if text.contains("BACKGROUND") {
        // Same process group, detached from the protocol pipes.
        match Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => format!("background pid {}", child.id()),
            Err(error) => format!("background failed: {error}"),
        }
    } else if text.contains("WHOAMI") {
        format!("session {session} resumed={resumed}")
    } else if text.contains("WRITE") {
        String::new()
    } else if let Some(rest) = text.split("ENV").nth(1) {
        std::iter::once("HOME")
            .chain(rest.split_whitespace())
            .map(|name| match std::env::var(name) {
                Ok(value) => format!("{name}={value}\n"),
                Err(_) => format!("{name} unset\n"),
            })
            .collect()
    } else {
        format!("echo: {text}")
    };
    if !answer.is_empty() {
        chunk(session, &answer);
    }
    reply(&id, json!({"stopReason": "end_turn"}));
    None
}
