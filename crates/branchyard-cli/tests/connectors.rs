//! `by gateway`, `by connect`, `--connector` and `by log` end to end, with a
//! stand-in for Anvil: a Python script that answers `anvil package
//! harness`, `anvil connectors index`, `anvil connect` and `anvil serve mcp
//! --fleet --http`, whose "gateway" decides calls from the token's grant
//! and appends Anvil's audit lines. It does not verify signatures (the
//! engine's tests do that against the published keys). Requires `git`,
//! `sh` and `python3`; no network beyond loopback, no real Anvil.

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

const FAKE_ANVIL: &str = r##"#!/usr/bin/env python3
import base64, http.server, json, os, sys

args = sys.argv[1:]

def claims_of(token):
    part = token.strip().split(".")[1]
    part += "=" * (-len(part) % 4)
    return json.loads(base64.urlsafe_b64decode(part))

def flag(name):
    return args[args.index(name) + 1]

if args[:2] == ["package", "harness"]:
    out = flag("--out")
    os.makedirs(os.path.join(out, "bin"), exist_ok=True)
    with open(os.path.join(out, "SKILL.md"), "w") as f:
        f.write("# " + os.path.basename(args[2]) + " (gateway mode)\n")
elif args[:2] == ["connectors", "index"]:
    with open(flag("--grants")) as f:
        grants = json.load(f)
    with open(flag("--out"), "w") as f:
        f.write("# Connectors\n")
        for g in grants:
            f.write("- %s (%s): %s/SKILL.md\n" % (g["connector"], g["mode"], g["connector"]))
elif args[:1] == ["connect"]:
    with open(os.environ["ANVIL_GATEWAY_TOKEN_FILE"]) as f:
        claims = claims_of(f.read())
    account = flag("--account") if "--account" in args else "default"
    print("connect %s for %s account %s via %s grants %s" % (
        args[1], claims["sub"], account, os.environ["ANVIL_GATEWAY_URL"], claims["by_grants"]))
elif args[:2] == ["serve", "mcp"]:
    for var in ["ANVIL_INBOUND_AUTH_MODE", "ANVIL_INBOUND_ISSUER", "ANVIL_INBOUND_AUDIENCE",
                "ANVIL_INBOUND_JWKS_URI", "ANVIL_AUDIT_FILE", "ANVIL_VAULT_KEY_FILE"]:
        if not os.environ.get(var):
            sys.exit("missing " + var)
    assert os.environ["ANVIL_INBOUND_AUTH_MODE"] == "branchyard"
    jwks = os.environ["ANVIL_INBOUND_JWKS_URI"][len("file://"):]
    assert json.load(open(jwks))["keys"], "no keys"

    class Gateway(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            claims = claims_of(self.headers["Authorization"].split(" ", 1)[1])
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            op = body["operation"]
            writes = not op.endswith(".list")
            entry = next((g for g in claims["by_grants"] if g["connector"] == "github"
                          and (not writes or g["mode"] == "write")), None)
            line = {
                "time": "2026-09-30T12:00:00Z", "sub": claims["sub"],
                "by_tenant": claims["by_tenant"], "by_branch": claims["by_branch"],
                "by_turn": claims["by_turn"], "connector": "github", "account": None,
                "operation": op, "decision": "allowed" if entry else "denied",
                "grant": entry, "upstream_status": 200 if entry else None, "latency_ms": 1,
                "input_sha256": "sha256:00", "error_code": None if entry else "policy_denied",
                "rule": None if entry else "policy/grant_denied", "dry_run": False,
                "trace_id": "t",
            }
            with open(os.environ["ANVIL_AUDIT_FILE"], "a") as f:
                f.write(json.dumps(line) + "\n")
            self.send_response(200 if entry else 403)
            self.end_headers()
            self.wfile.write(json.dumps({"ok": bool(entry)}).encode())

        def log_message(self, *a):
            pass

    http.server.HTTPServer(("127.0.0.1", int(flag("--http"))), Gateway).serve_forever()
else:
    sys.exit("fake anvil: " + " ".join(args))
"##;

/// What a harness's script does with Anvil's SDK in gateway mode: one call
/// through the gateway with the turn's token.
const CALL: &str = r#"import json, os, sys, urllib.request, urllib.error
token = open(os.environ["ANVIL_GATEWAY_TOKEN_FILE"]).read().strip()
request = urllib.request.Request(os.environ["ANVIL_GATEWAY_URL"], method="POST",
    data=json.dumps({"operation": sys.argv[1]}).encode(),
    headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
try:
    print("call %s: %d" % (sys.argv[1], urllib.request.urlopen(request).status))
except urllib.error.HTTPError as e:
    print("call %s: %d" % (sys.argv[1], e.code))
"#;

fn fake_agent() -> &'static Path {
    static AGENT: OnceLock<PathBuf> = OnceLock::new();
    AGENT.get_or_init(|| {
        let by = PathBuf::from(env!("CARGO_BIN_EXE_by"));
        let profile_dir = by.parent().unwrap().to_path_buf();
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
            None => panic!("unexpected binary location {}", by.display()),
        }
        assert!(
            command.status().unwrap().success(),
            "building fake-acp-agent failed"
        );
        profile_dir.join("fake-acp-agent")
    })
}

