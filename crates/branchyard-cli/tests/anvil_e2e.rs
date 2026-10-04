//! Branchyard and a real Anvil, end to end, on Anvil's `examples/github-mini`
//! fixture: `by gateway start` runs Anvil's gateway in branchyard mode
//! against a mock GitHub upstream, `by connect --api-key-stdin` connects the
//! person's account, and a branch granted `github:read` runs the fake ACP
//! agent, whose turn runs Python against the packaged SDK in gateway mode:
//! the turn's token is refused `403` at the gateway's `/connect/api-key`
//! (only `by connect`'s connect token is taken there), listing issues
//! succeeds, creating one is refused `policy_denied`, and `by log --json`
//! shows both as `connector_call` events.
//!
//! A second test runs the effect ledger against the same gateway: a branch
//! granted `github:write+confirm` comments on an issue over MCP and over
//! REST and creates a release, through the turn's ledger proxy; the
//! comments are ledgered `confirmed` with the inverse Anvil reports
//! (`undo.tool`), the release is staged as Anvil's draft, and `by undo`
//! deletes both comments and discards the draft upstream.
//!
//! Needs `node`, `python3` and a built Anvil: `ANVIL_BIN` (its
//! `bin-anvil.js`), or `/home/user/anvil/packages/cli/dist/bin-anvil.js`,
//! with `examples/github-mini` beside it. Ignored by default (CI has no
//! Anvil); run it with
//!
//! ```sh
//! cargo test -p branchyard-cli --test anvil_e2e -- --ignored --nocapture
//! ```
//!
//! and it says why when it skips.

#![allow(
    clippy::expect_used,
    clippy::let_underscore_must_use,
    clippy::map_unwrap_or,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::unwrap_used
)] // tests: a panic is the failure report
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use branchyard_testkit::fake_agent;
use serde_json::Value;

const DEFAULT_ANVIL: &str = "/home/user/anvil/packages/cli/dist/bin-anvil.js";
const UPSTREAM_TOKEN: &str = "e2e-upstream-pat";

/// The harness's script: Anvil's generated Python SDK in gateway mode.
/// `GITHUB_TOKEN` is set, and wrong: gateway mode must not read it. First it
/// tries, with the turn's own token, what a prompt-injected harness would:
/// replacing the person's GitHub credential at `/connect/api-key`. The
/// gateway must refuse it, and the list below still uses the person's key.
const HARNESS: &str = r#"import json, os, sys, urllib.request, urllib.error
os.environ["GITHUB_TOKEN"] = "must-not-be-read"
token = open(os.environ["ANVIL_GATEWAY_TOKEN_FILE"]).read().strip()
base = os.environ["ANVIL_GATEWAY_URL"].rsplit("/mcp", 1)[0]
overwrite = urllib.request.Request(
    base + "/connect/api-key",
    data=json.dumps({"connector": "github", "api_key": "from-the-harness"}).encode(),
    headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"},
    method="POST",
)
try:
    with urllib.request.urlopen(overwrite) as response:
        connect = {"status": response.status, "code": None}
except urllib.error.HTTPError as error:
    connect = {"status": error.code, "code": json.load(error).get("error", {}).get("code")}
sys.path.insert(0, os.path.join(os.environ["HOME"], ".branchyard/connectors/github/python"))
from anvil_github import GithubClient, AnvilError
client = GithubClient()
issues = client.list_issue(owner="octo", repo="hello")
try:
    client.create_issue(owner="octo", repo="hello", title="nope", confirm=True)
    refused = None
except AnvilError as error:
    refused = {"code": error.code, "details": error.details}
print("RESULT " + json.dumps({"issues": len(issues), "refused": refused, "connect": connect}))
"#;

