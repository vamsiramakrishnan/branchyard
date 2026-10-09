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
//! - `ORPHAN`: like `HANG`, and also appends a line to `orphan.log` in its
//!   working directory, starts `sleep 60` in its process group, replies
//!   `orphan <own pid> <sleep pid>`, and from then on outlives its closed
//!   stdin by 60 seconds: a harness that ignores its engine's death.
//! - `BACKGROUND`: starts `sleep 30` in its process group and replies with its
//!   pid.
//! - `EXIT`: writes to stderr and exits mid-turn.
//! - `GARBAGE`: writes a non-JSON line before the normal reply.
//! - `ENV A B`: replies `NAME=value` or `NAME unset` for `HOME` and each name
//!   after the keyword.
//! - `MCP <tool> <json>`, one per line: starts the first MCP server the
//!   client passed in `mcpServers` (its command, arguments and variables),
//!   initializes it, calls `tools/call` with the JSON as arguments, and
//!   replies `mcp <tool>: <text>` or `mcp <tool> error: <text>` for each
//!   line. `MCP tools` replies the listed tool names instead,
//!   `MCP wait <branch>` calls `inspect` until the branch is neither
//!   running nor waiting (for its prerequisites or on its children; `MCP
//!   wait {...}` calls the `wait` tool instead), and `MCP started <branch>` calls `events` until the branch's prompt
//!   has been recorded, which it is just before the prompt is submitted.
//!   Without a server it replies `mcp: no server`.
//! - `SH <command>`, one per line: runs the command with `sh -c` in its
//!   working directory and environment, and replies `sh: <exit status>`
//!   followed by the command's output, for each line.
//! - `REPLY_FILE <path>`, on a line of its own, checked before every other
//!   keyword: replies the file's contents verbatim (or `reply file <path>:
//!   <error>`) and does nothing else; a judge's canned verdict.
//! - `REPLY_SEQUENCE <dir>`, on a line of its own, checked right after
//!   `REPLY_FILE`: replies the contents of the first file in `dir` by name
//!   and removes it, so a run of prompts gets a run of canned answers (a
//!   judge that finds a goal unmet, then met); `reply sequence <dir>:
//!   empty` when none is left.
//! - `INSTRUCTED`: replies `instructed=<bool>`, whether a prompt this
//!   process received began with Branchyard's instructions preamble. The
//!   preamble is removed before any keyword is looked for.
//! - `AWAIT_STEER`: replies `waiting for steering`, then waits until a
//!   `_session/steering` request arrives, answers it `injected`, replies
//!   `steered: <text>` and ends the turn. Until then it ends only on
//!   `session/cancel`.
//!
//! It advertises the `_session/steering` extension
//! (`_meta.steering.supported`) as claude-agent-acp does. Steering while a
//! turn is open answers `injected` and replies `steered: <text>` into it;
//! with none open, it answers `promptRequired`. With
//! `FAKE_ACP_NO_STEER=1` it neither advertises nor handles the extension.
//!
//! With `FAKE_ACP_WAKE=<file>` in its environment, a prompt that is
//! Branchyard's automatic wake of a parent waiting on its children
//! (it contains `<branchyard-wake>`) is acted on as if it were the file's
//! contents instead, so a test scripts what a woken parent does; without
//! it, a wake is echoed like any prompt.
//!
//! With `FAKE_ACP_SILENT=1` in its environment it never answers
//! `initialize`, so no prompt is ever submitted to it.
//!
//! Once the directory it was started in is gone (its test ended and dropped
//! its temporary directory), it exits whatever it is doing, and, as the
//! leader of its process group, kills that group first: a test leaves no
//! agent, and no command one of its `SH` lines started, behind.
//!
//! Started as `fake-acp-agent --record-launch FILE ARG...`, standing in for
//! a harness a driver launches with its own arguments, it writes to `FILE`
//! as JSON what another process could see of it and exits: its command
//! line as `/proc/self/cmdline` shows it, the names (not values) of its
//! environment, and, for an argument after `--mcp-config` that names a
//! file, that file's mode and the servers it lists.

#![allow(clippy::expect_used, clippy::let_underscore_must_use)] // ratchet: branchyard-runtime
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

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
    /// `AWAIT_STEER`: the first steer ends the turn.
    awaits_steer: bool,
}

