#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
//! The workspace manager MCP, `by mcp manager`, driven over its stdio
//! against a real `by serve` whose workspace sessions must name an
//! execution preset. The fake ACP agent is the harness. Hermetic; requires
//! `git`.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Stdio};

use branchyard_client::workspace_api::StartSessionRequest;
use branchyard_testkit::{fake_agent, wait, Repo};
use serde_json::{json, Value};

const BY: &str = env!("CARGO_BIN_EXE_by");

/// A child process, killed and reaped on drop.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        branchyard_support::best_effort("kill the test child", self.0.kill());
        branchyard_support::best_effort("reap the test child", self.0.wait());
    }
}

/// `by serve -c config` on a loopback port it chose, and its URL.
fn serve(repo: &Repo, config: &Path) -> (Running, String) {
    let mut child = repo
        .command(BY)
        .args(["serve", "-c"])
        .arg(config)
        .args(["--listen", "127.0.0.1:0", "--quiet"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(fs::File::create(repo.dir.join("server.log")).unwrap())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line
        .trim()
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("server did not start: {line:?}"))
        .to_owned();
    (Running(child), url)
}

/// `by mcp manager` driven with JSON-RPC over its stdio.
struct Manager {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
    _child: Running,
}

impl Manager {
    fn start(repo: &Repo, url: &str, token_file: &Path) -> Manager {
        let mut child = repo
            .command(BY)
            .args(["mcp", "manager", "--url", url, "--token-file"])
            .arg(token_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(fs::File::create(repo.dir.join("manager.log")).unwrap())
            .spawn()
            .unwrap();
        let mut manager = Manager {
            stdin: child.stdin.take().unwrap(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            next: 0,
            _child: Running(child),
        };
        manager.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "test", "version": "0"}}),
        );
        manager.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        manager
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "the manager closed its output"
            );
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    /// Whether the tool call failed, and the JSON it answered.
    fn call(&mut self, tool: &str, arguments: Value) -> (bool, Value) {
        let response = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        let result = &response["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}: {response}"));
        (
            result["isError"] == true,
            serde_json::from_str(text).unwrap(),
        )
    }

    fn ok(&mut self, tool: &str, arguments: Value) -> Value {
        let (failed, answer) = self.call(tool, arguments);
        assert!(!failed, "{tool}: {answer}");
        answer
    }

    fn finished(&mut self, operation: &Value) -> Value {
        let id = operation["id"].as_str().unwrap().to_owned();
        wait::until(&format!("operation {id} to finish"), || {
            let state = self.ok("wait_operation", json!({"operation_id": id, "seconds": 5}));
            ["succeeded", "failed", "interrupted"]
                .contains(&state["state"].as_str().unwrap())
                .then_some(state)
        })
    }
}

#[test]
fn the_manager_starts_a_session_on_a_required_preset_as_the_http_route_does() {
    let repo = Repo::init(BY);
    let data = repo.dir.join("data");
    let config = repo.dir.join("config.json");
    let preset = json!({
        "id": "local-gemini",
        "label": "Local Gemini",
        "provider": {"kind": "local"},
        "harnesses": [{"harness": "gemini-cli", "models": ["fast", "deep"], "default_model": "deep"}]
    });
    fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "repos": {"app": repo.root},
            "data_dir": data,
            "harness_commands": {"gemini-cli": [fake_agent!()]},
            "execution_presets": [preset],
            "require_workspace_execution_presets": true
        }))
        .unwrap(),
    )
    .unwrap();
    let (_server, url) = serve(&repo, &config);
    let token = data.join("token");
    let mut manager = Manager::start(&repo, &url, &token);

    let project = manager.ok(
        "create_project",
        json!({"name": "Backlog", "source": {"kind": "repository", "repo": "app"},
               "result_mode": "mixed", "idempotency_key": "project"}),
    );
    let created = manager.ok(
        "create_workspace",
        json!({"project_id": project["id"], "title": "Preset work", "idempotency_key": "workspace"}),
    );
    assert_eq!(manager.finished(&created)["state"], "succeeded");
    let wid = created["workspace_id"].as_str().unwrap();

    let body = json!({"name": "Analyst", "prompt": "Summarise the backlog",
                      "harness": "gemini-cli", "access": "writer"});
    let start = |extra: Value, key: &str| {
        let mut args = body.clone();
        let fields = args.as_object_mut().unwrap();
        fields.extend(extra.as_object().unwrap().clone());
        fields.insert("workspace_id".into(), json!(wid));
        fields.insert("idempotency_key".into(), json!(key));
        args
    };

    let (failed, refused) = manager.call("start_session", start(json!({}), "raw"));
    assert!(failed, "{refused}");
    assert_eq!(refused["status"], 400, "{refused}");
    assert_eq!(refused["error"]["code"], "preset_required", "{refused}");

    let by_manager = manager.ok(
        "start_session",
        start(json!({"preset": "local-gemini"}), "manager-default"),
    );
    let chosen = manager.ok(
        "start_session",
        start(
            json!({"preset": "local-gemini", "model": "fast"}),
            "manager-chosen",
        ),
    );
    let mut http_body = body.clone();
    http_body["preset"] = json!("local-gemini");
    let request: StartSessionRequest = serde_json::from_value(http_body).unwrap();
    let by_http = branchyard_client::Client::from_token_file(&url, &token)
        .unwrap()
        .start_workspace_session(wid, &request, "http-default")
        .unwrap();

    let listed = manager.ok("list_sessions", json!({"workspace_id": wid}));
    let session = |id: &Value| -> Value {
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| &s["id"] == id)
            .unwrap_or_else(|| panic!("no session {id} in {listed}"))
            .clone()
    };
    let managed = session(&by_manager["session_id"]);
    let direct = session(&json!(by_http.session_id.unwrap()));
    for field in ["preset", "harness", "model", "access"] {
        assert_eq!(managed[field], direct[field], "{field}: {managed} {direct}");
    }
    assert_eq!(managed["preset"], "local-gemini");
    assert_eq!(managed["model"], "deep", "the preset's default_model");
    assert_eq!(session(&chosen["session_id"])["model"], "fast");
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 3);
    for operation in [by_manager, chosen, json!({"id": by_http.operation.id})] {
        let done = manager.finished(&operation);
        assert_eq!(done["state"], "succeeded", "{done}");
    }
}