fn anvil_bin() -> Option<PathBuf> {
    let path = std::env::var_os("ANVIL_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ANVIL));
    path.is_file().then_some(path)
}

fn works(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `packages/cli/dist/bin-anvil.js` → the repository root.
fn anvil_root(bin: &Path) -> PathBuf {
    bin.ancestors()
        .nth(4)
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

struct Cleanup {
    dir: PathBuf,
    root: PathBuf,
    mock: Option<Child>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = by(&self.root).args(["gateway", "stop"]).output();
        if let Some(mut mock) = self.mock.take() {
            let _ = mock.kill();
            let _ = mock.wait();
        }
        if std::env::var_os("BY_KEEP_E2E").is_none() {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// Where a test's Anvil runs: the mock upstream, the compiled bundle, a
/// repository pointed at them, the gateway started and the person's account
/// connected.
struct Anvil {
    cleanup: Cleanup,
    dir: PathBuf,
    root: PathBuf,
    anvil: PathBuf,
    upstream: String,
    log: PathBuf,
}

/// Set up Anvil for test `name`, or say why not and return `None`.
fn start(name: &str) -> Option<Anvil> {
    let Some(anvil) = anvil_bin() else {
        eprintln!(
            "skipped: no built Anvil (set ANVIL_BIN to its packages/cli/dist/bin-anvil.js; \
             {DEFAULT_ANVIL} does not exist)"
        );
        return None;
    };
    let fixture = anvil_root(&anvil).join("examples/github-mini");
    for (what, ok) in [
        ("node", works("node")),
        ("python3", works("python3")),
        (
            "Anvil's examples/github-mini",
            fixture.join("mock-upstream.mjs").is_file(),
        ),
    ] {
        if !ok {
            eprintln!("skipped: {what} is not available");
            return None;
        }
    }
    let dir = std::env::temp_dir().join(format!("branchyard-anvil-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("repo")).unwrap();
    let dir = fs::canonicalize(dir).unwrap();
    let root = dir.join("repo");
    let mut cleanup = Cleanup {
        dir: dir.clone(),
        root: root.clone(),
        mock: None,
    };

    // The mock upstream, expecting the person's token.
    let mut mock = Command::new("node")
        .arg(fixture.join("mock-upstream.mjs"))
        .args(["--port", "0", "--token", UPSTREAM_TOKEN])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(mock.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    cleanup.mock = Some(mock);
    let upstream: Value = serde_json::from_str(line.trim()).expect("the mock's address");
    let upstream = upstream["url"].as_str().unwrap().to_owned();

    // Compile the fixture into a bundle root, pointed at the mock.
    let spec = fs::read_to_string(fixture.join("openapi.yaml"))
        .unwrap()
        .replace("https://api.github.example", &upstream);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/openapi.yaml"), spec).unwrap();
    let bundles = dir.join("workspace");
    // Anvil keeps a source cache in its working directory: the scratch
    // directory, not this crate.
    let out = Command::new("node")
        .current_dir(&dir)
        .arg(&anvil)
        .arg("compile")
        .arg(dir.join("src/openapi.yaml"))
        .arg("--manifest")
        .arg(fixture.join("anvil.yaml"))
        .arg("--out")
        .arg(bundles.join("github"))
        .output()
        .unwrap();
    assert!(out.status.success(), "anvil compile: {}", text(&out));

    // A repository whose branchyard.toml points at Anvil and the bundles.
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "user.email", "test@localhost"]);
    fs::write(root.join("a.txt"), "one\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let gateway = format!("http://127.0.0.1:{port}/mcp");
    fs::write(
        root.join("branchyard.toml"),
        format!(
            "[connectors]\ngateway = \"{gateway}\"\nbundles = \"{}\"\nanvil = \"node {}\"\n",
            bundles.display(),
            anvil.display()
        ),
    )
    .unwrap();

    let out = by(&root)
        .args(["gateway", "start", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "by gateway start: {}", text(&out));
    let started: Value = serde_json::from_slice(&out.stdout).unwrap();
    let log = root.join(".branchyard/gateway/gateway.log");
    assert_eq!(
        started["listening"],
        true,
        "{started}\n{}",
        fs::read_to_string(&log).unwrap_or_default()
    );

    // Connect the person's account: a personal token, so a key connection.
    let mut connect = by(&root)
        .args(["connect", "github", "--api-key-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    connect
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{UPSTREAM_TOKEN}\n").as_bytes())
        .unwrap();
    let out = connect.wait_with_output().unwrap();
    assert!(out.status.success(), "by connect: {}", text(&out));
    Some(Anvil {
        cleanup,
        dir,
        root,
        anvil,
        upstream,
        log,
    })
}

/// What a branch's turns said, from `by log --json`.
fn said(root: &Path, branch: &str) -> (Vec<Value>, String) {
    let out = by(root).args(["log", branch, "--json"]).output().unwrap();
    assert!(out.status.success(), "by log: {}", text(&out));
    let events: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    let said: String = events
        .iter()
        .filter_map(|e| e["event"]["text"].as_str())
        .collect();
    (events, said)
}

/// The `RESULT {...}` line a harness printed.
fn result(said: &str, log: &Path) -> Value {
    serde_json::from_str(
        said.split("RESULT ")
            .nth(1)
            .and_then(|r| r.lines().next())
            .unwrap_or_else(|| {
                panic!(
                    "no result in {said}\n{}",
                    fs::read_to_string(log).unwrap_or_default()
                )
            }),
    )
    .unwrap()
}

fn by(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_by"));
    command
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("NO_COLOR", "1")
        .env(
            "BRANCHYARD_USER_CONFIG",
            "/nonexistent/branchyard-config.toml",
        )
        // The mock upstream is on loopback; Anvil refuses other hosts
        // unless allowed.
        .env("ANVIL_ALLOWED_HOSTS", "127.0.0.1");
    for var in [
        "BRANCHYARD_DELEGATION",
        "BRANCHYARD_BRANCH",
        "BRANCHYARD_ROOT",
        "BRANCHYARD_BY",
        "GITHUB_TOKEN",
    ] {
        command.env_remove(var);
    }
    command
}

#[test]
#[ignore = "needs node, python3 and a built Anvil (ANVIL_BIN); run with --ignored"]
fn a_branch_granted_github_read_lists_issues_and_is_refused_a_write_through_anvils_gateway() {
    let Some(Anvil {
        cleanup,
        dir,
        root,
        anvil,
        log,
        ..
    }) = start("e2e")
    else {
        return;
    };

    // The turn: the fake agent runs the harness's Python script.
    fs::write(dir.join("harness.py"), HARNESS).unwrap();
    let agent = fake_agent!();
    let prompt = format!(
        "SH python3 {}\nSH head -3 \"$HOME/.branchyard/connectors/INDEX.md\"",
        dir.join("harness.py").display()
    );
    let out = by(&root)
        .args([
            "run",
            &prompt,
            "--name",
            "reader",
            "--isolated",
            "--connector",
            "github:read",
            "--harness",
            "gemini-cli",
            "--command",
        ])
        .arg(agent)
        .arg("--yes")
        .output()
        .unwrap();
    assert!(out.status.success(), "by run: {}", text(&out));

    let (events, said) = said(&root, "reader");
    let result = result(&said, &log);
    // The turn's token cannot touch the person's connections ...
    assert_eq!(result["connect"]["status"], 403, "{said}");
    assert_eq!(
        result["connect"]["code"], "connect_token_required",
        "{said}"
    );
    // ... so the list below went upstream with the person's own key (the
    // mock refuses any other).
    assert_eq!(result["issues"], 2, "{said}");
    assert_eq!(result["refused"]["code"], "policy_denied", "{said}");
    assert_eq!(
        result["refused"]["details"]["code"], "policy/grant_denied",
        "{said}"
    );
    assert!(said.contains("github"), "the index: {said}");
    let calls: Vec<&Value> = events
        .iter()
        .filter(|e| e["activity"] == "connector_call")
        .map(|e| &e["connector_call"])
        .collect();
    assert_eq!(calls.len(), 2, "{events:#?}");
    assert_eq!(calls[0]["operation"], "github.issues.list");
    assert_eq!(calls[0]["decision"], "allowed");
    assert_eq!(calls[0]["upstream_status"], 200);
    assert_eq!(calls[1]["operation"], "github.issues.create");
    assert_eq!(calls[1]["decision"], "denied");
    assert_eq!(calls[1]["reason"], "policy_denied");
    assert!(calls
        .iter()
        .all(|c| c["subject"].as_str().unwrap().starts_with("local:")));
    eprintln!(
        "end to end against Anvil at {}: {} connector_call events",
        anvil.display(),
        calls.len()
    );
    drop(cleanup);
}

/// The effects harness: raw MCP and REST, as a harness without the SDK
/// would, through the turn's ledger proxy (`ANVIL_GATEWAY_URL`). It finds
/// each tool by its AIR operation id in `tools/list`.
const EFFECTS_HARNESS: &str = r#"import json, os, urllib.request, urllib.error
url = os.environ["ANVIL_GATEWAY_URL"]
token = open(os.environ["ANVIL_GATEWAY_TOKEN_FILE"]).read().strip()
headers = {"Authorization": "Bearer " + token, "Content-Type": "application/json",
           "Accept": "application/json, text/event-stream"}
def rpc(body, session=None):
    h = dict(headers)
    if session:
        h["Mcp-Session-Id"] = session
    request = urllib.request.Request(url, method="POST", data=json.dumps(body).encode(), headers=h)
    response = urllib.request.urlopen(request, timeout=120)
    text = response.read().decode()
    session = response.headers.get("Mcp-Session-Id") or session
    if response.headers.get("Content-Type", "").startswith("text/event-stream"):
        for block in text.split("\n\n"):
            data = "\n".join(l[5:].lstrip() for l in block.splitlines() if l.startswith("data:"))
            if data:
                message = json.loads(data)
                if message.get("id") == body.get("id"):
                    return message, session
        return None, session
    return (json.loads(text) if text else None), session
_, session = rpc({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}}})
rpc({"jsonrpc": "2.0", "method": "notifications/initialized"}, session)
listed, _ = rpc({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}, session)
tools = {t.get("_meta", {}).get("anvil/operation_id"): t["name"] for t in listed["result"]["tools"]}
def call(n, operation, arguments):
    answer, _ = rpc({"jsonrpc": "2.0", "id": n, "method": "tools/call",
                     "params": {"name": tools[operation], "arguments": arguments}}, session)
    result = answer["result"]
    return {"error": bool(result.get("isError")),
            "text": "".join(c.get("text", "") for c in result.get("content", []))[:300]}
comment = call(3, "github.comments.create",
               {"owner": "octo", "repo": "hello", "issue_number": 1, "body": "Looks good."})
rest = urllib.request.Request(
    url.rsplit("/mcp", 1)[0] + "/call/" + tools["github.comments.create"], method="POST",
    data=json.dumps({"arguments": {"owner": "octo", "repo": "hello", "issue_number": 1,
                                   "body": "Over REST."}}).encode(), headers=headers)
try:
    response = urllib.request.urlopen(rest, timeout=120)
    rest = {"status": response.status, "effect": response.headers.get("X-Anvil-Effect")}
except urllib.error.HTTPError as e:
    rest = {"status": e.code, "body": e.read().decode()[:300]}
release = call(4, "github.releases.create",
               {"owner": "octo", "repo": "hello", "tag_name": "v1.0.0", "name": "One", "confirm": True})
print("RESULT " + json.dumps({"tools": tools, "comment": comment, "rest": rest, "release": release}))
"#;

/// The mock upstream's record of the requests it served.
fn upstream_requests(upstream: &str) -> Vec<Value> {
    let out = Command::new("node")
        .arg("-e")
        .arg(
            "fetch(process.argv[1] + '/__requests').then(r => r.text()).then(t => process.stdout.write(t))",
        )
        .arg(upstream)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
#[ignore = "needs node, python3 and a built Anvil (ANVIL_BIN); run with --ignored"]
fn the_effect_ledger_records_anvils_reports_and_undoes_through_its_gateway() {
    let Some(Anvil {
        cleanup,
        dir,
        root,
        upstream,
        log,
        ..
    }) = start("effects")
    else {
        return;
    };
    fs::write(dir.join("effects.py"), EFFECTS_HARNESS).unwrap();
    let agent = fake_agent!();
    let out = by(&root)
        .args([
            "run",
            &format!("SH python3 {}", dir.join("effects.py").display()),
            "--name",
            "writer",
            "--isolated",
            "--connector",
            "github:write+confirm",
            "--harness",
            "gemini-cli",
            "--command",
        ])
        .arg(agent)
        .arg("--yes")
        .output()
        .unwrap();
    assert!(out.status.success(), "by run: {}", text(&out));
    let (_, said) = said(&root, "writer");
    let result = result(&said, &log);
    assert_eq!(result["comment"]["error"], false, "{said}");
    assert_eq!(result["rest"]["status"], 200, "{said}");
    // The proxy passes Anvil's report on to the harness.
    assert!(
        result["rest"]["effect"]
            .as_str()
            .is_some_and(|e| e.contains("github.comments.delete")),
        "{said}"
    );
    // Staged: Anvil made the draft, not the release.
    assert_eq!(result["release"]["error"], false, "{said}");

    let out = by(&root)
        .args(["effects", "--branch", "writer", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "by effects: {}", text(&out));
    let ledger: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ledger.len(), 3, "{ledger:#?}");
    let delete_tool = result["tools"]["github.comments.delete"].as_str().unwrap();
    for comment in &ledger[..2] {
        assert_eq!(comment["state"], "confirmed", "{comment:#}");
        assert_eq!(comment["class"], "reversible", "{comment:#}");
        assert_eq!(comment["declared"], true);
        assert_eq!(comment["operation_id"], "github.comments.create");
        let undo = &comment["undo"];
        assert_eq!(undo["kind"], "inverse", "{comment:#}");
        assert_eq!(undo["operation"], "github.comments.delete");
        assert_eq!(undo["tool"], delete_tool);
        assert!(undo["arguments"]["comment_id"].is_u64(), "{comment:#}");
        // The comment carries its key upstream: the entry's id.
        assert_eq!(comment["upstream_key"], comment["id"], "{comment:#}");
    }
    let release = &ledger[2];
    assert_eq!(release["state"], "staged", "{release:#}");
    let draft = &release["staged"]["draft"];
    assert_eq!(
        draft["promote"]["tool"], result["tools"]["github.releases.update"],
        "{release:#}"
    );
    assert_eq!(
        draft["discard"]["tool"], result["tools"]["github.releases.delete"],
        "{release:#}"
    );
    let requests = upstream_requests(&upstream);
    let posted: Vec<&Value> = requests
        .iter()
        .filter(|r| r["method"] == "POST" && r["path"].as_str().unwrap().ends_with("/comments"))
        .collect();
    assert_eq!(posted.len(), 2, "{requests:#?}");
    assert_eq!(posted[0]["idempotency_key"], ledger[0]["id"]);
    assert_eq!(posted[1]["idempotency_key"], ledger[1]["id"]);

    // Undo: both comments deleted with the tool Anvil named, the draft
    // discarded with its discard call.
    let out = by(&root)
        .args(["undo", "writer", "--yes", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "by undo: {}", text(&out));
    let done: Value = serde_json::from_slice(&out.stdout).unwrap();
    let states: Vec<&str> = done["outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["state"].as_str().unwrap())
        .collect();
    assert_eq!(
        states.iter().filter(|s| **s == "undone").count(),
        2,
        "{done:#}"
    );
    assert_eq!(
        states.iter().filter(|s| **s == "failed").count(),
        1,
        "the draft, discarded: {done:#}"
    );
    let requests = upstream_requests(&upstream);
    let deleted = |kind: &str| {
        requests
            .iter()
            .filter(|r| r["method"] == "DELETE" && r["path"].as_str().unwrap().contains(kind))
            .count()
    };
    assert_eq!(deleted("/issues/comments/"), 2, "{requests:#?}");
    assert_eq!(deleted("/releases/"), 1, "{requests:#?}");
    eprintln!(
        "effects end to end against Anvil: {} upstream requests",
        requests.len()
    );
    drop(cleanup);
}