/// Branchyard's ACP instructions preamble, as the driver writes it.
const PREAMBLE_OPEN: &str = "<branchyard-instructions>";
const PREAMBLE_CLOSE: &str = "</branchyard-instructions>";

/// Record what the process list shows of this process, and exit.
fn record_launch(file: &str) {
    let cmdline = std::fs::read("/proc/self/cmdline").unwrap_or_default();
    let cmdline: Vec<String> = cmdline
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    let mut env: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    env.sort();
    let mcp_config = cmdline
        .iter()
        .position(|a| a == "--mcp-config")
        .and_then(|at| cmdline.get(at + 1))
        .filter(|path| path.starts_with('/'))
        .map(|path| {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .map(|m| format!("{:o}", m.permissions().mode() & 0o777))
                .ok();
            let servers: Vec<String> = std::fs::read_to_string(path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|config| config["mcpServers"].as_object().cloned())
                .map(|servers| servers.keys().cloned().collect())
                .unwrap_or_default();
            json!({"path": path, "mode": mode, "servers": servers})
        });
    let record = json!({"cmdline": cmdline, "env": env, "mcp_config": mcp_config});
    std::fs::write(file, record.to_string()).expect("the launch record is writable");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--record-launch") {
        record_launch(args.get(2).expect("--record-launch FILE"));
        return;
    }
    exit_with_directory();
    let mut instructed = false;
    // The last instructions preamble, for SHOW_INSTRUCTIONS.
    let mut preamble = String::new();
    let mut servers = Value::Null;
    let mut session = "fake-session-1".to_owned();
    let mut resumed = false;
    let mut active: Option<Active> = None;
    let mut next_request = 1000;
    let silent = std::env::var_os("FAKE_ACP_SILENT").is_some_and(|v| v == "1");
    let steering = std::env::var_os("FAKE_ACP_NO_STEER").is_none_or(|v| v != "1");
    let mut stubborn = false;
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            eprintln!("fake-acp-agent: unreadable line {line:?}");
            continue;
        };
        let id = message.get("id").cloned();
        let params = &message["params"];
        match (message["method"].as_str(), id) {
            (Some("initialize"), Some(_)) if silent => {}
            (Some("initialize"), Some(id)) => {
                let mut result = json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {"loadSession": true, "sessionCapabilities": {"resume": {}}},
                });
                if steering {
                    result["_meta"] = json!({"steering": {"supported": true}});
                }
                reply(&id, result)
            }
            (Some("_session/steering"), Some(id)) if steering => {
                let text = params["prompt"][0]["text"].as_str().unwrap_or_default();
                match active.take() {
                    Some(turn) => {
                        reply(&id, json!({"outcome": "injected"}));
                        chunk(&session, &format!("steered: {text}"));
                        if turn.awaits_steer {
                            reply(&turn.id, json!({"stopReason": "end_turn"}));
                        } else {
                            active = Some(turn);
                        }
                    }
                    None => reply(
                        &id,
                        json!({"outcome": "promptRequired", "reason": "noRunningTurn"}),
                    ),
                }
            }
            (Some("session/new"), Some(id)) => {
                servers = params["mcpServers"].clone();
                reply(&id, json!({"sessionId": session}));
            }
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
                servers = params["mcpServers"].clone();
                session = params["sessionId"].as_str().unwrap_or_default().to_owned();
                resumed = true;
                reply(&id, json!({}));
            }
            (Some("session/prompt"), Some(id)) => {
                let mut text = params["prompt"][0]["text"].as_str().unwrap_or_default();
                if let Some(rest) = text.strip_prefix(PREAMBLE_OPEN) {
                    if let Some((given, prompt)) = rest.split_once(PREAMBLE_CLOSE) {
                        instructed = true;
                        preamble = given.to_owned();
                        text = prompt.trim_start();
                    }
                }
                let woken;
                if text.contains("<branchyard-wake>") {
                    if let Some(script) = std::env::var_os("FAKE_ACP_WAKE") {
                        woken = std::fs::read_to_string(&script).unwrap_or_else(|e| {
                            format!("wake script {}: {e}", script.to_string_lossy())
                        });
                        text = &woken;
                    }
                }
                if let Some(path) = text
                    .lines()
                    .find_map(|line| line.strip_prefix("REPLY_FILE "))
                {
                    let path = path.trim();
                    let reply_text = std::fs::read_to_string(path)
                        .unwrap_or_else(|e| format!("reply file {path}: {e}"));
                    chunk(&session, &reply_text);
                    reply(&id, json!({"stopReason": "end_turn"}));
                    continue;
                }
                if let Some(dir) = text
                    .lines()
                    .find_map(|line| line.strip_prefix("REPLY_SEQUENCE "))
                {
                    chunk(&session, &next_in_sequence(dir.trim()));
                    reply(&id, json!({"stopReason": "end_turn"}));
                    continue;
                }
                if text.contains("SHOW_INSTRUCTIONS") {
                    chunk(&session, &format!("instructions: {preamble}"));
                    reply(&id, json!({"stopReason": "end_turn"}));
                    continue;
                }
                if text.contains("INSTRUCTED") {
                    chunk(&session, &format!("instructed={instructed}"));
                    reply(&id, json!({"stopReason": "end_turn"}));
                    continue;
                }
                if text.lines().any(|line| line.starts_with("SH ")) {
                    chunk(&session, &shell(text));
                    reply(&id, json!({"stopReason": "end_turn"}));
                    continue;
                }
                if text.lines().any(|line| line.starts_with("MCP ")) {
                    chunk(&session, &mcp(&servers[0], text));
                    reply(&id, json!({"stopReason": "end_turn"}));
                    continue;
                }
                if text.contains("ORPHAN") {
                    stubborn = true;
                    chunk(&session, &orphan());
                    active = Some(Active {
                        id,
                        permission: None,
                        text: text.to_owned(),
                        awaits_steer: false,
                    });
                    continue;
                }
                if text.contains("AWAIT_STEER") {
                    chunk(&session, "waiting for steering");
                    active = Some(Active {
                        id,
                        permission: None,
                        text: text.to_owned(),
                        awaits_steer: true,
                    });
                    continue;
                }
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
    if stubborn {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// Watch the directory this process started in, on a thread; once it is
/// gone, kill this process group if this process leads it, and exit.
fn exit_with_directory() {
    let Ok(dir) = std::env::current_dir() else {
        return;
    };
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(100));
        if dir.exists() {
            continue;
        }
        eprintln!("fake-acp-agent: {} is gone; exiting", dir.display());
        #[cfg(unix)]
        if rustix::process::getpgrp() == rustix::process::getpid() {
            branchyard_support::kill_group(std::process::id());
        }
        std::process::exit(4);
    });
}

