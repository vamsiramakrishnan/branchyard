//! `[models]`, `--model-gateway`, `by models`, and what `by show`, `by log`
//! and `by stats` say about model calls, end to end with the built `by`
//! and the fake ACP agent. The upstream is a mock of Anthropic's API on
//! this machine's loopback; the harness is a Python script calling it
//! through the gateway by `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY`.
//! Requires `git`, `sh` and `python3`.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::thread;

use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

const KEY: &str = "sk-real-anthropic-cli";

const HARNESS: &str = r#"import json, os, sys, urllib.error, urllib.request
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
print("real keys in env:", ",".join(sorted(k for k, v in os.environ.items() if "sk-real" in v)) or "none")
for model in sys.argv[1:]:
    body = {"model": model, "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}
    request = urllib.request.Request(
        os.environ["ANTHROPIC_BASE_URL"] + "/v1/messages", data=json.dumps(body).encode(),
        headers={"x-api-key": os.environ["ANTHROPIC_API_KEY"], "anthropic-version": "2023-06-01",
                 "content-type": "application/json"}, method="POST")
    try:
        with opener.open(request, timeout=60) as response:
            print(f"call {model}: {response.status}")
    except urllib.error.HTTPError as error:
        print(f"call {model}: {error.code}")
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

/// A mock of Anthropic's Messages API: usage of 1000 input and 100 output
/// tokens per call, and the `x-api-key` of each request, sent back.
fn upstream() -> (u16, std::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, keys) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let sender = sender.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                let mut key = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("x-api-key:") {
                        key = line["x-api-key:".len()..].trim().to_owned();
                    }
                }
                let mut body = vec![0u8; length];
                let _ = reader.read_exact(&mut body);
                let model = serde_json::from_slice::<Value>(&body).unwrap_or_default()["model"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                let _ = sender.send(key);
                let body = json!({"type": "message", "model": model,
                    "content": [{"type": "text", "text": "hi"}],
                    "usage": {"input_tokens": 1000, "output_tokens": 100}})
                .to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    (port, keys)
}

struct Repo {
    dir: PathBuf,
    root: PathBuf,
    harness: PathBuf,
}

