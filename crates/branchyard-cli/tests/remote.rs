//! Remote mode end to end: the built `by` binary against `by serve`
//! spawned on 127.0.0.1, compared with the same commands run locally.
//! Hermetic: the fake ACP agent stands in for every harness. Requires
//! `git`, `sh` and `kill`.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

const BY: &str = env!("CARGO_BIN_EXE_by");

/// Built once per test binary, before any test starts timing.
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
        let agent = profile_dir.join("fake-acp-agent");
        assert!(agent.is_file());
        agent
    })
}

/// A temporary directory holding repositories. Removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Dir {
        fake_agent();
        let dir = std::env::temp_dir().join(format!(
            "branchyard-remote-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Dir(fs::canonicalize(dir).unwrap())
    }

    /// A repository at `<dir>/<name>` with one commit on `main`.
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
        assert!(command("git", &root)
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        assert!(command("git", &root)
            .args(["commit", "-q", "-m", "initial"])
            .status()
            .unwrap()
            .success());
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
        // Never a person's own ~/.config/branchyard/config.toml.
        .env(
            "BRANCHYARD_USER_CONFIG",
            "/nonexistent/branchyard-config.toml",
        )
        .env("PAGER", "cat")
        .env_remove("BRANCHYARD_REMOTE")
        .env_remove("BRANCHYARD_TOKEN_FILE")
        .env_remove("BRANCHYARD_REPO")
        .stdin(Stdio::null());
    command
}

/// `by serve` on an ephemeral loopback port. Stopped with SIGTERM on drop.
struct Served {
    child: Child,
    url: String,
    token_file: PathBuf,
}

impl Served {
    fn start(dir: &Path, repos: &[(&str, &Path)], extra: &[&str]) -> Served {
        let data = dir.join(format!("data-{}", COUNTER.fetch_add(1, Ordering::Relaxed)));
        let mut serve = command(BY, dir);
        serve.args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--quiet",
            "--shutdown-grace",
            "5",
            // Never this machine's real harnesses or usage files; the
            // inventory is tested in tests/harnesses.rs with fakes.
            "--no-inventory",
        ]);
        serve.arg("--data-dir").arg(&data);
        for (name, root) in repos {
            serve
                .arg("--repo")
                .arg(format!("{name}={}", root.display()));
        }
        serve.args(extra);
        let log = fs::File::create(dir.join("server.log")).unwrap();
        let mut child = serve.stdout(Stdio::piped()).stderr(log).spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let url = line
            .trim()
            .strip_prefix("listening on ")
            .unwrap_or_else(|| {
                let log = fs::read_to_string(dir.join("server.log")).unwrap_or_default();
                panic!("server did not start: {line:?}\n{log}")
            })
            .to_owned();
        Served {
            child,
            url,
            token_file: data.join("token"),
        }
    }

    fn stop(&mut self) -> Option<i32> {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        None
    }

    /// `by --remote URL --token-file F <args>`, run from `cwd`.
    fn by(&self, cwd: &Path, args: &[&str]) -> Output {
        command(BY, cwd)
            .arg("--remote")
            .arg(&self.url)
            .arg("--token-file")
            .arg(&self.token_file)
            .args(args)
            .output()
            .unwrap()
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.stop();
        }
    }
}

/// `args` with the fake agent as the harness command, and `gemini-cli` as
/// the harness where the command takes one and names none.
fn with_agent(args: &[&str]) -> Vec<String> {
    let mut all: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    if matches!(args[0], "run" | "fan" | "send" | "fork") {
        all.extend(["--command".into(), fake_agent().display().to_string()]);
        if args[0] == "run" && !args.contains(&"--harness") {
            all.extend(["--harness".into(), "gemini-cli".into()]);
        }
    }
    all
}

