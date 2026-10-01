//! Branchyard and a real Anvil, end to end, on Anvil's `examples/github-mini`
//! fixture: `by gateway start` runs Anvil's gateway in branchyard mode
//! against a mock GitHub upstream, `by connect --api-key-stdin` connects the
//! person's account, and a branch granted `github:read` runs the fake ACP
//! agent, whose turn runs Python against the packaged SDK in gateway mode:
//! listing issues succeeds, creating one is refused `policy_denied`, and
//! `by log --json` shows both as `connector_call` events.
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

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use serde_json::Value;

const DEFAULT_ANVIL: &str = "/home/user/anvil/packages/cli/dist/bin-anvil.js";
const UPSTREAM_TOKEN: &str = "e2e-upstream-pat";

/// The harness's script: Anvil's generated Python SDK in gateway mode.
/// `GITHUB_TOKEN` is set, and wrong: gateway mode must not read it.
const HARNESS: &str = r#"import json, os, sys
os.environ["GITHUB_TOKEN"] = "must-not-be-read"
sys.path.insert(0, os.path.join(os.environ["HOME"], ".branchyard/connectors/github/python"))
from anvil_github import GithubClient, AnvilError
client = GithubClient()
issues = client.list_issue(owner="octo", repo="hello")
try:
    client.create_issue(owner="octo", repo="hello", title="nope", confirm=True)
    refused = None
except AnvilError as error:
    refused = {"code": error.code, "details": error.details}
print("RESULT " + json.dumps({"issues": len(issues), "refused": refused}))
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

fn fake_agent() -> PathBuf {
    let by = PathBuf::from(env!("CARGO_BIN_EXE_by"));
    let profile_dir = by.parent().unwrap().to_path_buf();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--quiet", "--offline", "--manifest-path"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
        .args(["-p", "branchyard-runtime", "--bin", "fake-acp-agent"])
        .env("CARGO_TARGET_DIR", profile_dir.parent().unwrap())
        .status()
        .unwrap();
    assert!(status.success(), "building fake-acp-agent failed");
    profile_dir.join("fake-acp-agent")
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
    let Some(anvil) = anvil_bin() else {
        eprintln!(
            "skipped: no built Anvil (set ANVIL_BIN to its packages/cli/dist/bin-anvil.js; \
             {DEFAULT_ANVIL} does not exist)"
        );
        return;
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
            return;
        }
    }
    let dir = std::env::temp_dir().join(format!("branchyard-anvil-e2e-{}", std::process::id()));
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
    let out = Command::new("node")
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

    // The turn: the fake agent runs the harness's Python script.
    fs::write(dir.join("harness.py"), HARNESS).unwrap();
    let agent = fake_agent();
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
        .arg(&agent)
        .arg("--yes")
        .output()
        .unwrap();
    assert!(out.status.success(), "by run: {}", text(&out));

    let out = by(&root)
        .args(["log", "reader", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "by log: {}", text(&out));
    let events: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    let said: String = events
        .iter()
        .filter_map(|e| e["event"]["text"].as_str())
        .collect();
    let result: Value = serde_json::from_str(
        said.split("RESULT ")
            .nth(1)
            .and_then(|r| r.lines().next())
            .unwrap_or_else(|| {
                panic!(
                    "no result in {said}\n{}",
                    fs::read_to_string(&log).unwrap_or_default()
                )
            }),
    )
    .unwrap();
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
