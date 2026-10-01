//! `by trigger` end to end: local actions on the store `by serve` uses,
//! a webhook fired by `by serve` on loopback, and the same actions through
//! `--remote`. Hermetic: the fake ACP agent is the harness; the webhook is
//! signed here. Requires `git`, `sh` and `kill`.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

const BY: &str = env!("CARGO_BIN_EXE_by");

fn fake_agent() -> &'static Path {
    static AGENT: OnceLock<PathBuf> = OnceLock::new();
    AGENT.get_or_init(|| {
        let by = PathBuf::from(BY);
        let profile_dir = by.parent().unwrap().to_path_buf();
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut command = Command::new(cargo);
        command
            .args(["build", "--quiet", "--offline", "--manifest-path"])
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
            .args(["-p", "branchyard-runtime", "--bin", "fake-acp-agent"])
            .env("CARGO_TARGET_DIR", profile_dir.parent().unwrap());
        if profile_dir.file_name().and_then(|n| n.to_str()) == Some("release") {
            command.arg("--release");
        }
        // Built already (as by this test's build) when cargo is not at hand.
        let agent = profile_dir.join("fake-acp-agent");
        if !agent.is_file() {
            assert!(command.status().unwrap().success());
        }
        assert!(agent.is_file());
        agent
    })
}

struct Dir(PathBuf);