/// For `REPLY_SEQUENCE`: the first file in `dir` by name, removed.
fn next_in_sequence(dir: &str) -> String {
    let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(error) => return format!("reply sequence {dir}: {error}"),
    };
    files.sort();
    let Some(first) = files.into_iter().find(|p| p.is_file()) else {
        return format!("reply sequence {dir}: empty");
    };
    let text = std::fs::read_to_string(&first)
        .unwrap_or_else(|e| format!("reply sequence {}: {e}", first.display()));
    branchyard_support::cleanup_file(&first);
    text
}

/// For `ORPHAN`: note the prompt in `orphan.log`, start a child in this
/// process group, and say which processes to look for.
fn orphan() -> String {
    let noted = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("orphan.log")
        .and_then(|mut log| writeln!(log, "prompt received"));
    if let Err(error) = noted {
        return format!("orphan.log failed: {error}");
    }
    match Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => format!("orphan {} {}", std::process::id(), child.id()),
        Err(error) => format!("orphan failed: {error}"),
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
            awaits_steer: false,
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
            awaits_steer: false,
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

/// A minimal MCP client over a server's stdio.
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl McpClient {
    fn start(server: &Value) -> Result<McpClient, String> {
        let command = server["command"]
            .as_str()
            .ok_or("the server has no command")?;
        let mut process = Command::new(command);
        for arg in server["args"].as_array().into_iter().flatten() {
            process.arg(arg.as_str().unwrap_or_default());
        }
        for var in server["env"].as_array().into_iter().flatten() {
            process.env(
                var["name"].as_str().unwrap_or_default(),
                var["value"].as_str().unwrap_or_default(),
            );
        }
        let mut child = process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start {command}: {e}"))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = BufReader::new(child.stdout.take().expect("piped"));
        let mut client = McpClient {
            child,
            stdin,
            stdout,
            next: 0,
        };
        client.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "fake-acp-agent", "version": "0"}}),
        )?;
        client.notify("notifications/initialized")?;
        Ok(client)
    }

    fn notify(&mut self, method: &str) -> Result<(), String> {
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "method": method})
        )
        .map_err(|e| e.to_string())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{request}").map_err(|e| e.to_string())?;
        loop {
            let mut line = String::new();
            if self
                .stdout
                .read_line(&mut line)
                .map_err(|e| e.to_string())?
                == 0
            {
                return Err("the server closed its output".into());
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                return Err(format!("the server wrote a non-JSON line: {line:?}"));
            };
            if message["id"] != json!(id) {
                continue; // a notification or a request of its own
            }
            if let Some(error) = message.get("error") {
                return Err(format!("JSON-RPC error {error}"));
            }
            return Ok(message["result"].clone());
        }
    }

    /// `(is_error, text)` of one tool call.
    fn call(&mut self, tool: &str, arguments: Value) -> Result<(bool, String), String> {
        let result = self.request("tools/call", json!({"name": tool, "arguments": arguments}))?;
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        Ok((result["isError"] == true, text.to_owned()))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        branchyard_support::best_effort("kill child", self.child.kill());
        branchyard_support::best_effort("reap child", self.child.wait());
    }
}

