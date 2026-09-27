//! The built `by` binary end to end, against a temporary repository and the
//! fake ACP agent from branchyard-runtime. Hermetic: no real harness, no
//! network. Requires `git` and `sh`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Built once per test binary; cargo exposes a binary's path only to its
/// own package's tests.
fn fake_agent() -> &'static Path {
    static AGENT: OnceLock<PathBuf> = OnceLock::new();
    AGENT.get_or_init(|| {
        let by = PathBuf::from(env!("CARGO_BIN_EXE_by"));
        // target/<profile>/by
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

struct Repo {
    dir: PathBuf,
    root: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let repo = Repo {
            root: dir.join("repo"),
            dir,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@localhost"]);
        fs::write(repo.root.join("a.txt"), "one\n").unwrap();
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
            .env("PAGER", "cat");
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

    /// `by <args>` with the fake agent as the gemini-cli harness.
    fn by_agent(&self, args: &[&str]) -> Output {
        let agent = fake_agent().display().to_string();
        let mut all: Vec<&str> = args.to_vec();
        all.extend(["--command", &agent]);
        if args[0] != "send" && !args.contains(&"--harness") {
            all.extend(["--harness", "gemini-cli"]);
        }
        self.by(&all)
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.by(args);
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn run_ls_diff_log_merge_and_rm() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "WRITE hello.txt=hi", "--name", "hello"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("no isolation"), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("wrote hello.txt"), "{text}");
    assert!(
        text.contains("by/hello") && text.contains("ready"),
        "{text}"
    );

    let list = repo.json(&["ls", "--json"]);
    let branches = list.as_array().unwrap();
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0]["name"], "hello");
    assert_eq!(branches[0]["status"]["state"], "ready");
    assert_eq!(branches[0]["candidate"]["files_changed"], 1);
    assert_eq!(branches[0]["profile"], "gemini-cli-acp");

    let diff = repo.by(&["diff", "hello"]);
    assert!(stdout(&diff).contains("+hi"), "{}", stdout(&diff));

    let log = stdout(&repo.by(&["log", "hello"]));
    for expected in [
        "status: running",
        "harness ready",
        "prompt: WRITE hello.txt=hi",
        "wrote hello.txt",
        "candidate ",
        "session closed",
        "status: ready",
    ] {
        assert!(log.contains(expected), "{expected:?} missing from\n{log}");
    }
    let events = repo.json(&["log", "hello", "--json"]);
    let events = events.as_array().unwrap();
    assert_eq!(events[0]["activity"], "status");
    assert!(events.iter().any(|e| e["event"]["type"] == "turn_ended"));

    let merged = repo.by(&["merge", "hello"]);
    assert!(merged.status.success(), "{}", stderr(&merged));
    assert!(
        stdout(&merged).starts_with("merged hello into main"),
        "{}",
        stdout(&merged)
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("hello.txt")).unwrap(),
        "hi\n"
    );
    assert_eq!(
        repo.json(&["show", "hello", "--json"])["status"]["state"],
        "merged"
    );

    let removed = repo.by(&["rm", "hello"]);
    assert_eq!(stdout(&removed), "removed hello\n");
    assert_eq!(repo.json(&["ls", "--json"]), Value::Array(Vec::new()));
    assert_eq!(
        repo.git(&["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
}

#[test]
fn without_a_terminal_permissions_are_denied_and_shown() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "PERMISSION WRITE p.txt=1", "--name", "p"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("will be denied"), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("denied write marker: Denied by Branchyard policy."),
        "{text}"
    );
    let log = stdout(&repo.by(&["log", "p"]));
    assert!(log.contains("denied write marker"), "{log}");

    let yes = repo.by_agent(&["run", "PERMISSION WRITE p.txt=1", "--name", "q", "--yes"]);
    assert!(
        stdout(&yes).contains("allowed write marker"),
        "{}",
        stdout(&yes)
    );
    let events = repo.json(&["log", "q", "--json"]);
    let decision = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["activity"] == "decision")
        .unwrap()
        .clone();
    assert_eq!(decision["allowed"], true);
    assert_eq!(decision["source"]["kind"], "default");
}