impl Repo {
    fn new(port: u16) -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-models-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let repo = Repo {
            root: dir.join("repo"),
            harness: dir.join("harness.py"),
            dir,
        };
        fs::write(&repo.harness, HARNESS).unwrap();
        fs::write(repo.dir.join("anthropic-key"), KEY).unwrap();
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@localhost"]);
        fs::write(repo.root.join("a.txt"), "one\n").unwrap();
        fs::write(
            repo.root.join("branchyard.toml"),
            format!(
                "version = 1\n\n[secrets]\nanthropic = \"@{}\"\n\n[models]\nallow = [\"claude-*\"]\n\n\
                 [models.backends.anthropic]\napi = \"anthropic\"\nurl = \"http://127.0.0.1:{port}\"\n\
                 key = \"anthropic\"\n\n[[models.routes]]\nmodel = \"claude-*\"\n\
                 backends = [\"anthropic\"]\nrequests_per_minute = 100\n\n\
                 [models.budget]\nmonthly_usd = 10.0\n",
                repo.dir.join("anthropic-key").display()
            ),
        )
        .unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "-m", "initial"]);
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
            "BRANCHYARD_REMOTE",
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

    fn run(&self, name: &str, flags: &[&str], models: &[&str]) -> Output {
        let agent = fake_agent().display().to_string();
        let prompt = format!("SH python3 {} {}", self.harness.display(), models.join(" "));
        let mut args = vec![
            "run",
            "--name",
            name,
            "--harness",
            "gemini-cli",
            "--command",
            &agent,
            "--yes",
        ];
        args.extend(flags);
        args.extend(["--", &prompt]);
        self.by(&args)
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.by(args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn text(&self, args: &[&str]) -> String {
        let out = self.by(args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn branches_use_the_gateway_by_configuration_and_every_surface_shows_it() {
    let (port, keys) = upstream();
    let repo = Repo::new(port);
    // `[models] allow` puts a new branch on the gateway.
    let out = repo.run("a", &[], &["claude-sonnet-4-6"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(keys.try_iter().collect::<Vec<_>>(), [KEY]);
    // Sonnet 4.6 at $3 in and $15 out per million.
    let cost = (1000.0 * 3.0 + 100.0 * 15.0) / 1e6;
    let shown = repo.json(&["show", "a", "--json"]);
    assert_eq!(shown["models"]["mode"], "gateway");
    assert_eq!(shown["models"]["models"], json!(["claude-*"]));
    assert_eq!(shown["models"]["calls"], 1);
    assert_eq!(shown["models"]["input_tokens"], 1000);
    assert_eq!(shown["models"]["output_tokens"], 100);
    let metered = shown["models"]["cost_usd"].as_f64().unwrap();
    assert!((metered - cost).abs() < 1e-12, "{metered}");
    assert_eq!(shown["cost_usd"].as_f64(), Some(metered));
    let text = repo.text(&["show", "a"]);
    assert!(
        text.contains("gateway (claude-*); 1 call, 1.0k in / 100 out tokens, $0.0045"),
        "{text}"
    );
    let log = repo.text(&["log", "a"]);
    assert!(log.contains("real keys in env: none"), "{log}");
    assert!(
        log.contains(
            "model: claude-sonnet-4-6 allowed via anthropic (200, 1000 in / 100 out, $0.0045"
        ),
        "{log}"
    );
    assert!(!log.contains(KEY));
    let log = repo.text(&["log", "a", "--json"]);
    assert!(log.contains(r#""activity": "model""#), "{log}");
    // A flag narrows what the configuration allows, for that branch.
    let out = repo.run(
        "b",
        &["--model-gateway=claude-haiku-*"],
        &["claude-sonnet-4-6"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let shown = repo.json(&["show", "b", "--json"]);
    assert_eq!(shown["models"]["models"], json!(["claude-haiku-*"]));
    assert_eq!(shown["models"]["calls"], 0);
    assert_eq!(shown["models"]["refused"], json!({"denied": 1}));
    assert!(
        keys.try_iter().next().is_none(),
        "a refused call was forwarded"
    );
    // `by models`: its backends, routes, budget and usage.
    let models = repo.json(&["models", "--json"]);
    assert_eq!(models["configured"], true);
    assert_eq!(models["backends"][0]["name"], "anthropic");
    assert_eq!(models["backends"][0]["key_set"], true);
    assert!(models["backends"][0]["key"]
        .as_str()
        .unwrap()
        .ends_with("anthropic-key"));
    assert_eq!(models["routes"][0]["requests_per_minute"], 100);
    assert_eq!(models["usage"]["calls"], 1);
    assert_eq!(models["by_model"]["claude-sonnet-4-6"]["calls"], 1);
    assert_eq!(models["budget"]["monthly_usd"], 10.0);
    assert_eq!(models["budget"]["this_month"]["calls"], 1);
    let text = repo.text(&["models"]);
    for line in [
        "backends",
        "anthropic      anthropic",
        "routes",
        "claude-*       anthropic; 100 requests a minute",
        "monthly $0.0045 of $10.00 (0%)",
        "usage this month",
        "1 call, 1.0k in / 100 out tokens, $0.0045",
    ] {
        assert!(text.contains(line), "{line:?} not in {text}");
    }
    assert!(!text.contains(KEY));
    // `by stats`: the calls by decision and their metered cost.
    let stats = repo.json(&["stats", "--json"]);
    assert_eq!(stats["model_calls"], json!({"allowed": 1, "denied": 1}));
    assert!((stats["model_cost_usd"].as_f64().unwrap() - cost).abs() < 1e-12);
    // A bad [models] is refused by name.
    fs::write(
        repo.root.join("branchyard.toml"),
        "version = 1\n\n[[models.routes]]\nmodel = \"*\"\nbackends = [\"nowhere\"]\n",
    )
    .unwrap();
    let out = repo.by(&["config", "validate"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no backend nowhere")
            || String::from_utf8_lossy(&out.stdout).contains("no backend nowhere")
    );
}