struct Repo {
    dir: PathBuf,
    root: PathBuf,
    url: String,
}

impl Repo {
    fn new() -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-connectors-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let root = dir.join("repo");
        // A free port for the gateway.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let repo = Repo {
            root,
            url: format!("http://127.0.0.1:{port}/mcp"),
            dir,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@localhost"]);
        fs::write(repo.root.join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "-m", "initial"]);
        let anvil = repo.dir.join("fake-anvil");
        fs::write(&anvil, FAKE_ANVIL).unwrap();
        fs::set_permissions(&anvil, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(repo.dir.join("call.py"), CALL).unwrap();
        let bundles = repo.dir.join("bundles");
        for id in ["github", "linear"] {
            fs::create_dir_all(bundles.join(id)).unwrap();
            fs::write(
                bundles.join(id).join("air.yaml"),
                format!("service: {id}\n"),
            )
            .unwrap();
        }
        // Outside the repository, so the bundles are not part of it.
        fs::write(
            repo.root.join("branchyard.toml"),
            format!(
                "[connectors]\ngateway = \"{}\"\nbundles = \"{}\"\nanvil = \"{}\"\n",
                repo.url,
                bundles.display(),
                anvil.display()
            ),
        )
        .unwrap();
        repo
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env(
                "BRANCHYARD_USER_CONFIG",
                "/nonexistent/branchyard-config.toml",
            );
        for var in [
            "BRANCHYARD_DELEGATION",
            "BRANCHYARD_BRANCH",
            "BRANCHYARD_ROOT",
            "BRANCHYARD_BY",
        ] {
            command.env_remove(var);
        }
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.command("git").args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn by(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_by"))
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.by(args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = self.by(&["gateway", "stop"]);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn python() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn the_gateway_runs_supervised_a_granted_turn_calls_it_and_by_log_shows_the_calls() {
    if !python() {
        eprintln!("skipped: python3 is not installed");
        return;
    }
    let repo = Repo::new();
    let status = repo.json(&["gateway", "status", "--json"]);
    assert_eq!(status["listening"], false);
    assert_eq!(
        status["connectors"],
        serde_json::json!(["github", "linear"])
    );

    let started = repo.json(&["gateway", "start", "--json"]);
    assert_eq!(started["started"], true, "{started}");
    assert_eq!(started["listening"], true, "{started}");
    let again = repo.json(&["gateway", "start", "--json"]);
    assert_eq!(again["started"], false, "one gateway per repository");
    let status = repo.json(&["gateway", "status", "--json"]);
    assert_eq!(status["listening"], true);
    assert!(status["running"]["pid"].is_u64());
    let issuer = status["issuer"].as_str().unwrap().to_owned();
    assert!(issuer.starts_with("branchyard:local:"), "{status}");
    // The key is private and the public set is what the gateway reads.
    let gateway_dir = repo.root.join(".branchyard/gateway");
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&gateway_dir.join("key")), 0o600);
    assert_eq!(mode(&gateway_dir.join("vault.key")), 0o600);
    let jwks: Value =
        serde_json::from_str(&fs::read_to_string(gateway_dir.join("jwks.json")).unwrap()).unwrap();
    assert_eq!(jwks["keys"][0]["alg"], "EdDSA");
    assert!(jwks["keys"][0].get("d").is_none());

    // A turn granted github:read reads and is refused a write.
    let agent = fake_agent().display().to_string();
    let call = repo.dir.join("call.py").display().to_string();
    let prompt = format!(
        "SH python3 {call} issues.list\nSH python3 {call} issues.create\n\
         SH cat \"$HOME/.branchyard/connectors/INDEX.md\""
    );
    let out = repo.by(&[
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
        &agent,
        "--yes",
    ]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let log = repo.json(&["log", "reader", "--json"]);
    let events = log.as_array().unwrap();
    let said: String = events
        .iter()
        .filter_map(|e| e["event"]["text"].as_str())
        .collect();
    assert!(said.contains("call issues.list: 200"), "{said}");
    assert!(said.contains("call issues.create: 403"), "{said}");
    assert!(said.contains("- github (read): github/SKILL.md"), "{said}");
    let calls: Vec<&Value> = events
        .iter()
        .filter(|e| e["activity"] == "connector_call")
        .map(|e| &e["connector_call"])
        .collect();
    assert_eq!(calls.len(), 2, "{log}");
    assert_eq!(calls[0]["operation"], "issues.list");
    assert_eq!(calls[0]["decision"], "allowed");
    assert_eq!(calls[1]["decision"], "denied");
    assert_eq!(calls[1]["reason"], "policy_denied");
    assert_eq!(calls[1]["rule"], "policy/grant_denied");
    let provisioned = events
        .iter()
        .find(|e| e["activity"] == "provisioned")
        .unwrap();
    assert_eq!(provisioned["connectors"], serde_json::json!(["github"]));
    let text = stdout(&repo.by(&["log", "reader"]));
    assert!(
        text.contains("connector: github issues.create denied (policy_denied, 1 ms)"),
        "{text}"
    );
    assert!(text.contains("provisioned: connectors github"), "{text}");

    // A connector the gateway does not serve fails the turn by name.
    let out = repo.by(&[
        "run",
        "say hi",
        "--name",
        "unserved",
        "--isolated",
        "--connector",
        "slack",
        "--harness",
        "gemini-cli",
        "--command",
        &agent,
        "--yes",
    ]);
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(text.contains("connector slack is not served"), "{text}");

    // `by connect` runs anvil connect as the person, with a grant of nothing.
    let out = repo.by(&["connect", "github", "--account", "work"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let said = stdout(&out);
    assert!(said.contains("connect github for local:"), "{said}");
    assert!(said.contains("account work"), "{said}");
    assert!(said.contains("grants []"), "{said}");
    assert!(!gateway_dir.read_dir().unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("connect-")));

    // Rotation publishes both keys.
    let rotated = repo.json(&["gateway", "rotate-key", "--json"]);
    assert_eq!(rotated["kids"].as_array().unwrap().len(), 2);
    let published = repo.json(&["gateway", "jwks"]);
    assert_eq!(published["keys"].as_array().unwrap().len(), 2);

    let stopped = repo.json(&["gateway", "stop", "--json"]);
    assert_eq!(stopped["stopped"], true);
    let deadline = Instant::now() + Duration::from_secs(10);
    while repo.json(&["gateway", "status", "--json"])["listening"] == true {
        assert!(Instant::now() < deadline, "the gateway still listens");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn connectors_without_a_private_home_are_refused_before_a_branch_exists() {
    let repo = Repo::new();
    let out = repo.by(&[
        "run",
        "say hi",
        "--name",
        "shared",
        "--connector",
        "github",
        "--harness",
        "gemini-cli",
        "--command",
        &fake_agent().display().to_string(),
        "--yes",
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("private"), "{}", stderr(&out));
    let list = repo.json(&["ls", "--json"]);
    assert_eq!(list.as_array().unwrap().len(), 0);
}