fn local(root: &Path, args: &[&str]) -> Output {
    command(BY, root).args(with_agent(args)).output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Replace what legitimately differs between two repositories: their
/// paths, commit IDs and times.
fn normalize(text: &str, root: &Path) -> String {
    let text = text.replace(&root.display().to_string(), "<root>");
    let mut out = String::new();
    for line in text.lines() {
        let line = match line.get(..24) {
            Some(stamp) if stamp.as_bytes()[10] == b'T' && stamp.ends_with('Z') => {
                format!("<time>{}", &line[24..])
            }
            _ => line.to_owned(),
        };
        out.push_str(&hex_runs(&line));
        out.push('\n');
    }
    out
}

/// Runs of 7 or more hex digits containing a digit become `<sha>`.
fn hex_runs(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let hex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
        if hex(chars[i]) && (i == 0 || !chars[i - 1].is_ascii_alphanumeric()) {
            let mut j = i;
            while j < chars.len() && hex(chars[j]) {
                j += 1;
            }
            let run = &chars[i..j];
            let bounded = j == chars.len() || !chars[j].is_ascii_alphanumeric();
            if bounded && run.len() >= 7 && run.iter().any(|c| c.is_ascii_digit()) {
                out.push_str("<sha>");
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// JSON with times zeroed, for structural comparison.
fn json(bytes: &[u8], root: &Path) -> Value {
    fn scrub(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, v) in map.iter_mut() {
                    if matches!(key.as_str(), "created_at" | "at_ms" | "acquired_at") {
                        *v = Value::from(0);
                    } else {
                        scrub(v);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(scrub),
            _ => {}
        }
    }
    let raw = text(bytes);
    let mut value: Value = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{e}: {raw}"));
    scrub(&mut value);
    let text = normalize(&serde_json::to_string_pretty(&value).unwrap(), root);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

#[test]
fn remote_commands_print_what_local_ones_do() {
    let dir = Dir::new();
    let here = dir.repo("here");
    let there = dir.repo("there");
    let server = Served::start(&dir.0, &[("app", &there)], &["--allow-client-commands"]);

    let both = |args: &[&str]| -> (Output, Output) {
        let l = local(&here, args);
        let r = server.by(
            &dir.0,
            &with_agent(args)
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        (l, r)
    };
    let same = |args: &[&str]| {
        let (l, r) = both(args);
        assert_eq!(
            l.status.code(),
            r.status.code(),
            "{args:?}\nlocal: {}\nremote: {}",
            text(&l.stderr),
            text(&r.stderr)
        );
        assert_eq!(
            normalize(&text(&l.stdout), &here),
            normalize(&text(&r.stdout), &there),
            "{args:?}\nremote stderr: {}",
            text(&r.stderr)
        );
        (l, r)
    };
    let same_json = |args: &[&str]| {
        let (l, r) = both(args);
        assert!(
            l.status.success() && r.status.success(),
            "{args:?}: {}",
            text(&r.stderr)
        );
        assert_eq!(json(&l.stdout, &here), json(&r.stdout, &there), "{args:?}");
    };

    let (_, r) = same(&["run", "WRITE hello.txt=hi", "--name", "hello", "--yes"]);
    assert!(
        text(&r.stdout).contains("wrote hello.txt"),
        "{}",
        text(&r.stdout)
    );
    assert!(
        text(&r.stderr).contains("remote mode on"),
        "{}",
        text(&r.stderr)
    );
    let (_, r) = same(&["run", "PERMISSION WRITE p.txt=1", "--name", "p"]);
    assert!(
        text(&r.stderr).contains("will be denied"),
        "{}",
        text(&r.stderr)
    );
    assert!(text(&r.stdout).contains("denied write marker"));
    same(&["send", "hello", "WHOAMI"]);
    same(&[
        "fork",
        "hello",
        "WRITE g.txt=2",
        "--fresh-session",
        "--name",
        "hello-alt",
        "--yes",
    ]);
    same(&["run", "HANG", "--name", "slow", "--max-minutes", "0.005"]);
    same(&["run", "EXIT", "--name", "exits"]);

    // Two branches interleave in either order; compare line sets.
    let (l, r) = both(&[
        "fan",
        "WRITE f.txt=x",
        "--harness",
        "gemini-cli,qwen-code",
        "--yes",
    ]);
    assert!(r.status.success(), "{}", text(&r.stderr));
    let lines = |out: &Output, root: &Path| {
        let mut lines: Vec<String> = normalize(&text(&out.stdout), root)
            .lines()
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    };
    assert_eq!(lines(&l, &here), lines(&r, &there));

    for args in [
        &["ls"][..],
        &["show", "hello"],
        &["show", "hello-alt"],
        &["diff", "hello"],
        &["log", "hello"],
        &["log", "p"],
        &["harnesses", "--profiles"],
    ] {
        same(args);
    }
    for args in [
        &["ls", "--json"][..],
        &["show", "hello", "--json"],
        &["log", "hello", "--json"],
        &["harnesses", "--profiles", "--json"],
    ] {
        same_json(args);
    }

    let (_, r) = same(&["merge", "hello"]);
    assert!(text(&r.stdout).starts_with("merged hello into main"));
    assert_eq!(fs::read_to_string(there.join("hello.txt")).unwrap(), "hi\n");
    same(&["rm", "hello-alt"]);
    same(&["ls"]);

    // Errors print the same message and exit the same way.
    for args in [
        &["merge", "nope"][..],
        &["merge", "hello"],
        &["diff", "nope"],
        &["send", "nope", "x"],
        &["run", "x", "--harness", "nope"],
        &["rm", "nope"],
        &["cancel", "nope"],
    ] {
        let (l, r) = same(args);
        assert_eq!(l.status.code(), Some(1), "{args:?}");
        assert_eq!(
            normalize(&text(&l.stderr), &here)
                .lines()
                .last()
                .map(str::to_owned),
            normalize(&text(&r.stderr), &there)
                .lines()
                .last()
                .map(str::to_owned),
            "{args:?}"
        );
    }
    assert_eq!(
        server.by(&dir.0, &["merge", "nope"]).stderr,
        b"by: no branch named nope\n"
    );
}

#[test]
fn by_cancel_stops_a_turn_on_the_server() {
    let dir = Dir::new();
    let there = dir.repo("there");
    let server = Served::start(&dir.0, &[("app", &there)], &["--allow-client-commands"]);
    let args = with_agent(&["run", "HANG", "--name", "held"]);
    let running = command(BY, &dir.0)
        .arg("--remote")
        .arg(&server.url)
        .arg("--token-file")
        .arg(&server.token_file)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let runner = std::thread::spawn(move || running.wait_with_output().unwrap());
    let deadline = Instant::now() + Duration::from_secs(60);
    while !text(&server.by(&dir.0, &["log", "held"]).stdout).contains("prompt: HANG") {
        assert!(Instant::now() < deadline, "the turn never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    let cancel = server.by(&dir.0, &["cancel", "held"]);
    assert!(cancel.status.success(), "{}", text(&cancel.stderr));
    assert_eq!(text(&cancel.stdout), "asked held to stop\n");
    let ran = runner.join().unwrap();
    assert!(ran.status.success(), "{}", text(&ran.stderr));
    assert!(
        text(&ran.stdout).contains("interrupted"),
        "{}",
        text(&ran.stdout)
    );
    let json_cancel = server.by(&dir.0, &["cancel", "held", "--json"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&json_cancel.stdout).unwrap(),
        serde_json::json!({"cancelled": []})
    );
}

#[test]
fn by_send_steer_reaches_a_turn_on_the_server() {
    let dir = Dir::new();
    let there = dir.repo("there");
    let server = Served::start(&dir.0, &[("app", &there)], &["--allow-client-commands"]);
    let args = with_agent(&["run", "AWAIT_STEER", "--name", "live"]);
    let running = command(BY, &dir.0)
        .arg("--remote")
        .arg(&server.url)
        .arg("--token-file")
        .arg(&server.token_file)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let runner = std::thread::spawn(move || running.wait_with_output().unwrap());
    let deadline = Instant::now() + Duration::from_secs(60);
    while !text(&server.by(&dir.0, &["log", "live"]).stdout).contains("waiting for steering") {
        assert!(Instant::now() < deadline, "the turn never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    let steer = server.by(
        &dir.0,
        &["send", "live", "--steer", "and the tests", "--json"],
    );
    assert!(steer.status.success(), "{}", text(&steer.stderr));
    let steered: Value = serde_json::from_slice(&steer.stdout).unwrap();
    assert_eq!(steered["branch"], "live");
    assert!(
        steered["by"]
            .as_str()
            .unwrap()
            .ends_with("through the server"),
        "{steered}"
    );
    assert!(
        matches!(
            steered["state"]["state"].as_str(),
            Some("delivered" | "accepted")
        ),
        "{steered}"
    );
    let ran = runner.join().unwrap();
    assert!(ran.status.success(), "{}", text(&ran.stderr));
    let log = text(&server.by(&dir.0, &["log", "live"]).stdout);
    assert!(log.contains("steered: and the tests"), "{log}");
    // No turn runs now: refused, as locally.
    let late = server.by(&dir.0, &["send", "live", "--steer", "late"]);
    assert!(!late.status.success());
    assert!(
        text(&late.stderr).contains("is not running a turn"),
        "{}",
        text(&late.stderr)
    );
}

#[test]
fn remote_mode_is_configured_by_flags_or_environment() {
    let dir = Dir::new();
    let one = dir.repo("one");
    let two = dir.repo("two");
    let agent = format!("gemini-cli={}", fake_agent().display());
    let server = Served::start(
        &dir.0,
        &[("one", &one), ("two", &two)],
        &["--harness-command", &agent],
    );

    // Two repositories: which one must be said.
    let ambiguous = server.by(&dir.0, &["ls"]);
    assert_eq!(ambiguous.status.code(), Some(1));
    assert_eq!(
        text(&ambiguous.stderr),
        "by: the server serves one, two; pass --repo NAME or set BRANCHYARD_REPO\n"
    );
    let run = server.by(
        &dir.0,
        &[
            "--repo",
            "two",
            "run",
            "WRITE t.txt=2",
            "--name",
            "t",
            "--harness",
            "gemini-cli",
            "--yes",
        ],
    );
    assert!(run.status.success(), "{}", text(&run.stderr));

    // The same through the environment.
    let listed = command(BY, &dir.0)
        .env("BRANCHYARD_REMOTE", &server.url)
        .env("BRANCHYARD_TOKEN_FILE", &server.token_file)
        .env("BRANCHYARD_REPO", "two")
        .args(["ls", "--json"])
        .output()
        .unwrap();
    assert!(listed.status.success(), "{}", text(&listed.stderr));
    let branches: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(branches[0]["name"], "t");
    assert_eq!(branches[0]["status"]["state"], "ready");

    // This server keeps executables to its own configuration.
    let refused = server.by(
        &dir.0,
        &["--repo", "one", "run", "x", "--command", "/bin/sh", "--yes"],
    );
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("does not accept a request's own command"),
        "{}",
        text(&refused.stderr)
    );
    let ask = server.by(&dir.0, &["--repo", "one", "run", "x", "--ask"]);
    assert_eq!(ask.status.code(), Some(1));
    assert!(text(&ask.stderr).contains("--ask is not available in remote mode"));

    // Credentials and reachability.
    let wrong = dir.0.join("wrong.token");
    fs::write(&wrong, "not-the-token-0123456789\n").unwrap();
    let refused = command(BY, &dir.0)
        .args(["--remote", &server.url, "--token-file"])
        .arg(&wrong)
        .args(["--repo", "one", "ls"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(
        text(&refused.stderr),
        "by: a valid bearer token is required\n"
    );
    let missing = command(BY, &dir.0)
        .args(["--remote", &server.url, "ls"])
        .output()
        .unwrap();
    assert!(text(&missing.stderr).contains("remote mode needs a token"));
    let local_only = command(BY, &one)
        .args(["--repo", "one", "ls"])
        .output()
        .unwrap();
    assert!(text(&local_only.stderr).contains("pass --remote URL too"));
    let serve_remote = command(BY, &dir.0)
        .args(["--remote", &server.url, "serve"])
        .output()
        .unwrap();
    assert_eq!(serve_remote.status.code(), Some(2));

    // Watching remotely shows the branch and its activity.
    let watched = server.by(&dir.0, &["--repo", "two", "watch", "--once"]);
    assert!(watched.status.success(), "{}", text(&watched.stderr));
    let frame = text(&watched.stdout);
    assert!(
        frame.contains(&server.url) && frame.contains("(two)"),
        "{frame}"
    );
    assert!(
        frame.contains("t  ") && frame.contains("wrote t.txt"),
        "{frame}"
    );

    let mut server = server;
    assert_eq!(server.stop(), Some(0), "a clean shutdown");
    let gone = command(BY, &dir.0)
        .args(["--remote", &server.url, "--token-file"])
        .arg(&server.token_file)
        .args(["--repo", "one", "ls"])
        .output()
        .unwrap();
    assert_eq!(gone.status.code(), Some(1));
    assert!(
        text(&gone.stderr).starts_with(&format!("by: cannot reach {}", server.url)),
        "{}",
        text(&gone.stderr)
    );
}

#[test]
fn serve_refuses_a_public_address_without_tls() {
    let dir = Dir::new();
    let root = dir.repo("r");
    let out = command(BY, &root)
        .args(["serve", "--listen", "0.0.0.0:0", "--data-dir"])
        .arg(dir.0.join("data"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("refusing to serve plain HTTP on 0.0.0.0:0"),
        "{}",
        text(&out.stderr)
    );
    let help = command(BY, &root).args(["help", "serve"]).output().unwrap();
    assert!(text(&help.stdout).contains("--insecure-bind"));
}

#[test]
fn delegation_commands_print_what_local_ones_do() {
    let dir = Dir::new();
    let here = dir.repo("here");
    let there = dir.repo("there");
    let server = Served::start(
        &dir.0,
        &[("app", &there)],
        &["--allow-client-commands", "--allow-delegation"],
    );
    let both = |args: &[&str]| -> (Output, Output) {
        let l = local(&here, args);
        let r = server.by(
            &dir.0,
            &with_agent(args)
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        (l, r)
    };
    // The same exit status, and the same JSON on stdout.
    let same_json = |args: &[&str]| -> Value {
        let (l, r) = both(args);
        assert_eq!(
            l.status.code(),
            r.status.code(),
            "{args:?}\nlocal: {}\nremote: {}",
            text(&l.stderr),
            text(&r.stderr)
        );
        let (l, r) = (json(&l.stdout, &here), json(&r.stdout, &there));
        assert_eq!(l, r, "{args:?}");
        r
    };
    let same_text = |args: &[&str]| {
        let (l, r) = both(args);
        assert_eq!(l.status.code(), r.status.code(), "{args:?}");
        assert_eq!(
            normalize(&text(&l.stdout), &here),
            normalize(&text(&r.stdout), &there),
            "{args:?}\nremote stderr: {}",
            text(&r.stderr)
        );
    };

    for args in [
        &["run", "say hi", "--name", "root", "--delegate=2", "--yes"][..],
        &["run", "say hi", "--name", "plain", "--yes"],
    ] {
        let (l, r) = both(args);
        assert!(
            l.status.success() && r.status.success(),
            "{}",
            text(&r.stderr)
        );
    }
    let kid = same_json(&[
        "spawn",
        "WRITE kid.txt=k",
        "--parent",
        "root",
        "--name",
        "kid",
        "--yes",
        "--json",
    ]);
    assert_eq!(kid["status"]["state"], "ready");
    assert_eq!(kid["envelope"]["max_depth"], 1);
    same_json(&["inspect", "kid", "--json"]);
    same_json(&["inspect", "root", "--json"]);
    same_json(&["children", "root", "--json"]);
    same_json(&["events", "kid", "--cursor", "0", "--limit", "2", "--json"]);
    same_json(&["events", "kid", "--json"]);
    let sent = same_json(&["send", "kid", "WHOAMI", "--json"]);
    assert_eq!(sent["name"], "kid");
    same_text(&["inspect", "kid"]);
    same_text(&["events", "kid", "--cursor", "1", "--limit", "3"]);
    let merged = same_json(&["integrate", "kid", "--json"]);
    assert_eq!(merged["target"], "by/root");
    assert_eq!(
        command("git", &there)
            .args(["show", "by/root:kid.txt"])
            .output()
            .unwrap()
            .stdout,
        b"k\n"
    );

    // Refusals are the same JSON errors.
    for args in [
        &["spawn", "x", "--parent", "plain", "--json"][..],
        &["spawn", "x", "--json"],
        &["integrate", "plain", "--json"],
        &["inspect", "nope", "--json"],
        &["children", "--json"],
        &["events", "nope", "--json"],
    ] {
        let error = same_json(args);
        assert!(error["error"]["kind"].is_string(), "{args:?}: {error}");
    }
    let (l, r) = both(&["integrate", "plain"]);
    assert_eq!(l.status.code(), Some(1));
    assert_eq!(text(&l.stderr), text(&r.stderr));
}

#[test]
fn a_harness_on_the_server_delegates_with_by_in_its_shell() {
    let dir = Dir::new();
    let there = dir.repo("there");
    let prompt = [
        "SH by spawn 'WRITE kid.txt=k' --name kid --wait --json",
        "SH by integrate kid --json",
    ]
    .join("\n");
    let args = with_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    // Refused unless the operator offers delegation.
    let plain = Served::start(&dir.0, &[("app", &there)], &["--allow-client-commands"]);
    let refused = plain.by(&dir.0, &args);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("does not offer delegation"),
        "{}",
        text(&refused.stderr)
    );
    drop(plain);

    let server = Served::start(
        &dir.0,
        &[("app", &there)],
        &["--allow-client-commands", "--allow-delegation"],
    );
    let out = server.by(&dir.0, &args);
    assert!(
        out.status.success(),
        "{}\n{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("delegated") && stdout.contains("kid"),
        "{stdout}"
    );
    let log = text(&server.by(&dir.0, &["log", "root", "--json"]).stdout);
    assert!(
        log.contains("\"sh: 0\\n"),
        "the harness's by commands succeeded: {log}"
    );
    let root: Value =
        serde_json::from_slice(&server.by(&dir.0, &["show", "root", "--json"]).stdout).unwrap();
    assert_eq!(root["children"], serde_json::json!(["kid"]));
    let kid: Value =
        serde_json::from_slice(&server.by(&dir.0, &["show", "kid", "--json"]).stdout).unwrap();
    assert_eq!(kid["status"]["state"], "merged");
    assert_eq!(
        command("git", &there)
            .args(["show", "by/root:kid.txt"])
            .output()
            .unwrap()
            .stdout,
        b"k\n"
    );
}

#[test]
fn unapproved_tools_need_the_operators_consent() {
    let dir = Dir::new();
    let there = dir.repo("there");
    let args = with_agent(&[
        "run",
        "WRITE u.txt=1",
        "--name",
        "u",
        "--allow-unapproved-tools",
        "--yes",
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let plain = Served::start(&dir.0, &[("app", &there)], &["--allow-client-commands"]);
    let refused = plain.by(&dir.0, &args);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("--allow-unapproved-tools"),
        "{}",
        text(&refused.stderr)
    );
    drop(plain);
    let server = Served::start(
        &dir.0,
        &[("app", &there)],
        &["--allow-client-commands", "--allow-unapproved-tools"],
    );
    let out = server.by(&dir.0, &args);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("wrote u.txt"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn remote_secrets_come_from_the_servers_table() {
    let dir = Dir::new();
    let there = dir.repo("there");
    // Inherited by the server, which reads it; the client never sees it.
    std::env::set_var("BY_TEST_SERVE_GEMINI", "served-key");
    let server = Served::start(
        &dir.0,
        &[("app", &there)],
        &[
            "--allow-client-commands",
            "--secret",
            "GEMINI_API_KEY=BY_TEST_SERVE_GEMINI",
        ],
    );
    let args = with_agent(&[
        "run",
        "SH test ${#GEMINI_API_KEY} -eq 10 && echo key-from-the-server",
        "--name",
        "s",
        "--isolated",
        "--secret",
        "GEMINI_API_KEY",
        "--model",
        "gemini-served",
        "--yes",
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = server.by(&dir.0, &args);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("key-from-the-server"),
        "{}",
        text(&out.stdout)
    );
    assert!(
        text(&out.stdout).contains("provisioned: auth api-key"),
        "{}",
        text(&out.stdout)
    );

    // A source is the server's to choose.
    let args = with_agent(&[
        "run",
        "x",
        "--isolated",
        "--secret",
        "GEMINI_API_KEY=HOME",
        "--yes",
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let refused = server.by(&dir.0, &args);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("secrets come from the server"),
        "{}",
        text(&refused.stderr)
    );
    // A secret the server does not define.
    let args = with_agent(&[
        "run",
        "x",
        "--isolated",
        "--secret",
        "OPENAI_API_KEY",
        "--yes",
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let refused = server.by(&dir.0, &args);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("--secret OPENAI_API_KEY"),
        "{}",
        text(&refused.stderr)
    );
}

/// `by serve --database`: the same commands with the server's state in
/// PostgreSQL. Runs with the `postgres` feature when
/// `BY_TEST_POSTGRES_URL` is set.
#[cfg(feature = "postgres")]
#[test]
fn by_serve_keeps_its_state_in_postgres() {
    let Some(url) = std::env::var("BY_TEST_POSTGRES_URL")
        .ok()
        .filter(|u| !u.is_empty())
    else {
        eprintln!("skipped: set BY_TEST_POSTGRES_URL to run the PostgreSQL remote test");
        return;
    };
    let dir = Dir::new();
    let there = dir.repo("there");
    // A repository name no other run uses, since it scopes the state.
    let name = format!("pg-{}", std::process::id());
    let server = Served::start(
        &dir.0,
        &[(&name, &there)],
        &["--allow-client-commands", "--database", &url],
    );
    let args = with_agent(&["run", "WRITE p.txt=1", "--name", "p", "--yes"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = server.by(&dir.0, &args);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let listed = server.by(&dir.0, &["ls", "--json"]);
    let branches: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(branches[0]["name"], "p");
    assert_eq!(branches[0]["status"]["state"], "ready");
    assert!(!there.join(".branchyard/state.db").exists());
}

/// A rig of the fake agent for both sides: a lead that may spawn two
/// workers.
const TEAM: &str = r#"
version = 1
name = "team"
root = "lead"

[seats.lead]
harness = "gemini-cli"
delegates_to = ["worker"]
policy = { default = "allow", delegation_commands = true }

[seats.worker]
description = "Writes files."
instances = 2
policy = { deny = ["Edit"] }
"#;

#[test]
fn rig_runs_print_what_local_ones_do() {
    let dir = Dir::new();
    let here = dir.repo("here");
    let there = dir.repo("there");
    let spec = dir.0.join("team.toml");
    fs::write(&spec, TEAM).unwrap();
    let spec = spec.display().to_string();
    let agent = fake_agent().display().to_string();
    let prompt = [
        "SH by spawn --seat worker 'WRITE w.txt=w' --wait --json",
        "SH by integrate team-worker --json",
    ]
    .join("\n");
    let run = ["rig", "run", &spec, &prompt, "--command", &agent, "--json"];

    // A rig's root delegates, so the operator must allow delegation.
    let plain = Served::start(&dir.0, &[("app", &there)], &["--allow-client-commands"]);
    let refused = plain.by(&dir.0, &run);
    assert_eq!(refused.status.code(), Some(1));
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(refused["error"]["kind"], "delegation_not_allowed");
    drop(plain);

    let server = Served::start(
        &dir.0,
        &[("app", &there)],
        &["--allow-client-commands", "--allow-delegation"],
    );
    let l = command(BY, &here).args(run).output().unwrap();
    let r = server.by(&dir.0, &run);
    assert!(l.status.success(), "{}", text(&l.stderr));
    assert!(
        r.status.success(),
        "{}\n{}",
        text(&r.stdout),
        text(&r.stderr)
    );
    let (lj, rj) = (json(&l.stdout, &here), json(&r.stdout, &there));
    assert_eq!(lj, rj);
    assert_eq!(rj["root"]["children"], serde_json::json!(["team-worker"]));
    assert_eq!(rj["descendants"][0]["status"]["state"], "merged");
    assert_eq!(
        command("git", &there)
            .args(["show", "by/team:w.txt"])
            .output()
            .unwrap()
            .stdout,
        b"w\n"
    );

    // Filling a seat as a person: the same JSON, and the same refusals.
    for args in [
        &[
            "spawn",
            "WRITE x.txt=x",
            "--parent",
            "team",
            "--seat",
            "worker",
            "--yes",
            "--json",
        ][..],
        &["spawn", "x", "--parent", "team", "--yes", "--json"],
        &[
            "spawn", "x", "--parent", "team", "--seat", "boss", "--yes", "--json",
        ],
        &["inspect", "team", "--json"],
    ] {
        let l = command(BY, &here).args(args).output().unwrap();
        let r = server.by(&dir.0, args);
        assert_eq!(
            l.status.code(),
            r.status.code(),
            "{args:?}\n{}",
            text(&r.stderr)
        );
        assert_eq!(json(&l.stdout, &here), json(&r.stdout, &there), "{args:?}");
    }
    let inspected: Value = serde_json::from_slice(
        &server
            .by(&dir.0, &["inspect", "team-worker-2", "--json"])
            .stdout,
    )
    .unwrap();
    assert_eq!(inspected["seat"], "worker");

    // A seat's secrets are names the operator must define, like a task's.
    let secret = dir.0.join("secret.toml");
    fs::write(
        &secret,
        TEAM.replace("name = \"team\"", "name = \"vault\"").replace(
            "instances = 2",
            "instances = 2\nisolated = true\nsecrets = [\"UNDEFINED_KEY\"]",
        ),
    )
    .unwrap();
    let secret = secret.display().to_string();
    let refused = server.by(&dir.0, &["rig", "run", &secret, "go", "--json"]);
    assert_eq!(refused.status.code(), Some(1));
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(refused["error"]["kind"], "secret_not_allowed", "{refused}");
}

/// `by artifact` and `by scratch` against `--remote`: the same JSON as
/// local mode. Artifact ids are per-repository sequence numbers
/// (`art1`, `art2`, …), so running the same sequence of commands against
/// two freshly created, identical repositories gives identical ids too;
/// nothing needs to be scrubbed beyond what `json()` already normalizes.
#[test]
fn artifact_and_scratch_commands_print_what_local_ones_do() {
    let dir = Dir::new();
    let here = dir.repo("here");
    let there = dir.repo("there");
    let server = Served::start(&dir.0, &[("app", &there)], &["--allow-client-commands"]);

    // A branch to act as, created identically in both repositories, plus
    // an unrelated one to stand in for a sibling that needs an explicit
    // share.
    for name in ["hello", "buddy"] {
        let args = ["run", "WRITE hello.txt=hi", "--name", name, "--yes"];
        let l = local(&here, &args);
        assert!(l.status.success(), "{}", text(&l.stderr));
        let r = server.by(
            &dir.0,
            &with_agent(&args)
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        assert!(r.status.success(), "{}", text(&r.stderr));
    }

    let payload = dir.0.join("payload.txt");
    fs::write(&payload, "shared artifact bytes\n").unwrap();
    let payload = payload.display().to_string();
    let out_local = dir.0.join("out-local.bin");
    let out_remote = dir.0.join("out-remote.bin");
    let out_local2 = dir.0.join("out-local-2.bin");
    let out_remote2 = dir.0.join("out-remote-2.bin");

    let both =
        |args: &[&str]| -> (Output, Output) { (local(&here, args), server.by(&dir.0, args)) };
    let same_json = |args: &[&str]| -> (Value, Value) {
        let (l, r) = both(args);
        assert!(
            l.status.success() && r.status.success(),
            "{args:?}\nlocal: {}\nremote: {}",
            text(&l.stderr),
            text(&r.stderr)
        );
        let (lj, rj) = (json(&l.stdout, &here), json(&r.stdout, &there));
        assert_eq!(lj, rj, "{args:?}");
        (lj, rj)
    };

    let (_, published) = same_json(&[
        "artifact",
        "publish",
        &payload,
        "--name",
        "greeting.txt",
        "--label",
        "k=v",
        "--branch",
        "hello",
        "--json",
    ]);
    let id = published["id"].as_str().unwrap().to_owned();
    same_json(&["artifact", "list", "--branch", "hello", "--json"]);

    let l = local(
        &here,
        &[
            "artifact",
            "get",
            &id,
            "--out",
            out_local.to_str().unwrap(),
            "--branch",
            "hello",
        ],
    );
    let r = server.by(
        &dir.0,
        &[
            "artifact",
            "get",
            &id,
            "--out",
            out_remote.to_str().unwrap(),
            "--branch",
            "hello",
        ],
    );
    assert!(
        l.status.success() && r.status.success(),
        "{}",
        text(&r.stderr)
    );
    assert_eq!(
        text(&l.stdout).split(" to ").next(),
        text(&r.stdout).split(" to ").next()
    );
    assert_eq!(fs::read(&out_local).unwrap(), fs::read(&payload).unwrap());
    assert_eq!(fs::read(&out_remote).unwrap(), fs::read(&payload).unwrap());

    // Refused for the unrelated branch until shared; the same error kind
    // either way.
    let denied_local = local(
        &here,
        &[
            "artifact",
            "get",
            &id,
            "--out",
            out_local2.to_str().unwrap(),
            "--branch",
            "buddy",
            "--json",
        ],
    );
    let denied_remote = server.by(
        &dir.0,
        &[
            "artifact",
            "get",
            &id,
            "--out",
            out_remote2.to_str().unwrap(),
            "--branch",
            "buddy",
            "--json",
        ],
    );
    assert_eq!(denied_local.status.code(), Some(1));
    assert_eq!(denied_remote.status.code(), Some(1));
    let (lj, rj): (Value, Value) = (
        serde_json::from_slice(&denied_local.stdout).unwrap(),
        serde_json::from_slice(&denied_remote.stdout).unwrap(),
    );
    assert_eq!(lj["error"]["kind"], rj["error"]["kind"]);

    same_json(&[
        "artifact", "share", &id, "--to", "buddy", "--branch", "hello", "--json",
    ]);
    let l = local(
        &here,
        &[
            "artifact",
            "get",
            &id,
            "--out",
            out_local2.to_str().unwrap(),
            "--branch",
            "buddy",
        ],
    );
    let r = server.by(
        &dir.0,
        &[
            "artifact",
            "get",
            &id,
            "--out",
            out_remote2.to_str().unwrap(),
            "--branch",
            "buddy",
        ],
    );
    assert!(
        l.status.success() && r.status.success(),
        "{}",
        text(&r.stderr)
    );

    // Scratch areas: create, list, lock, unlock, share.
    same_json(&["scratch", "create", "cache", "--branch", "hello", "--json"]);
    same_json(&["scratch", "list", "--branch", "hello", "--json"]);
    same_json(&["scratch", "lock", "cache", "--branch", "hello", "--json"]);
    same_json(&["scratch", "unlock", "cache", "--branch", "hello", "--json"]);
    same_json(&[
        "scratch", "share", "cache", "--to", "buddy", "--branch", "hello", "--json",
    ]);
    same_json(&["scratch", "list", "--branch", "buddy", "--json"]);

    // Outside a harness, both modes need --branch; the messages differ (no
    // harness to delegate as, in either mode) but both refuse the same way.
    let l = local(&here, &["artifact", "list"]);
    let r = server.by(&dir.0, &["artifact", "list"]);
    assert_eq!(l.status.code(), Some(1));
    assert_eq!(r.status.code(), Some(1));
}

#[test]
fn graph_commands_print_what_local_ones_do() {
    let dir = Dir::new();
    let here = dir.repo("here");
    let there = dir.repo("there");
    let server = Served::start(
        &dir.0,
        &[("app", &there)],
        &["--allow-client-commands", "--allow-delegation"],
    );
    let both = |args: &[&str]| -> (Output, Output) {
        let l = local(&here, args);
        let r = server.by(
            &dir.0,
            &with_agent(args)
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        (l, r)
    };
    let same_json = |args: &[&str]| -> Value {
        let (l, r) = both(args);
        assert_eq!(
            l.status.code(),
            r.status.code(),
            "{args:?}\nlocal: {}\nremote: {}",
            text(&l.stderr),
            text(&r.stderr)
        );
        let (l, r) = (json(&l.stdout, &here), json(&r.stdout, &there));
        assert_eq!(l, r, "{args:?}");
        r
    };
    // The server runs children on its own threads; wait there for them as
    // the local command already did.
    let settled = |names: &[&str]| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let graph = json(
                &server
                    .by(&dir.0, &["graph", "show", "root", "--json"])
                    .stdout,
                &there,
            );
            let done = graph["children"].as_array().unwrap().iter().all(|c| {
                !names.contains(&c["name"].as_str().unwrap())
                    || !matches!(c["status"]["state"].as_str(), Some("running"))
            });
            if done {
                return;
            }
            assert!(Instant::now() < deadline, "{graph}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let (l, r) = both(&["run", "say hi", "--name", "root", "--delegate", "--yes"]);
    assert!(
        l.status.success() && r.status.success(),
        "{}",
        text(&r.stderr)
    );
    assert_eq!(
        same_json(&["graph", "show", "root", "--json"])["revision"],
        0
    );
    let edits = r#"[{"kind":"spawn","prompt":"WRITE lib.txt=1","name":"lib"},
        {"kind":"spawn","prompt":"WRITE app.txt=1","name":"app","depends_on":["lib"],"after":"integrated"},
        {"kind":"spawn","prompt":"EXIT","name":"bad"},
        {"kind":"spawn","prompt":"WRITE x.txt=1","name":"blocked","depends_on":["bad"]}]"#;
    let applied = same_json(&[
        "graph",
        "apply",
        "--parent",
        "root",
        "--edits",
        edits,
        "--expected-revision",
        "0",
        "--yes",
        "--json",
    ]);
    assert_eq!(applied["revision"], 1);
    assert_eq!(applied["spawned"][1]["status"]["state"], "waiting");
    settled(&["lib", "bad", "blocked"]);
    let shown = same_json(&["graph", "show", "root", "--json"]);
    assert_eq!(shown["children"][1]["status"]["state"], "waiting");
    assert_eq!(shown["children"][3]["status"]["state"], "blocked");
    same_json(&["inspect", "app", "--json"]);
    same_json(&["inspect", "root", "--json"]);
    // Integrating the prerequisite starts its dependent, from the merge.
    let merged = same_json(&["integrate", "lib", "--json"]);
    assert_eq!(merged["target"], "by/root");
    settled(&["app"]);
    let shown = same_json(&["graph", "show", "root", "--json"]);
    assert_eq!(shown["children"][1]["status"]["state"], "ready");
    same_json(&["inspect", "app", "--json"]);
    // Refusals are the same JSON errors, and change nothing.
    for (args, kind) in [
        (
            &[
                "graph",
                "apply",
                "--parent",
                "root",
                "--edits",
                edits,
                "--expected-revision",
                "0",
                "--json",
            ][..],
            "stale_revision",
        ),
        (
            &[
                "graph",
                "apply",
                "--parent",
                "root",
                "--edits",
                r#"[{"kind":"add_dependency","dependent":"app","prerequisite":"lib"}]"#,
                "--expected-revision",
                "1",
                "--json",
            ],
            "denied",
        ),
        (
            &[
                "graph",
                "apply",
                "--parent",
                "root",
                "--edits",
                r#"[{"kind":"spawn","prompt":"x","name":"p","depends_on":["q"]},{"kind":"spawn","prompt":"y","name":"q","depends_on":["p"]}]"#,
                "--expected-revision",
                "1",
                "--json",
            ],
            "denied",
        ),
        (&["graph", "show", "nope", "--json"], "unknown_branch"),
    ] {
        let error = same_json(args);
        assert_eq!(error["error"]["kind"], kind, "{args:?}: {error}");
    }
    assert_eq!(
        same_json(&["graph", "show", "root", "--json"])["revision"],
        1
    );
}

#[test]
fn the_server_comes_from_flags_after_the_command_or_the_environment() {
    let dir = Dir::new();
    let there = dir.repo("there");
    let server = Served::start(&dir.0, &[("app", &there)], &[]);
    let expected = text(&server.by(&dir.0, &["ls", "--json"]).stdout);
    assert_eq!(expected.trim(), "[]");
    // Global options after the command, and after its own flags.
    let after = command(BY, &dir.0)
        .args(["ls", "--json", "--remote", &server.url, "--token-file"])
        .arg(&server.token_file)
        .output()
        .unwrap();
    assert!(after.status.success(), "{}", text(&after.stderr));
    assert_eq!(text(&after.stdout), expected);
    // Only the environment, as clap's `env` reads it.
    let env = command(BY, &dir.0)
        .args(["ls", "--json"])
        .env("BRANCHYARD_REMOTE", &server.url)
        .env("BRANCHYARD_TOKEN_FILE", &server.token_file)
        .env("BRANCHYARD_REPO", "app")
        .output()
        .unwrap();
    assert!(env.status.success(), "{}", text(&env.stderr));
    assert_eq!(text(&env.stdout), expected);
    // A flag wins over its variable.
    let flag_wins = command(BY, &dir.0)
        .args(["ls", "--json", "--repo", "nope"])
        .env("BRANCHYARD_REMOTE", &server.url)
        .env("BRANCHYARD_TOKEN_FILE", &server.token_file)
        .env("BRANCHYARD_REPO", "app")
        .output()
        .unwrap();
    assert_eq!(flag_wins.status.code(), Some(1));
    assert!(
        text(&flag_wins.stderr).contains("nope"),
        "{}",
        text(&flag_wins.stderr)
    );
}

#[test]
fn compare_works_remotely_and_local_only_commands_say_so() {
    let dir = Dir::new();
    let there = dir.repo("there");
    let server = Served::start(&dir.0, &[("app", &there)], &["--allow-client-commands"]);
    for (name, prompt) in [("one", "WRITE a.txt=1"), ("two", "WRITE b.txt=2")] {
        let args = with_agent(&["run", prompt, "--name", name, "--yes"]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let ran = server.by(&dir.0, &args);
        assert!(ran.status.success(), "{}", text(&ran.stderr));
    }
    let table = server.by(&dir.0, &["compare", "one", "two"]);
    assert!(table.status.success(), "{}", text(&table.stderr));
    let table = text(&table.stdout);
    assert!(table.contains("UNIQUE FILES"), "{table}");
    assert!(table.lines().nth(1).unwrap().starts_with("one"), "{table}");
    let json: Value = serde_json::from_slice(
        &server
            .by(&dir.0, &["compare", "one", "two", "--json"])
            .stdout,
    )
    .unwrap();
    assert_eq!(json[1]["unique_files"], serde_json::json!(["b.txt"]));
    let show = text(&server.by(&dir.0, &["show", "one"]).stdout);
    assert!(
        show.contains("checkpoints") && show.contains("* 1  "),
        "{show}"
    );
    for args in [
        &["try", "one"][..],
        &["rewind", "one", "--to", "0", "--yes"],
        &["compare", "one", "two", "--diff", "one", "two"],
    ] {
        let refused = server.by(&dir.0, args);
        assert!(!refused.status.success(), "{args:?}");
        assert!(
            text(&refused.stderr).contains("remote mode"),
            "{args:?}: {}",
            text(&refused.stderr)
        );
    }
}

#[test]
fn work_requiring_a_label_no_worker_carries_says_why_in_by_show() {
    let dir = Dir::new();
    let there = dir.repo("there");
    let server = Served::start(
        &dir.0,
        &[("app", &there)],
        &[
            "--allow-client-commands",
            "--label",
            "linux",
            "--unclaimable-after",
            "0",
        ],
    );
    let args = with_agent(&[
        "run",
        "WRITE x.txt=1",
        "--name",
        "on-gpu",
        "--require-label",
        "gpu",
        "--yes",
    ]);
    let running = command(BY, &dir.0)
        .arg("--remote")
        .arg(&server.url)
        .arg("--token-file")
        .arg(&server.token_file)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(fs::File::create(dir.0.join("run.err")).unwrap())
        .spawn()
        .unwrap();
    let mut running = Killed(running);
    let deadline = Instant::now() + Duration::from_secs(60);
    let shown = loop {
        let out = server.by(&dir.0, &["show", "on-gpu"]);
        let shown = text(&out.stdout);
        if shown.contains("waiting") {
            break shown;
        }
        assert!(Instant::now() < deadline, "never said why: {shown}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(shown.contains("not created yet"), "{shown}");
    assert!(shown.contains("requires  gpu"), "{shown}");
    assert!(shown.contains("no live worker carries"), "{shown}");
    let json: Value =
        serde_json::from_slice(&server.by(&dir.0, &["show", "on-gpu", "--json"]).stdout).unwrap();
    assert_eq!(json["operation"]["requires"][0], "gpu");
    assert_eq!(json["operation"]["state"], "queued");
    // The waiting `by run` says so on its standard error.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !fs::read_to_string(dir.0.join("run.err"))
        .unwrap_or_default()
        .contains("is still queued: no live worker carries")
    {
        assert!(
            Instant::now() < deadline,
            "{}",
            fs::read_to_string(dir.0.join("run.err")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(running.0.try_wait().unwrap().is_none());
    drop(running);
    // Locally there are no workers to choose among.
    let local = command(BY, &there)
        .args(with_agent(&["run", "x", "--require-label", "gpu"]))
        .output()
        .unwrap();
    assert_eq!(local.status.code(), Some(1));
    assert!(
        text(&local.stderr).contains("use it with --remote"),
        "{}",
        text(&local.stderr)
    );
}

/// A child killed when dropped.
struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `--priority` reaches the server's queue, and `by stats` summarizes the
/// branches locally (with the turns' outcomes from the event store) and
/// remotely (with the server's queue by priority); locally `--priority` is
/// refused, since there is no queue.
#[test]
fn priority_reaches_the_queue_and_by_stats_summarizes_it() {
    let dir = Dir::new();
    let there = dir.repo("there");
    let server = Served::start(
        &dir.0,
        &[("app", &there)],
        &["--allow-client-commands", "--label", "linux"],
    );
    let done = server.by(
        &dir.0,
        &with_agent(&[
            "run",
            "WRITE a.txt=1",
            "--name",
            "done",
            "--priority",
            "-3",
            "--yes",
        ])
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>(),
    );
    assert!(done.status.success(), "{}", text(&done.stderr));
    // Requires a label no worker carries: it stays queued, at priority 7.
    let args = with_agent(&[
        "run",
        "WRITE x.txt=1",
        "--name",
        "queued",
        "--require-label",
        "gpu",
        "--priority",
        "7",
        "--yes",
    ]);
    let running = command(BY, &dir.0)
        .arg("--remote")
        .arg(&server.url)
        .arg("--token-file")
        .arg(&server.token_file)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let running = Killed(running);
    let deadline = Instant::now() + Duration::from_secs(60);
    let stats = loop {
        let out = server.by(&dir.0, &["stats", "--json"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let stats: Value = serde_json::from_slice(&out.stdout).unwrap();
        if stats["queue"]["queued"]["7"] == 1 {
            break stats;
        }
        assert!(Instant::now() < deadline, "never queued: {stats}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(stats["branches"]["ready"], 1, "{stats}");
    assert_eq!(stats["turns"]["gemini-cli"], 1, "{stats}");
    let shown = text(&server.by(&dir.0, &["stats"]).stdout);
    assert!(
        shown.contains("queue     1 queued (priority 7: 1)"),
        "{shown}"
    );
    drop(running);

    // Locally: no queue, so no priority; the stats read the event store.
    let refused = local(&there, &["run", "x", "--priority", "2"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(text(&refused.stderr).contains("use it with --remote"));
    let out = local(&there, &["run", "WRITE b.txt=1", "--name", "here", "--yes"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    // The server's SQLite state is the repository's own, so the branch
    // it ran counts here too.
    let stats: Value = serde_json::from_slice(&local(&there, &["stats", "--json"]).stdout).unwrap();
    assert_eq!(stats["branches"]["ready"], 2, "{stats}");
    assert_eq!(stats["outcomes"]["completed"], 2, "{stats}");
    assert_eq!(stats["turn_seconds"]["count"], 2, "{stats}");
    assert!(stats.get("queue").is_none(), "{stats}");
    let bad = local(&there, &["run", "x", "--priority", "11"]);
    assert_eq!(bad.status.code(), Some(2), "{}", text(&bad.stderr));
}

#[test]
fn plan_and_knowledge_commands_work_against_a_server() {
    let dir = Dir::new();
    let there = dir.repo("there");
    let server = Served::start(&dir.0, &[("app", &there)], &["--allow-client-commands"]);
    let by = |args: &[&str]| -> Output {
        let args = with_agent(args);
        server.by(&dir.0, &args.iter().map(String::as_str).collect::<Vec<_>>())
    };
    let ok_json = |args: &[&str]| -> Value {
        let out = by(args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    };

    // A planned run waits on the server; its plan is shown and approved
    // with an edit made here, in this machine's editor.
    let out = by(&[
        "run",
        "Mark it PERMISSION WRITE marker.txt=x",
        "--name",
        "planned",
        "--plan",
        "--yes",
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let plan = ok_json(&["plan", "show", "planned", "--json"]);
    assert_eq!(plan["phase"], "awaiting", "{plan}");
    let editor = "sh -c 'printf \"WRITE edited.txt=1\" > \"$0\"'";
    let approved = ok_json(&[
        "plan", "approve", "planned", "--edit", "--editor", editor, "--yes", "--json",
    ]);
    assert_eq!(approved["status"]["state"], "ready", "{approved}");
    let show = ok_json(&["show", "planned", "--json"]);
    assert_eq!(show["plan"]["phase"], "approved", "{show}");

    // Knowledge: added and adopted as the server's caller, then given to
    // the server's next task.
    let added = ok_json(&["knowledge", "add", "Keep commits small.", "--json"]);
    assert_eq!(added["status"], "adopted", "{added}");
    let proposed = ok_json(&[
        "knowledge",
        "add",
        "Notes are dated.",
        "--propose",
        "--json",
    ]);
    let id = proposed["id"].as_u64().unwrap().to_string();
    let list = ok_json(&["knowledge", "list", "--status", "proposed", "--json"]);
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    ok_json(&["knowledge", "adopt", &id, "--json"]);
    let markdown = text(&by(&["knowledge", "export"]).stdout);
    assert!(markdown.contains("Notes are dated."), "{markdown}");
    let out = by(&["run", "SHOW_INSTRUCTIONS", "--name", "told", "--yes"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let log = text(&by(&["log", "told"]).stdout);
    assert!(
        log.contains(&format!("knowledge #{}, #{id}", added["id"])),
        "{log}"
    );
    // A harness distiller runs only where its harness does.
    let refused = by(&["knowledge", "distill", "told", "--harness", "gemini-cli"]);
    assert!(!refused.status.success());
}
