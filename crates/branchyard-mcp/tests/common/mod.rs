//! Temporary repositories, the fake ACP agent, and an MCP client over the
//! server's stdio. Hermetic: requires `git`; no real harness, no network.

#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Once, OnceLock};

use branchyard::{Activity, Envelope, Event, Policy, RecordedEvent, TaskOptions, Yard};
use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);
static HERMETIC: Once = Once::new();

pub const SERVER: &str = env!("CARGO_BIN_EXE_branchyard-mcp");

/// The `fake-acp-agent` binary from branchyard-runtime, built once per test
/// binary; cargo exposes a binary's path only to its own package's tests.
pub fn fake_agent() -> &'static Path {
    static AGENT: OnceLock<PathBuf> = OnceLock::new();
    AGENT.get_or_init(|| {
        // target/<profile>/branchyard-mcp
        let profile_dir = Path::new(SERVER).parent().unwrap().to_path_buf();
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut command = Command::new(cargo);
        command
            .args(["build", "--quiet", "--offline", "--manifest-path"])
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
            .args(["-p", "branchyard-runtime", "--bin", "fake-acp-agent"])
            .env("CARGO_TARGET_DIR", profile_dir.parent().unwrap());
        match profile_dir.file_name().and_then(|n| n.to_str()) {
            Some("debug") => {}
            Some("release") => {
                command.arg("--release");
            }
            Some(other) => {
                command.args(["--profile", other]);
            }
            None => panic!("unexpected binary location {SERVER}"),
        }
        assert!(
            command.status().unwrap().success(),
            "building fake-acp-agent failed"
        );
        let agent = profile_dir.join("fake-acp-agent");
        assert!(agent.is_file());
        agent
    })
}

/// A temporary repository with one commit on `main`. Removed on drop.
pub struct Fixture {
    pub dir: PathBuf,
    pub root: PathBuf,
    pub yard: Yard,
}

impl Fixture {
    pub fn new() -> Fixture {
        HERMETIC.call_once(|| {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        });
        fake_agent();
        let dir = std::env::temp_dir().join(format!(
            "branchyard-mcp-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let root = dir.join("repo");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@localhost"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        let yard = Yard::open(&root).unwrap();
        Fixture { dir, root, yard }
    }

    pub fn git(&self, args: &[&str]) -> String {
        git(&self.root, args)
    }

    /// The fake agent as gemini-cli, allowed everything, delegating within
    /// `envelope` through the real `branchyard-mcp`.
    pub fn delegating(&self, envelope: Envelope) -> TaskOptions {
        TaskOptions {
            harness: Some("gemini-cli".into()),
            command: Some(vec![fake_agent().display().to_string()]),
            policy: Policy::allow_all(),
            delegation: Some(envelope),
            delegation_server: Some(vec![SERVER.into()]),
            ..TaskOptions::default()
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The text the harness sent, joined.
pub fn text(events: &[RecordedEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Harness(Event::MessageDelta { text, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// `branchyard-mcp` driven over its stdio with JSON-RPC.
pub struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Client {
    pub fn start(root: &Path, branch: &str, token: &str) -> Client {
        let mut child = Command::new(SERVER)
            .args(["--root", &root.display().to_string(), "--branch", branch])
            .env("BRANCHYARD_DELEGATION", token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Client {
            child,
            stdin,
            stdout,
            next: 0,
        }
    }

    pub fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    /// The whole response to a request.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "the server closed its output"
            );
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == json!(id) {
                assert_eq!(message["jsonrpc"], "2.0");
                return message;
            }
        }
    }

    pub fn initialize(&mut self) -> Value {
        let response = self.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "test", "version": "0"}}),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        response
    }

    /// `(isError, text)` of a tool call.
    pub fn call(&mut self, tool: &str, arguments: Value) -> (bool, String) {
        let response = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        let result = &response["result"];
        assert!(result.is_object(), "{response}");
        (
            result["isError"] == true,
            result["content"][0]["text"].as_str().unwrap().to_owned(),
        )
    }

    /// Close stdin and wait for the server to exit on its own.
    pub fn finish(mut self) -> std::process::ExitStatus {
        self.stdin.take();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the server did not exit when its input closed"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
