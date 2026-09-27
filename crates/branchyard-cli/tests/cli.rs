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
        // Never inherit a delegating harness's identity from whoever runs
        // the tests.
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
fn by_cancel_stops_a_turn_that_another_by_runs() {
    let repo = Repo::new();
    let agent = fake_agent().display().to_string();
    let running = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["run", "HANG", "--name", "held", "--harness", "gemini-cli"])
        .args(["--command", &agent])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let runner = std::thread::spawn(move || running.wait_with_output().unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let log = repo.by(&["log", "held"]);
        if stdout(&log).contains("prompt: HANG") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the turn never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Another process may not send to it while it runs.
    let refused = repo.by_agent(&["send", "held", "WHOAMI"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("branch held is running a turn"),
        "{}",
        stderr(&refused)
    );
    let cancel = repo.by(&["cancel", "held"]);
    assert!(cancel.status.success(), "{}", stderr(&cancel));
    assert_eq!(stdout(&cancel), "asked held to stop\n");
    let ran = runner.join().unwrap();
    assert!(ran.status.success(), "{}", stderr(&ran));
    assert!(stdout(&ran).contains("interrupted"), "{}", stdout(&ran));
    assert_eq!(
        repo.json(&["show", "held", "--json"])["status"]["state"],
        "interrupted"
    );
    let log = stdout(&repo.by(&["log", "held"]));
    assert!(log.contains("warning: cancelled by by cancel"), "{log}");
    assert_eq!(
        stdout(&repo.by(&["cancel", "held"])),
        "nothing was running\n"
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

/// The JSON value a `SH ... --json` line printed in a harness's reply,
/// after the `sh: <status>` line for the `n`th command.
fn sh_json(reply: &str, n: usize) -> (i32, Value) {
    let mut parts = reply.split("sh: ").skip(1 + n);
    let part = parts
        .next()
        .unwrap_or_else(|| panic!("no command {n} in {reply}"));
    let (status, rest) = part.split_once('\n').unwrap();
    let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
    (status.parse().unwrap(), values.next().unwrap().unwrap())
}

/// The harness's reply on `branch`'s last turn.
fn reply(repo: &Repo, branch: &str) -> String {
    repo.json(&["log", branch, "--json"])
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"]["type"] == "message_delta")
        .map(|e| e["event"]["text"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_harness_delegates_with_by_in_its_shell() {
    let repo = Repo::new();
    let prompt = [
        "SH by inspect --json",
        "SH by spawn 'WRITE kid.txt=k' --name kid --json",
        "SH by spawn 'WRITE later.txt=l' --name later --wait --json",
        "SH by children --json",
        "SH by inspect main --json",
        "SH by integrate later --json",
        "SH by send kid 'say more' --json",
        "SH by spawn x --parent other",
    ]
    .join("\n");
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    let (code, me) = sh_json(&said, 0);
    assert_eq!((code, me["name"].as_str()), (0, Some("root")));
    assert_eq!(me["envelope"]["max_depth"], 1);
    let (code, kid) = sh_json(&said, 1);
    assert_eq!((code, kid["name"].as_str()), (0, Some("kid")));
    assert_eq!(kid["status"]["state"], "running");
    let (code, later) = sh_json(&said, 2);
    assert_eq!(
        (code, later["status"]["state"].as_str()),
        (0, Some("ready"))
    );
    let (_, children) = sh_json(&said, 3);
    let names: Vec<&str> = children["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["kid", "later"]);
    let (code, refused) = sh_json(&said, 4);
    assert_eq!(code, 1);
    assert_eq!(refused["error"]["kind"], "denied");
    assert!(refused["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not a descendant"));
    let (code, merged) = sh_json(&said, 5);
    assert_eq!((code, merged["target"].as_str()), (0, Some("by/root")));
    let (code, sent) = sh_json(&said, 6);
    // kid may still be running its first turn; either way the answer says so.
    assert!(
        (code == 0 && sent["name"] == "kid") || sent["error"]["kind"] == "running",
        "{said}"
    );
    assert!(said.contains("inside a harness, the parent is"), "{said}");

    // `by run` waited for the whole subtree, and `by ls` shows the tree.
    let text = stdout(&out);
    assert!(text.contains("delegated"), "{text}");
    let ls = stdout(&repo.by(&["ls"]));
    assert!(ls.contains("└ kid") && ls.contains("└ later"), "{ls}");
    let list = repo.json(&["ls", "--json"]);
    for branch in list.as_array().unwrap() {
        assert_ne!(branch["status"]["state"], "running", "{branch}");
    }
    let root = repo.json(&["show", "root", "--json"]);
    assert_eq!(root["children"], serde_json::json!(["kid", "later"]));
    assert_eq!(repo.json(&["show", "kid", "--json"])["depth"], 1);
}

#[test]
fn a_harness_steers_its_running_children_with_by_and_python() {
    let repo = Repo::new();
    let script = "import branchyard as b; b.spawn('AWAIT_STEER', name='py'); \
                  s = b.steer('py', 'from python'); print('steered', s.by, s.state['state']); \
                  d = b.wait('py', timeout=60, poll=0.05); print('finished', d.status['state']); \
                  exec('try:\\n b.steer(\\'py\\', \\'late\\')\\nexcept b.NotRunningError as e:\\n print(\\'refused\\', e.kind)')";
    let prompt = [
        "SH by spawn AWAIT_STEER --name kid --json".to_owned(),
        "SH by send kid --steer 'check the edge case' --json".to_owned(),
        format!("SH python3 -c \"{script}\""),
    ]
    .join("\n");
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    let (code, steered) = sh_json(&said, 1);
    assert_eq!(code, 0, "{said}");
    assert_eq!(steered["branch"], "kid");
    assert_eq!(steered["by"], "root");
    assert!(
        matches!(
            steered["state"]["state"].as_str(),
            Some("delivered" | "accepted")
        ),
        "{said}"
    );
    for expected in ["steered root", "finished no_changes", "refused not_running"] {
        assert!(said.contains(expected), "{expected:?} missing from\n{said}");
    }
    for (child, text) in [("kid", "check the edge case"), ("py", "from python")] {
        let said = reply(&repo, child);
        assert!(said.contains(&format!("steered: {text}")), "{said}");
    }
}

/// `by send --steer` from another process adds to a turn a separate `by
/// run` process is running, and is refused, with a reason and a failing
/// exit, when no turn runs.
#[test]
fn send_steer_reaches_a_turn_another_process_runs() {
    let repo = Repo::new();
    let agent = fake_agent().display().to_string();
    /// Kills the turn's process if the test fails before it ends.
    struct Running(std::process::Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut running = Running(
        repo.command(env!("CARGO_BIN_EXE_by"))
            .args(["run", "AWAIT_STEER", "--name", "live", "--yes"])
            .args(["--harness", "gemini-cli", "--command", &agent])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let waiting = || {
        let log = repo.by(&["log", "live", "--json"]);
        log.status.success() && stdout(&log).contains("waiting for steering")
    };
    while !waiting() {
        assert!(std::time::Instant::now() < deadline, "live never started");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let limited = repo.by(&["send", "live", "--steer", "x", "--budget-usd", "1"]);
    assert!(!limited.status.success());
    assert!(stderr(&limited).contains("send --steer takes only --json"));
    let out = repo.by(&["send", "live", "--steer", "try the other file"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).starts_with("delivered into live's running turn"),
        "{}",
        stdout(&out)
    );
    assert!(running.0.wait().unwrap().success());
    assert!(reply(&repo, "live").contains("steered: try the other file"));
    let log = stdout(&repo.by(&["log", "live"]));
    assert!(
        log.contains("steered by by send --steer: try the other file"),
        "{log}"
    );

    let late = repo.by(&["send", "live", "--steer", "late", "--json"]);
    assert!(!late.status.success());
    let error: Value = serde_json::from_slice(&late.stdout).unwrap();
    assert_eq!(error["error"]["kind"], "not_running");
}

#[test]
fn a_harness_delegates_with_the_python_module() {
    let repo = Repo::new();
    let script = "import branchyard as b; c = b.spawn('WRITE py.txt=p', name='py'); \
                  print('spawned', c.name, c.status['state']); d = b.wait(c.name, timeout=60, poll=0.05); \
                  print('finished', d.status['state'], d.candidate['files_changed']); \
                  print('merged into', b.integrate(c.name).target); \
                  print('children', [x['name'] for x in b.children().descendants]); \
                  exec('try:\\n b.inspect(\\'main\\')\\nexcept b.DeniedError as e:\\n print(\\'denied\\', e.kind)')";
    let prompt = format!("SH python3 -c \"{script}\"");
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    for expected in [
        "sh: 0",
        "spawned py running",
        "finished ready 1",
        "merged into by/root",
        "children ['py']",
        "denied denied",
    ] {
        assert!(said.contains(expected), "{expected:?} missing from\n{said}");
    }
    let root = repo.json(&["show", "root", "--json"]);
    assert_eq!(root["status"]["state"], "ready");
    assert_eq!(root["candidate"]["files_changed"], 1);
}

#[test]
fn artifacts_and_scratch_reach_the_python_module() {
    let repo = Repo::new();
    let script = "import branchyard as b; \
                  a = b.publish('a.txt', name='a.txt', labels={'k': 'v'}); \
                  print('published', a.name, a.labels); \
                  print('listed', [x.name for x in b.list_artifacts()]); \
                  s = b.create_scratch('cache'); print('scratch', s.name, s.owner_branch); \
                  print('reachable', [x.name for x in b.list_scratch()]); \
                  l = b.lock_scratch('cache'); print('locked', l.holder_branch); \
                  b.unlock_scratch('cache'); print('unlocked')";
    let prompt = format!("SH python3 -c \"{script}\"");
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    for expected in [
        "published a.txt {'k': 'v'}",
        "listed ['a.txt']",
        "scratch cache root",
        "reachable ['cache']",
        "locked root",
        "unlocked",
    ] {
        assert!(said.contains(expected), "{expected:?} missing from\n{said}");
    }
}

#[test]
fn a_child_messages_its_parent_with_the_python_module_and_is_delivered_next_turn() {
    let repo = Repo::new();
    let out = repo.by_agent(&[
        "run",
        "WRITE root.txt=r",
        "--name",
        "root",
        "--delegate=2",
        "--yes",
    ]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));

    // The child's own turn reports, asks and reads its inbox through the
    // Python module, which shells out to `by` exactly as a harness would.
    let script = "import branchyard as b; r = b.report('tests pass'); \
                  print('reported', r.id, r.kind); \
                  a = b.ask('should I rename the module?'); \
                  print('asked', a.message.id, a.answer); \
                  i = b.inbox(); print('inbox', len(i.messages))";
    let kid_prompt = format!("SH python3 -c \"{script}\"");
    let kid = repo.json(&[
        "spawn",
        &kid_prompt,
        "--parent",
        "root",
        "--name",
        "kid",
        "--wait",
        "--yes",
        "--json",
    ]);
    assert_eq!(kid["status"]["state"], "no_changes", "{kid}");
    let said = reply(&repo, "kid");
    assert!(said.contains("reported"), "{said}");
    assert!(said.contains("asked"), "{said}");
    assert!(
        said.contains("inbox 0"),
        "no messages delivered yet: {said}"
    );

    // Both a report and a question reach the parent's inbox, unread, and
    // are recorded on both event logs.
    let root_inbox = repo.json(&["inbox", "--as", "root", "--json"]);
    let messages = root_inbox["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "{root_inbox}");
    assert!(
        messages.iter().all(|m| m["delivered"] == false),
        "{root_inbox}"
    );
    let report_id = messages.iter().find(|m| m["kind"] == "report").unwrap()["id"]
        .as_u64()
        .unwrap();
    let question_id = messages.iter().find(|m| m["kind"] == "question").unwrap()["id"]
        .as_u64()
        .unwrap();
    let kid_log = repo.json(&["log", "kid", "--json"]);
    let kid_log = kid_log.as_array().unwrap();
    assert!(
        kid_log
            .iter()
            .any(|e| e["activity"] == "message" && e["message"]["id"] == report_id),
        "{kid_log:?}"
    );

    // Authority: a branch answers only its own descendants.
    let refused = repo.by(&[
        "answer",
        &question_id.to_string(),
        "no",
        "--as",
        "kid",
        "--json",
    ]);
    assert_eq!(refused.status.code(), Some(1));
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(refused["error"]["kind"], "denied");

    let answer = repo.json(&[
        "answer",
        &question_id.to_string(),
        "yes, rename it",
        "--as",
        "root",
        "--json",
    ]);
    assert_eq!(answer["to"], "kid");
    assert_eq!(answer["in_reply_to"], question_id);

    // Delivered at kid's next turn, prepended to the prompt it actually
    // ran, and acknowledged so it is not delivered twice.
    let sent = repo.json(&["send", "kid", "WHOAMI", "--wait", "--json"]);
    assert_eq!(sent["status"]["state"], "no_changes", "{sent}");
    let kid_log = repo.json(&["log", "kid", "--json"]);
    let prompt = kid_log
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|e| e["activity"] == "prompt")
        .unwrap()["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(prompt.contains("<branchyard-inbox>"), "{prompt}");
    assert!(prompt.contains("yes, rename it"), "{prompt}");
    assert!(prompt.ends_with("WHOAMI"), "{prompt}");

    let kid_inbox = repo.json(&["inbox", "--as", "kid", "--json"]);
    let messages = kid_inbox["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1, "{kid_inbox}");
    assert_eq!(messages[0]["delivered"], true, "{kid_inbox}");
}

#[test]
fn the_same_commands_act_with_your_authority_outside_a_harness() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "say hi", "--name", "root", "--delegate=2", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let plain = repo.by_agent(&["run", "say hi", "--name", "plain", "--yes"]);
    assert!(plain.status.success(), "{}", stderr(&plain));

    // Outside a harness, spawn names its parent and waits for the child.
    let agent = fake_agent().display().to_string();
    let spawned = repo.by(&[
        "spawn",
        "WRITE kid.txt=k",
        "--parent",
        "root",
        "--name",
        "kid",
        "--yes",
        "--json",
    ]);
    assert!(
        spawned.status.success(),
        "{}\n{}",
        stdout(&spawned),
        stderr(&spawned)
    );
    let kid: Value = serde_json::from_slice(&spawned.stdout).unwrap();
    assert_eq!(kid["status"]["state"], "ready");
    assert_eq!(kid["envelope"]["max_depth"], 1);
    assert_eq!(repo.json(&["inspect", "kid", "--json"])["parent"], "root");
    let children = repo.json(&["children", "root", "--json"]);
    assert_eq!(children["descendants"][0]["name"], "kid");
    let events = repo.json(&["events", "kid", "--cursor", "0", "--limit", "2", "--json"]);
    assert_eq!(events["next_cursor"], 2);
    assert_eq!(
        repo.json(&["cancel", "kid", "--json"]),
        serde_json::json!({"cancelled": []})
    );
    let merged = repo.json(&["integrate", "kid", "--json"]);
    assert_eq!(merged["target"], "by/root");
    assert_eq!(repo.git(&["show", "by/root:kid.txt"]), "k\n");

    // The envelope binds you too, and refusals are JSON with --json.
    let refused = repo.by(&["spawn", "x", "--parent", "plain", "--json"]);
    assert_eq!(refused.status.code(), Some(1));
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(refused["error"]["kind"], "denied");
    let text = repo.by(&["integrate", "plain"]);
    assert!(
        stderr(&text).contains("not delegated by another branch"),
        "{}",
        stderr(&text)
    );
    let inspect = stdout(&repo.by(&["inspect", "root"]));
    assert!(
        inspect.contains("children") && inspect.contains("kid"),
        "{inspect}"
    );
    let _ = agent;
}

#[test]
fn a_delegating_harness_gets_tools_and_skill_outside_its_worktree() {
    let repo = Repo::new();
    let prompt = "INSTRUCTED";
    let out = repo.by_agent(&["run", prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(reply(&repo, "root"), "instructed=true");
    let root = repo.json(&["show", "root", "--json"]);
    assert_eq!(root["status"]["state"], "no_changes");
    assert_eq!(root["candidate"], Value::Null);
    let worktree = root["worktree"].as_str().unwrap();
    let status = repo
        .command("git")
        .args([
            "-C",
            worktree,
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored",
        ])
        .output()
        .unwrap();
    assert_eq!(stdout(&status), "", "the worktree is untouched");
    let dir = repo.root.join(".branchyard");
    assert!(dir.join("plugin/skills/delegate/SKILL.md").is_file());
    assert!(dir.join("plugin/.claude-plugin/plugin.json").is_file());
    assert!(dir.join("sdk/python/branchyard.py").is_file());
    assert_eq!(
        repo.git(&["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
    // Without delegation there is no preamble, and `by mcp` is the server
    // when there is.
    let plain = repo.by_agent(&["run", "INSTRUCTED", "--name", "plain", "--yes"]);
    assert!(plain.status.success());
    assert_eq!(reply(&repo, "plain"), "instructed=false");
    let tools = repo.by_agent(&["run", "MCP tools", "--name", "mcp", "--delegate", "--yes"]);
    assert!(tools.status.success(), "{}", stderr(&tools));
    assert!(
        reply(&repo, "mcp").contains(
            "mcp tools: spawn,inspect,events,send,steer,propose_integration,cancel,children"
        ),
        "{}",
        reply(&repo, "mcp")
    );
    // Inside a harness that was not given delegation, `by` will not act
    // with your authority.
    let by = env!("CARGO_BIN_EXE_by");
    let bare = repo.by_agent(&[
        "run",
        &format!("SH {by} spawn x --parent root --json"),
        "--name",
        "bare",
        "--yes",
    ]);
    assert!(bare.status.success());
    let (code, refused) = sh_json(&reply(&repo, "bare"), 0);
    assert_eq!(code, 1);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("was not given delegation"),
        "{refused}"
    );
}

#[test]
fn by_mcp_needs_a_token() {
    let repo = Repo::new();
    let out = repo.by(&["mcp", "--root", "/tmp", "--branch", "b"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("BRANCHYARD_DELEGATION is not set"),
        "{}",
        stderr(&out)
    );
}

/// The example rigs, from the repository root.
fn example(name: &str) -> String {
    format!("{}/../../examples/rigs/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn rig_check_prints_the_plan_and_names_what_it_refuses() {
    let repo = Repo::new();
    let text = repo.by(&["rig", "check", &example("feature.toml")]);
    assert!(text.status.success(), "{}", stderr(&text));
    let text = stdout(&text);
    for expected in [
        "rig feature: ",
        "root   lead as branch feature, claude-code (claude-code-stream-json)",
        "envelope depth 1, 3 children, harnesses claude-code, codex",
        "implementer (under lead), codex (codex-app-server), up to 2 at once",
        "deny Edit, Write, MultiEdit, NotebookEdit",
    ] {
        assert!(text.contains(expected), "{expected:?} missing from\n{text}");
    }
    // --json prints the plan the golden file holds.
    let json = repo.by(&["rig", "check", &example("parser.toml"), "--json"]);
    assert!(json.status.success(), "{}", stderr(&json));
    let golden = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/golden/parser.plan.json"
    ))
    .unwrap();
    assert_eq!(stdout(&json), golden);

    let spec = repo.dir.join("bad.toml");
    fs::write(
        &spec,
        "version = 1\nname = \"bad\"\nroot = \"lead\"\n[seats.lead]\ncollaborates_with = [\"x\"]\n",
    )
    .unwrap();
    let spec = spec.display().to_string();
    let refused = repo.by(&["rig", "check", &spec]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(
        stderr(&refused),
        format!(
            "by: {spec}: line 5: seats.lead.collaborates_with: not supported: Branchyard has no \
             messaging between branches; a branch acts only on its descendants\n"
        )
    );
    let refused = repo.by(&["rig", "run", &spec, "go", "--json"]);
    assert_eq!(refused.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(error["error"]["kind"], "invalid_rig");
    assert_eq!(error["error"]["field"], "seats.lead.collaborates_with");
    assert_eq!(error["error"]["line"], 5);
    assert!(repo.json(&["ls", "--json"]).as_array().unwrap().is_empty());

    for usage in [
        &["rig", "run", &spec][..],
        &["rig", "check", &spec, "extra"],
        &["rig", "start", &spec],
        &["rig", "check", &spec, "--name", "x"],
    ] {
        assert_eq!(repo.by(usage).status.code(), Some(2), "{usage:?}");
    }
    // Profiles that cannot route permissions need consent before anything runs.
    fs::write(
        repo.dir.join("pi.toml"),
        "version = 1\nname = \"pi\"\nroot = \"lead\"\n[seats.lead]\nharness = \"pi\"\n",
    )
    .unwrap();
    let pi = repo.dir.join("pi.toml").display().to_string();
    let check = stdout(&repo.by(&["rig", "check", &pi]));
    assert!(
        check.contains("needs --allow-unapproved-tools: lead"),
        "{check}"
    );
    let run = repo.by(&["rig", "run", &pi, "go"]);
    assert_eq!(run.status.code(), Some(1));
    assert!(
        stderr(&run).contains("--allow-unapproved-tools"),
        "{}",
        stderr(&run)
    );
}

/// A rig of the fake agent: a lead that may spawn two workers and a
/// reviewer.
const TEAM: &str = r#"
version = 1
name = "team"
root = "lead"

[seats.lead]
harness = "gemini-cli"
delegates_to = ["worker", "reviewer"]
policy = { default = "allow", deny = ["WebFetch"] }

[seats.worker]
description = "Writes files."
instances = 2

[seats.reviewer]
description = "Reviews, never edits."
policy = { deny = ["Edit"] }
"#;

#[test]
fn a_rig_runs_its_root_which_fills_seats_with_by_and_python() {
    let repo = Repo::new();
    let spec = repo.dir.join("team.toml");
    fs::write(&spec, TEAM).unwrap();
    let script = "import branchyard as b; c = b.spawn('INSTRUC' + 'TED', seat='reviewer'); \
                  print('seat', c.seat, c.name); d = b.wait(c.name, timeout=60, poll=0.05); \
                  print('reviewed', d.status['state'], d.last_message, d.seat)";
    let prompt = [
        "SH by inspect --json".to_owned(),
        "SH by spawn --seat worker 'WRITE w.txt=w' --wait --json".into(),
        "SH by spawn 'say free' --json".into(),
        format!("SH python3 -c \"{script}\""),
        "SH by integrate team-worker --json".into(),
    ]
    .join("\n");
    let agent = fake_agent().display().to_string();
    let spec = spec.display().to_string();
    let out = repo.by(&["rig", "run", &spec, &prompt, "--command", &agent, "--json"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stderr(&out).contains(
            "by: rig team: root seat lead on gemini-cli; it may spawn seats worker, reviewer"
        ),
        "{}",
        stderr(&out)
    );
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["rig"], "team");
    assert_eq!(result["root"]["name"], "team");
    assert_eq!(
        result["root"]["children"],
        serde_json::json!(["team-worker", "team-reviewer"])
    );
    let descendants: Vec<(&str, &str)> = result["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["name"].as_str().unwrap(),
                d["status"]["state"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        descendants,
        [("team-worker", "merged"), ("team-reviewer", "no_changes")]
    );

    let said = reply(&repo, "team");
    let (_, me) = sh_json(&said, 0);
    assert_eq!(me["seat"], "lead");
    assert_eq!(me["seats"], serde_json::json!(["worker", "reviewer"]));
    assert_eq!(me["envelope"]["max_children"], 3);
    let (code, worker) = sh_json(&said, 1);
    assert_eq!(
        (code, worker["status"]["state"].as_str()),
        (0, Some("ready"))
    );
    assert_eq!(worker["seat"], "worker");
    let (code, free) = sh_json(&said, 2);
    assert_eq!(code, 1);
    assert!(
        free["error"]["message"]
            .as_str()
            .unwrap()
            .contains("spawns only by seat: one of worker, reviewer"),
        "{free}"
    );
    // The reviewer got its seat's instructions and nothing else.
    assert!(said.contains("seat reviewer team-reviewer"), "{said}");
    assert!(
        said.contains("reviewed no_changes instructed=true reviewer"),
        "{said}"
    );
    let (code, merged) = sh_json(&said, 4);
    assert_eq!((code, merged["target"].as_str()), (0, Some("by/team")));
    assert_eq!(repo.git(&["show", "by/team:w.txt"]), "w\n");
    // Nothing was written into a worktree but the harnesses' own work.
    let status = repo.git(&[
        "-C",
        ".branchyard/worktrees/team-reviewer",
        "status",
        "--short",
    ]);
    assert_eq!(status, "");

    let inspected = stdout(&repo.by(&["inspect", "team"]));
    assert!(
        inspected.contains("lead; spawns seats: worker, reviewer"),
        "{inspected}"
    );

    // Outside a harness, a person fills a seat the same way.
    let second = repo.json(&[
        "spawn",
        "WRITE x.txt=x",
        "--parent",
        "team",
        "--seat",
        "worker",
        "--yes",
        "--json",
    ]);
    assert_eq!(second["name"], "team-worker-2");
    assert_eq!(second["seat"], "worker");
    let third = repo.by(&[
        "spawn", "x", "--parent", "team", "--seat", "worker", "--yes", "--json",
    ]);
    assert_eq!(third.status.code(), Some(1));
    let third: Value = serde_json::from_slice(&third.stdout).unwrap();
    // The envelope still bounds everything: three seats' worth of children.
    assert!(
        third["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already has 3 children, its envelope's max_children"),
        "{third}"
    );
}

#[test]
fn artifact_and_scratch_commands_follow_the_delegation_tree() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "WRITE payload.txt=hi", "--name", "root"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let out = repo.by_agent(&["run", "no changes", "--name", "sibling"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let published = repo.json(&[
        "artifact",
        "publish",
        ".branchyard/worktrees/root/payload.txt",
        "--branch",
        "root",
        "--json",
    ]);
    let id = published["id"].as_str().unwrap().to_owned();
    assert_eq!(published["name"], "payload.txt");
    assert!(!published["digest"].as_str().unwrap().is_empty());

    let listed = repo.json(&["artifact", "list", "--branch", "root", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let listed_sibling = repo.json(&["artifact", "list", "--branch", "sibling", "--json"]);
    assert_eq!(listed_sibling.as_array().unwrap().len(), 0);

    let out = repo.by(&[
        "artifact", "share", &id, "--to", "sibling", "--branch", "root",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let listed_sibling = repo.json(&["artifact", "list", "--branch", "sibling", "--json"]);
    assert_eq!(listed_sibling.as_array().unwrap().len(), 1);

    let out_path = repo.dir.join("out.bin");
    let out = repo.by(&[
        "artifact",
        "get",
        &id,
        "--out",
        out_path.to_str().unwrap(),
        "--branch",
        "sibling",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(&out_path).unwrap(), "hi\n");

    let area = repo.json(&["scratch", "create", "cache", "--branch", "root", "--json"]);
    assert_eq!(area["name"], "cache");
    let lock = repo.json(&["scratch", "lock", "cache", "--branch", "root", "--json"]);
    assert_eq!(lock["holder_branch"], "root");
    let denied = repo.by(&["scratch", "lock", "cache", "--branch", "sibling"]);
    assert_eq!(denied.status.code(), Some(1));
    assert!(stderr(&denied).contains("may not"), "{}", stderr(&denied));
    let out = repo.by(&["scratch", "unlock", "cache", "--branch", "root"]);
    assert!(out.status.success(), "{}", stderr(&out));
}