/// Carry out every `MCP` line of `text` and describe the results.
fn mcp(server: &Value, text: &str) -> String {
    if server.is_null() {
        return "mcp: no server\n".into();
    }
    let mut client = match McpClient::start(server) {
        Ok(client) => client,
        Err(error) => return format!("mcp: {error}\n"),
    };
    let mut out = String::new();
    for line in text.lines().filter_map(|l| l.strip_prefix("MCP ")) {
        let (tool, rest) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        let outcome = match tool {
            "tools" => client.request("tools/list", json!({})).map(|result| {
                let names: Vec<&str> = result["tools"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t["name"].as_str())
                    .collect();
                (false, names.join(","))
            }),
            // `MCP wait <branch>` polls; `MCP wait {...}` calls the tool.
            "wait" if !rest.trim_start().starts_with('{') => wait(&mut client, rest.trim()),
            "started" => started(&mut client, rest.trim()),
            _ => match serde_json::from_str::<Value>(if rest.trim().is_empty() {
                "{}"
            } else {
                rest
            }) {
                Ok(arguments) => client.call(tool, arguments),
                Err(error) => Err(format!("bad arguments: {error}")),
            },
        };
        match outcome {
            Ok((false, text)) => out.push_str(&format!("mcp {tool}: {text}\n")),
            Ok((true, text)) => out.push_str(&format!("mcp {tool} error: {text}\n")),
            Err(error) => out.push_str(&format!("mcp {tool} failed: {error}\n")),
        }
    }
    out
}

/// Poll `inspect` until `branch` is neither running nor waiting (for its
/// prerequisites or on its children), for up to a minute.
fn wait(client: &mut McpClient, branch: &str) -> Result<(bool, String), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (error, text) = client.call("inspect", json!({"branch": branch}))?;
        if error {
            return Ok((true, text));
        }
        let status: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let state = status["status"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if !matches!(
            state.as_str(),
            "running" | "waiting" | "waiting_on_children"
        ) || Instant::now() >= deadline
        {
            return Ok((false, state));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run every `SH` line of `text` and describe the results.
fn shell(text: &str) -> String {
    let mut out = String::new();
    for command in text.lines().filter_map(|l| l.strip_prefix("SH ")) {
        match Command::new("sh")
            .args(["-c", command])
            .stdin(Stdio::null())
            .output()
        {
            Ok(done) => {
                out.push_str(&format!("sh: {}\n", done.status.code().unwrap_or(-1)));
                out.push_str(&String::from_utf8_lossy(&done.stdout));
                out.push_str(&String::from_utf8_lossy(&done.stderr));
            }
            Err(error) => out.push_str(&format!("sh: failed: {error}\n")),
        }
    }
    out
}

/// Poll `events` until `branch`'s prompt is recorded, for up to a minute.
fn started(client: &mut McpClient, branch: &str) -> Result<(bool, String), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let arguments = json!({"branch": branch, "cursor": 0, "limit": 200});
        let (error, text) = client.call("events", arguments)?;
        if error {
            return Ok((true, text));
        }
        let page: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let prompted = page["events"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|event| event["activity"].get("prompt").is_some());
        if prompted || Instant::now() >= deadline {
            return Ok((false, prompted.to_string()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
