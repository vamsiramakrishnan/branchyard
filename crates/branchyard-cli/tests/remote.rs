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
                    if matches!(key.as_str(), "created_at" | "at_ms") {
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
        &["harnesses"],
    ] {
        same(args);
    }
    for args in [
        &["ls", "--json"][..],
        &["show", "hello", "--json"],
        &["log", "hello", "--json"],
        &["harnesses", "--json"],
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
