//! The service registry and live catalogs through the built `by`:
//!
//! - a connector gateway with no URL configured: `by gateway start` picks a
//!   port and registers it, and `by connect` and `by gateway status` find
//!   it in the registry; a pinned `[connectors] gateway` still wins;
//! - `by services` lists it with its owner and lease; a supervisor killed
//!   with SIGKILL leaves its gateway process running, and `by services gc`
//!   reclaims it;
//! - `by catalog refresh` against a local mock of the MCP registry and the
//!   npm registry: pages and releases cached with ETags (a second refresh
//!   is all `304`s), entries pinned and matched to the baseline, and a
//!   cache whose checksum does not verify is refused.
//!
//! Requires `git`, `sh` and `python3`; nothing leaves loopback.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use branchyard_testkit::wait;
use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A stand-in for Anvil: `serve mcp` listens on `--http` and answers
/// anything; `connect` prints the gateway it was given.
const FAKE_ANVIL: &str = r##"#!/usr/bin/env python3
import http.server, os, sys
args = sys.argv[1:]
if args[:1] == ["connect"]:
    print("connect %s via %s" % (args[2], os.environ["ANVIL_GATEWAY_URL"]))
elif args[:2] == ["serve", "mcp"]:
    port = int(args[args.index("--http") + 1])
    class Gateway(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            self.send_response(200)
            self.end_headers()
        def log_message(self, *a):
            pass
    http.server.HTTPServer(("127.0.0.1", port), Gateway).serve_forever()
else:
    sys.exit("fake anvil: " + " ".join(args))
"##;

/// A mock of the MCP registry (`/v0/servers`, two pages) and of npm
/// (`/<package>/latest`), with ETags: a request whose `If-None-Match`
/// matches is answered `304`. Each request is appended to the log named
/// by its first argument as `path if-none-match`.
const MOCK_REGISTRY: &str = r##"#!/usr/bin/env python3
import hashlib, http.server, json, sys, urllib.parse
log = sys.argv[1]
PAGES = {
    None: {"servers": [
        {"server": {"name": "com.amplitude/mcp", "title": "Amplitude", "description": "a",
                    "version": "2.0.0",
                    "remotes": [{"type": "streamable-http", "url": "https://mcp.amplitude.com/mcp"}]},
         "_meta": {}},
        {"server": {"name": "io.example/notes", "description": "notes", "version": "1.2.0",
                    "remotes": [{"type": "streamable-http", "url": "https://notes.example/mcp",
                                 "headers": [{"name": "Authorization", "isRequired": True}]}]}}],
           "metadata": {"nextCursor": "io.example/notes:1.2.0", "count": 2}},
    "io.example/notes:1.2.0": {"servers": [
        {"server": {"name": "io.example/fs", "description": "files", "version": "0.1.5",
                    "packages": [{"registryType": "npm", "identifier": "@example/fs",
                                  "version": "0.1.5",
                                  "environmentVariables": [{"name": "FS_ROOT", "isRequired": True}]}]}}],
           "metadata": {"count": 1}},
}
class Registry(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        query = urllib.parse.parse_qs(url.query)
        with open(log, "a") as f:
            f.write("%s %s\n" % (self.path, self.headers.get("If-None-Match", "-")))
        if url.path == "/v0/servers":
            body = PAGES[query.get("cursor", [None])[0]]
        elif url.path.endswith("/latest"):
            body = {"name": urllib.parse.unquote(url.path[1:-len("/latest")]), "version": "9.9.9",
                    "dist": {"integrity": "sha512-pinned"}}
        else:
            self.send_response(404)
            self.end_headers()
            return
        data = json.dumps(body).encode()
        etag = '"%s"' % hashlib.sha256(data).hexdigest()[:16]
        if self.headers.get("If-None-Match") == etag:
            self.send_response(304)
            self.send_header("ETag", etag)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("ETag", etag)
        self.end_headers()
        self.wfile.write(data)
    def log_message(self, *a):
        pass
server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Registry)
print(server.server_address[1], flush=True)
server.serve_forever()
"##;

struct Repo {
    dir: PathBuf,
    root: PathBuf,
    anvil: PathBuf,
    bundles: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-services-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let root = dir.join("repo");
        let anvil = dir.join("fake-anvil");
        fs::write(&anvil, FAKE_ANVIL).unwrap();
        fs::set_permissions(&anvil, fs::Permissions::from_mode(0o755)).unwrap();
        let bundles = dir.join("bundles");
        fs::create_dir_all(bundles.join("github")).unwrap();
        fs::write(bundles.join("github/air.yaml"), "service: github\n").unwrap();
        let repo = Repo {
            dir,
            root,
            anvil,
            bundles,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@localhost"]);
        fs::write(repo.root.join("a.txt"), "one\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "-m", "initial"]);
        repo.configure(None);
        repo
    }

    /// `[connectors]` with the bundles and Anvil, and a gateway URL only
    /// when `pin` gives one.
    fn configure(&self, pin: Option<&str>) {
        let mut text = format!(
            "[connectors]\nbundles = \"{}\"\nanvil = \"{}\"\n",
            self.bundles.display(),
            self.anvil.display()
        );
        if let Some(url) = pin {
            text.push_str(&format!("gateway = \"{url}\"\n"));
        }
        fs::write(self.root.join("branchyard.toml"), text).unwrap();
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env("BRANCHYARD_CATALOG_DIR", self.dir.join("catalog"))
            .env(
                "BRANCHYARD_USER_CONFIG",
                "/nonexistent/branchyard-config.toml",
            );
        for var in [
            "BRANCHYARD_DELEGATION",
            "BRANCHYARD_BRANCH",
            "BRANCHYARD_ROOT",
            "BRANCHYARD_BY",
            "BRANCHYARD_REGISTRY",
            "BRANCHYARD_REMOTE",
            "BRANCHYARD_MCP_REGISTRY",
            "BRANCHYARD_NPM_REGISTRY",
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
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            stdout(&out),
            stderr(&out)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = self.by(&["gateway", "stop"]);
        let _ = self.by(&["services", "gc"]);
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

/// The pids whose parent is `parent`.
fn children(parent: u64) -> Vec<u64> {
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u64>().ok())
        .filter(|pid| {
            fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit(')')
                    .next()
                    .and_then(|rest| rest.split_whitespace().nth(1))
                    .and_then(|p| p.parse::<u64>().ok())
                    == Some(parent)
            })
        })
        .collect()
}

#[test]
fn an_unpinned_gateway_is_registered_found_and_reclaimed_after_its_supervisor_dies() {
    if !python() {
        eprintln!("skipped: python3 is not installed");
        return;
    }
    let repo = Repo::new();
    // Nothing yet: no registry, no gateway.
    let listed = repo.json(&["services", "--json"]);
    assert_eq!(listed["services"], serde_json::json!([]));
    let out = repo.by(&["gateway", "status"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("by gateway start"),
        "{}",
        stderr(&out)
    );

    // Started with no URL configured: a free port, registered.
    let started = repo.json(&["gateway", "start", "--json"]);
    assert_eq!(started["started"], true, "{started}");
    assert_eq!(started["listening"], true, "{started}");
    let url = started["url"].as_str().unwrap().to_owned();
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    let supervisor = started["pid"].as_u64().unwrap();
    // Healthy once the supervisor has seen it listen.
    let mut service = Value::Null;
    wait::until("the gateway's record to be healthy", || {
        service = repo.json(&["services", "--kind", "connector_gateway", "--json"])["services"][0]
            .clone();
        service["health"] == "healthy" && service["reclaim"]["type"] == "process"
    });
    assert_eq!(service["endpoints"][0]["url"], url.as_str());
    assert_eq!(
        service["capabilities"]["connectors"],
        serde_json::json!(["github"])
    );
    assert_eq!(service["owner"]["pid"], supervisor);
    assert_eq!(service["state"], "live");
    let gateway_pid = service["reclaim"]["pid"].as_u64().unwrap();
    assert_eq!(children(supervisor), [gateway_pid]);
    let text = stdout(&repo.by(&["services"]));
    assert!(text.contains("connector_gateway"), "{text}");
    assert!(text.contains(&format!("pid {supervisor}")), "{text}");

    // Consumers find it without a URL in any file.
    let status = repo.json(&["gateway", "status", "--json"]);
    assert_eq!(status["url"], url.as_str());
    assert_eq!(status["source"], "registry");
    assert_eq!(status["listening"], true);
    let out = repo.by(&["connect", "github"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("connect github via {url}")),
        "{}",
        stdout(&out)
    );
    // A second start finds the one that runs.
    let again = repo.json(&["gateway", "start", "--json"]);
    assert_eq!(again["started"], false, "{again}");

    // A pin still wins.
    repo.configure(Some("http://127.0.0.1:9/mcp"));
    let status = repo.json(&["gateway", "status", "--json"]);
    assert_eq!(status["url"], "http://127.0.0.1:9/mcp");
    assert_eq!(status["source"], "pinned");
    repo.configure(None);

    // The supervisor dies without stopping its gateway: the gateway leaks.
    let killed = Command::new("kill")
        .args(["-KILL", &supervisor.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    wait::until("the supervisor to die", || !wait::alive(supervisor));
    assert!(
        wait::alive(gateway_pid),
        "the gateway outlived its supervisor"
    );
    let reaped = repo.json(&["services", "gc", "--json"]);
    let reaped = reaped["reaped"].as_array().unwrap();
    let ours = reaped
        .iter()
        .find(|r| r["kind"] == "connector_gateway")
        .unwrap_or_else(|| panic!("{reaped:?}"));
    assert_eq!(ours["outcome"], "reclaimed", "{ours}");
    assert!(
        ours["detail"]
            .as_str()
            .unwrap()
            .contains(&format!("pid {gateway_pid}")),
        "{ours}"
    );
    wait::until("the leaked gateway to stop", || !wait::alive(gateway_pid));
    let listed = repo.json(&["services", "--all", "--json"]);
    let record = listed["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["kind"] == "connector_gateway")
        .unwrap()
        .clone();
    assert_eq!(record["state"], "reclaimed");
    // Not listed once reclaimed, and no gateway is found any more.
    assert_eq!(
        repo.json(&["services", "--json"])["services"],
        serde_json::json!([])
    );
    assert!(!repo.by(&["gateway", "status"]).status.success());
}

#[test]
fn a_pinned_gateway_started_elsewhere_is_adopted_and_never_reclaimed() {
    let repo = Repo::new();
    // Something this repository did not start listens at the pinned URL.
    let elsewhere = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", elsewhere.local_addr().unwrap());
    repo.configure(Some(&url));
    let adopted = repo.json(&["gateway", "start", "--json"]);
    assert_eq!(adopted["started"], false, "{adopted}");
    assert_eq!(adopted["adopted"], true, "{adopted}");
    let listed = repo.json(&["services", "--kind", "connector_gateway", "--json"]);
    let service = &listed["services"][0];
    assert_eq!(service["endpoints"][0]["url"], url.as_str());
    assert_eq!(service["capabilities"]["adopted"], true);
    assert_eq!(service["owner"]["pid"], 0);
    assert!(service.get("reclaim").is_none(), "{service}");
    // Nothing to reclaim, whatever a reaper does.
    let reaped = repo.json(&["services", "gc", "--json"]);
    assert_eq!(reaped["reaped"], serde_json::json!([]));
    assert!(std::net::TcpStream::connect(elsewhere.local_addr().unwrap()).is_ok());
}

/// The mock registry, its port, and the log of what it was asked.
fn mock(dir: &Path) -> (Child, u16, PathBuf) {
    let script = dir.join("mock-registry");
    fs::write(&script, MOCK_REGISTRY).unwrap();
    let log = dir.join("requests.log");
    let mut child = Command::new("python3")
        .arg(&script)
        .arg(&log)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    (child, line.trim().parse().unwrap(), log)
}

#[test]
fn catalogs_refresh_from_live_registries_with_etags_and_checksums() {
    if !python() {
        eprintln!("skipped: python3 is not installed");
        return;
    }
    let repo = Repo::new();
    let (mut server, port, log) = mock(&repo.dir);
    let base = format!("http://127.0.0.1:{port}");
    let refresh = [
        "catalog",
        "refresh",
        "--mcp-registry",
        &base,
        "--npm-registry",
        &base,
        "--json",
    ];
    // Nothing is fetched before asking.
    let status = repo.json(&["catalog", "status", "--json"]);
    assert_eq!(status["cached"], false);
    assert!(!log.exists());

    let first = repo.json(&refresh);
    assert_eq!(first["connectors"], 3, "{first}");
    let harnesses = first["harnesses"].as_u64().unwrap();
    assert!(harnesses >= 5, "{first}");
    assert_eq!(first["requests"].as_u64().unwrap(), 2 + harnesses);
    assert_eq!(first["not_modified"], 0);
    // Again: every response is unchanged, so each is a 304.
    let second = repo.json(&refresh);
    assert_eq!(second["requests"], first["requests"]);
    assert_eq!(second["not_modified"], second["requests"], "{second}");
    let lines = fs::read_to_string(&log).unwrap();
    let asked: Vec<&str> = lines.lines().collect();
    assert!(
        asked[0].starts_with("/v0/servers?limit=100&version=latest -"),
        "{lines}"
    );
    assert!(
        asked
            .iter()
            .any(|l| l.starts_with("/@openai%2fcodex/latest")),
        "{lines}"
    );
    let conditional = asked.iter().filter(|l| !l.ends_with(" -")).count();
    assert_eq!(conditional as u64, second["requests"].as_u64().unwrap());

    // Status verifies, and has the pins.
    let status = repo.json(&["catalog", "status", "--json"]);
    assert_eq!(status["verified"], true);
    let codex = status["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == "codex")
        .unwrap()
        .clone();
    assert_eq!(codex["package"], "@openai/codex");
    assert_eq!(codex["latest"], "9.9.9");
    assert_eq!(codex["integrity"], "sha512-pinned");

    // The connector catalog: the pinned baseline, then the registry's,
    // pinned at their versions and matched to the baseline.
    let catalog = repo.json(&["connectors", "catalog", "--json"]);
    let entries = catalog.as_array().unwrap();
    let live: Vec<&Value> = entries.iter().filter(|e| e["live"] == true).collect();
    assert_eq!(live.len(), 3);
    assert!(entries.len() > 50, "the baseline stays");
    let amplitude = live
        .iter()
        .find(|e| e["id"] == "com.amplitude/mcp")
        .unwrap();
    assert_eq!(amplitude["same_as"], "amplitude");
    assert_eq!(amplitude["source"], "mcp-registry:com.amplitude/mcp@2.0.0");
    let fs_entry = live.iter().find(|e| e["id"] == "io.example/fs").unwrap();
    assert_eq!(fs_entry["package"], "@example/fs@0.1.5");
    assert_eq!(fs_entry["credentials"], serde_json::json!(["FS_ROOT"]));

    // A cache that no longer matches its checksum is refused.
    let cached = repo.dir.join("catalog/connectors.json");
    let text = fs::read_to_string(&cached).unwrap();
    fs::write(&cached, text.replace("notes", "n0tes")).unwrap();
    let out = repo.by(&["catalog", "status"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("does not match its checksum"),
        "{}",
        stderr(&out)
    );
    let out = repo.by(&["connectors", "catalog", "--json"]);
    assert!(out.status.success());
    assert!(
        stderr(&out).contains("live connector catalog was refused"),
        "{}",
        stderr(&out)
    );
    let entries: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(entries
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e.get("live").is_none()));
    // A refresh mends it.
    let mended = repo.json(&refresh);
    assert_eq!(mended["connectors"], 3);
    assert_eq!(
        repo.json(&["catalog", "status", "--json"])["verified"],
        true
    );
    server.kill().unwrap();
    server.wait().unwrap();
}