impl Dir {
    fn new() -> Dir {
        fake_agent();
        let dir = std::env::temp_dir().join(format!(
            "branchyard-trigger-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Dir(fs::canonicalize(dir).unwrap())
    }

    fn repo(&self, name: &str) -> PathBuf {
        let root = self.0.join(name);
        fs::create_dir_all(&root).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "Test"],
            &["config", "user.email", "test@localhost"],
        ] {
            assert!(command("git", &root).args(args).status().unwrap().success());
        }
        fs::write(root.join("a.txt"), "one\n").unwrap();
        for args in [&["add", "."][..], &["commit", "-q", "-m", "initial"]] {
            assert!(command("git", &root).args(args).status().unwrap().success());
        }
        root
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn command(program: &str, dir: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("NO_COLOR", "1")
        .env(
            "BRANCHYARD_USER_CONFIG",
            "/nonexistent/branchyard-config.toml",
        )
        .env_remove("BRANCHYARD_REMOTE")
        .env_remove("BRANCHYARD_TOKEN_FILE")
        .env_remove("BRANCHYARD_REPO")
        .stdin(Stdio::null());
    command
}

fn by(root: &Path, args: &[&str]) -> Output {
    command(BY, root).args(args).output().unwrap()
}

fn ok(output: &Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn json(output: &Output) -> Value {
    serde_json::from_str(&ok(output)).unwrap()
}

/// `by serve` in `root`, on its default data directory, on a free
/// loopback port.
struct Served {
    child: Child,
    url: String,
}

impl Served {
    fn start(root: &Path) -> Served {
        let mut serve = command(BY, root);
        serve.args(["serve", "--listen", "127.0.0.1:0", "--quiet"]);
        serve
            .arg("--harness-command")
            .arg(format!("gemini-cli={}", fake_agent().display()));
        let log = fs::File::create(root.join("../server.log")).unwrap();
        let mut child = serve.stdout(Stdio::piped()).stderr(log).spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let url = line
            .trim()
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("server did not start: {line:?}"))
            .to_owned();
        Served { child, url }
    }

    fn remote(&self, root: &Path, args: &[&str]) -> Output {
        command(BY, root)
            .arg("--remote")
            .arg(&self.url)
            .arg("--token-file")
            .arg(root.join(".branchyard/server/token"))
            .args(args)
            .output()
            .unwrap()
    }

    fn post(&self, path: &str, headers: &str, body: &str) -> u16 {
        let addr = self.url.strip_prefix("http://").unwrap();
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{headers}\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn local_triggers_are_kept_where_by_serve_fires_them_and_remote_sees_them() {
    let d = Dir::new();
    let root = d.repo("app");
    let fake = |args: &[&str]| by(&root, args);

    let nightly = json(&fake(&[
        "trigger",
        "add",
        "nightly",
        "--every",
        "1h",
        "--prompt",
        "WRITE n.txt=1",
        "--harness",
        "gemini-cli",
        "--json",
    ]));
    assert!(
        nightly["trigger"]["next_due_ms"].as_u64().is_some(),
        "{nightly}"
    );
    assert_eq!(nightly["secret"], Value::Null);
    let hooks = json(&fake(&[
        "trigger",
        "add",
        "hooks",
        "--on",
        "generic",
        "--if",
        "label=agent",
        "--prompt",
        "WRITE {{event.text}}.txt=1",
        "--harness",
        "gemini-cli",
        "--branch-name",
        "hook-{{event.text}}",
        "--yes",
        "--json",
    ]));
    let secret = hooks["secret"]
        .as_str()
        .expect("a generated secret")
        .to_owned();
    let id = hooks["trigger"]["id"].as_str().unwrap().to_owned();
    assert!(hooks["trigger"]["webhook_url"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/v1/triggers/{id}/fire")));

    let list = ok(&fake(&["trigger", "list"]));
    assert!(
        list.contains("nightly") && list.contains("every 1h"),
        "{list}"
    );
    assert!(
        list.contains("hooks") && list.contains("on generic"),
        "{list}"
    );
    let shown = ok(&fake(&["trigger", "show", "hooks"]));
    assert!(shown.contains("if:       label=agent"), "{shown}");

    // A test renders the task and says whether it would fire.
    fs::write(
        d.0.join("yes.json"),
        r#"{"id":"t1","text":"tested","labels":["agent"]}"#,
    )
    .unwrap();
    fs::write(d.0.join("no.json"), r#"{"id":"t2","text":"tested"}"#).unwrap();
    let event = d.0.join("yes.json").display().to_string();
    let tested = json(&fake(&[
        "trigger", "test", "hooks", "--event", &event, "--json",
    ]));
    assert_eq!(tested["would_fire"], true, "{tested}");
    assert_eq!(tested["task"]["prompt"], "WRITE tested.txt=1");
    assert_eq!(tested["task"]["name"], "hook-tested");
    let event = d.0.join("no.json").display().to_string();
    let tested = ok(&fake(&["trigger", "test", "hooks", "--event", &event]));
    assert!(tested.contains("would fire: no"), "{tested}");
    assert!(tested.contains("no label agent"), "{tested}");
    let tested = json(&fake(&["trigger", "test", "nightly", "--json"]));
    assert_eq!(tested["would_fire"], true);

    // Refusals: a placeholder that names nothing; a precheck the server
    // does not allow.
    let bad = fake(&[
        "trigger",
        "add",
        "bad",
        "--on",
        "generic",
        "--prompt",
        "{{event.nope}}",
    ]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("not a placeholder"));
    let gated = fake(&[
        "trigger",
        "add",
        "gated",
        "--on",
        "generic",
        "--prompt",
        "x",
        "--precheck",
        "true",
    ]);
    assert!(!gated.status.success());
    assert!(String::from_utf8_lossy(&gated.stderr).contains("--allow-trigger-prechecks"));
    let off = ok(&fake(&["trigger", "disable", "nightly"]));
    assert!(off.contains("state:    disabled"), "{off}");
    let on = json(&fake(&["trigger", "enable", "nightly", "--json"]));
    assert_eq!(on["enabled"], true);

    // `by serve` fires a delivery to a trigger added locally.
    let served = Served::start(&root);
    let body = r#"{"id":"w1","text":"served","labels":["agent"]}"#;
    let signature = branchyard_server::triggers::events::sign(&secret, &[body.as_bytes()]);
    let status = served.post(
        &format!("/v1/triggers/{id}/fire"),
        &format!("X-Branchyard-Signature: sha256={signature}\r\n"),
        body,
    );
    assert_eq!(status, 202);
    let deadline = Instant::now() + Duration::from_secs(60);
    let runs = loop {
        let runs = json(&fake(&["trigger", "runs", "hooks", "--json"]));
        if runs["runs"][0]["outcome"].is_object() {
            break runs;
        }
        assert!(Instant::now() < deadline, "the run never settled: {runs}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(runs["runs"][0]["state"], "fired", "{runs}");
    assert_eq!(runs["runs"][0]["branches"][0], "hook-served");
    assert_eq!(runs["runs"][0]["outcome"]["ok"], true, "{runs}");
    let text = ok(&fake(&["trigger", "runs", "hooks"]));
    assert!(
        text.contains("fired") && text.contains("hook-served"),
        "{text}"
    );

    // The same store through the server's API.
    let remote = json(&served.remote(&root, &["trigger", "list", "--json"]));
    let names: Vec<&str> = remote["triggers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["nightly", "hooks"]);
    let made = json(&served.remote(
        &root,
        &[
            "trigger",
            "add",
            "remote-one",
            "--cron",
            "0 9 * * 1-5",
            "--tz",
            "Europe/Berlin",
            "--prompt",
            "Weekly review",
            "--json",
        ],
    ));
    assert_eq!(made["trigger"]["when"]["timezone"], "Europe/Berlin");
    let local = ok(&fake(&["trigger", "show", "remote-one"]));
    assert!(
        local.contains("cron \"0 9 * * 1-5\" Europe/Berlin"),
        "{local}"
    );
    let runs = json(&served.remote(&root, &["trigger", "runs", "hooks", "--json"]));
    assert_eq!(runs["runs"][0]["branches"][0], "hook-served");
    let removed = ok(&served.remote(&root, &["trigger", "rm", "remote-one"]));
    assert!(removed.contains("removed trigger remote-one"));
    let gone = fake(&["trigger", "show", "remote-one"]);
    assert!(!gone.status.success());
}