#[test]
fn fan_prefixes_branches_and_compares_them() {
    let repo = Repo::new();
    let out = repo.by_agent(&[
        "fan",
        "WRITE f.txt=x",
        "--harness",
        "gemini-cli,qwen-code",
        "--yes",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("write-f-txt-x-gemini-cli │ wrote f.txt"),
        "{text}"
    );
    assert!(
        text.contains("write-f-txt-x-qwen-code  │ wrote f.txt"),
        "{text}"
    );
    assert!(text.contains("by diff write-f-txt-x-qwen-code"), "{text}");
    assert_eq!(repo.json(&["ls", "--json"]).as_array().unwrap().len(), 2);
}

#[test]
fn send_continues_and_max_minutes_interrupts() {
    let repo = Repo::new();
    assert!(repo
        .by_agent(&["run", "WHOAMI", "--name", "s"])
        .status
        .success());
    let sent = repo.by_agent(&["send", "s", "WHOAMI"]);
    assert!(stdout(&sent).contains("resumed=true"), "{}", stdout(&sent));

    let hung = repo.by_agent(&["run", "HANG", "--name", "h", "--max-minutes", "0.005"]);
    assert!(hung.status.success(), "{}", stderr(&hung));
    assert!(
        stdout(&hung).contains("over budget: max_duration"),
        "{}",
        stdout(&hung)
    );
}

#[test]
fn errors_exit_nonzero() {
    let repo = Repo::new();
    let missing = repo.by(&["merge", "nope"]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(stderr(&missing), "by: no branch named nope\n");

    let usage = repo.by(&["run"]);
    assert_eq!(usage.status.code(), Some(2));

    let unknown = repo.by(&["run", "x", "--harness", "nope"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(stderr(&unknown).contains("no harness or profile named nope"));

    let absent = repo.by(&[
        "run",
        "x",
        "--harness",
        "gemini-cli",
        "--command",
        "/nonexistent/agent",
    ]);
    assert_eq!(absent.status.code(), Some(1));
    assert!(
        stderr(&absent).contains("unavailable"),
        "{}",
        stderr(&absent)
    );

    let failed = repo.by_agent(&["run", "EXIT", "--name", "exits"]);
    assert_eq!(failed.status.code(), Some(1), "{}", stdout(&failed));
    assert!(stdout(&failed).contains("failed"), "{}", stdout(&failed));

    let outside = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["ls"])
        .current_dir(&repo.dir)
        .output()
        .unwrap();
    assert_eq!(outside.status.code(), Some(1));
    assert!(stderr(&outside).contains("not inside a git work tree"));
}

#[test]
fn watch_prints_the_tree_once_or_logs_changes_until_q() {
    let repo = Repo::new();
    let empty = repo.by(&["watch", "--once"]);
    assert!(empty.status.success(), "{}", stderr(&empty));
    assert!(
        stdout(&empty).contains("no branches yet"),
        "{}",
        stdout(&empty)
    );

    assert!(repo
        .by_agent(&["run", "WRITE w.txt=1", "--name", "w", "--yes"])
        .status
        .success());
    let agent = fake_agent().display().to_string();
    let fork = repo.by(&[
        "fork",
        "w",
        "WRITE v.txt=2",
        "--name",
        "w-alt",
        "--fresh-session",
        "--yes",
        "--command",
        &agent,
    ]);
    assert!(fork.status.success(), "{}", stderr(&fork));
    let once = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["watch", "--once"])
        .env("COLUMNS", "100")
        .output()
        .unwrap();
    let frame = stdout(&once);
    assert!(frame.starts_with("by watch · "), "{frame}");
    assert!(
        frame.contains("\nw        gemini-cli  ready"),
        "the root row:\n{frame}"
    );
    assert!(
        frame.contains("\n└ w-alt  gemini-cli  ready"),
        "the fork indented under it:\n{frame}"
    );
    assert!(frame.contains("wrote v.txt"), "{frame}");
    assert!(frame.lines().all(|l| l.chars().count() <= 100), "{frame}");

    // Without a terminal: one line per change, until `q` on stdin.
    use std::io::{BufRead, BufReader, Write};
    let mut child = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["watch", "--interval", "0.1"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    assert!(first.contains("  w  ready  turns 1"), "{first}");
    let second = lines.next().unwrap().unwrap();
    assert!(second.contains("  w-alt  ready"), "{second}");
    child.stdin.take().unwrap().write_all(b"q\n").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("by watch did not exit on q");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
