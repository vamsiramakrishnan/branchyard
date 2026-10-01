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
            // Never a person's own ~/.config/branchyard/config.toml.
            .env(
                "BRANCHYARD_USER_CONFIG",
                "/nonexistent/branchyard-config.toml",
            )
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
        .env(
            "BRANCHYARD_USER_CONFIG",
            "/nonexistent/branchyard-config.toml",
        )
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

/// `by watch` on a pseudo-terminal (util-linux `script`) as a cockpit:
/// `m` then `y` on the selected ready branch runs `by merge`, whose result
/// the dashboard shows, then `q` quits and restores the terminal.
#[cfg(target_os = "linux")]
#[test]
fn the_watch_cockpit_merges_the_selected_branch_on_m_then_y() {
    use std::io::Write;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    if !Path::new("/usr/bin/script").exists() {
        eprintln!("skipped: no /usr/bin/script for a pseudo-terminal");
        return;
    }
    let repo = Repo::new();
    let run = repo.by_agent(&[
        "run",
        "WRITE w.txt=1",
        "--name",
        "w",
        "--yes",
        "--check",
        "test -f w.txt",
    ]);
    assert!(run.status.success(), "{}", stderr(&run));
    let by = env!("CARGO_BIN_EXE_by");
    let mut watch = repo.command("/usr/bin/script");
    watch
        .args([
            "-qfc",
            &format!("stty cols 120 rows 30; {by} watch --interval 0.2"),
            "/dev/null",
        ])
        .env("TERM", "xterm")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = watch.spawn().unwrap();
    let mut keys = process.stdin.take().unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    // A follow-up first: `s`, a line of text, Enter; it runs in the
    // background while the dashboard carries on.
    keys.write_all(b"sWRITE x.txt=2\r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let shown = repo.json(&["show", "w", "--json"]);
        if shown["turns"] == 2 && shown["status"]["state"] == "ready" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the send did not finish: {shown}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
    keys.write_all(b"m").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    keys.write_all(b"y").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !fs::read_to_string(repo.root.join("x.txt")).is_ok_and(|t| t == "2\n") {
        assert!(Instant::now() < deadline, "the merge did not happen");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Let the result reach the screen, then quit.
    std::thread::sleep(Duration::from_millis(1000));
    keys.write_all(b"qq").unwrap();
    drop(keys);
    let out = process.wait_with_output().unwrap();
    let screen = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{screen}");
    // ratatui redraws only changed cells, so look for whole runs only.
    for expected in [
        "running: by send w 'WRITE x.txt=2'",
        "the background; output in",
        "┌ merge w ─",
        "candidate ",
        "merged w into main (",
        "◆ merged into main",
    ] {
        assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
    }
}

/// A waiting `by run` on a terminal says when its branch ends: a bell and
/// OSC 9 by default, OSC 777 when `branchyard.toml` asks, nothing with
/// `--no-notify`; and never on a pipe.
#[cfg(target_os = "linux")]
#[test]
fn a_waiting_run_notifies_on_its_terminal_when_the_branch_ends() {
    if !Path::new("/usr/bin/script").exists() {
        eprintln!("skipped: no /usr/bin/script for a pseudo-terminal");
        return;
    }
    let repo = Repo::new();
    let agent = fake_agent().display().to_string();
    let by = env!("CARGO_BIN_EXE_by");
    let on_terminal = |name: &str, extra: &str| {
        let line = format!(
            "{by} run 'WRITE {name}.txt=1' --name {name} --yes --harness gemini-cli \
             --command {agent} {extra}"
        );
        let out = repo
            .command("/usr/bin/script")
            .args(["-qfec", &line, "/dev/null"])
            .env("TERM", "xterm")
            .env_remove("TERM_PROGRAM")
            .env_remove("VTE_VERSION")
            .env_remove("TMUX")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let screen = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{screen}");
        screen
    };
    let screen = on_terminal("a", "");
    assert!(
        screen.contains("\x07\x1b]9;Branchyard: a is ready to merge\x07"),
        "{screen:?}"
    );
    assert!(!on_terminal("b", "--no-notify").contains("\x1b]9;"));
    fs::write(
        repo.root.join("branchyard.toml"),
        "[notify]\nterminal = \"osc777\"\n",
    )
    .unwrap();
    assert!(repo.by(&["config", "validate"]).status.success());
    let screen = on_terminal("c", "");
    assert!(
        screen.contains("\x1b]777;notify;Branchyard;c is ready to merge\x07"),
        "{screen:?}"
    );
    fs::write(
        repo.root.join("branchyard.toml"),
        "[notify]\nenabled = false\n",
    )
    .unwrap();
    assert!(!on_terminal("d", "").contains("Branchyard"));
    // Piped: no escapes in the output at all.
    let piped = repo.by_agent(&["run", "WRITE e.txt=1", "--name", "e", "--yes"]);
    assert!(!stdout(&piped).contains('\x07') && !stderr(&piped).contains('\x07'));
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

/// A harness builds a graph of its children with `by graph` in its shell
/// and with the Python module; both reach the same operation. A child whose
/// prerequisite is still running is created `waiting` (`b`, after `a`, in
/// the same proposal); one whose prerequisite has already settled starts at
/// once (`c`, applied after the script has waited for `b`). The script waits
/// for `b` first so that `c`'s state does not depend on how fast `a` and `b`
/// ran: without the wait, `c` was `waiting` or `running` by timing alone.
#[test]
fn a_harness_applies_a_graph_with_by_and_python() {
    let repo = Repo::new();
    let edits = r#"[{"kind":"spawn","prompt":"WRITE a.txt=1","name":"a"},{"kind":"spawn","prompt":"WRITE b.txt=1","name":"b","depends_on":["a"]}]"#;
    let script = "import branchyard as b; g = b.graph(); print('python rev', g.revision); \
                  b.wait('b', timeout=60, poll=0.05); \
                  a = b.apply_graph([{'kind': 'spawn', 'prompt': 'WRITE c.txt=1', 'name': 'c', \
                  'depends_on': ['b']}], g.revision); \
                  print('python applied', a.revision, a.spawned[0].status['state'], a.spawned[0].depends_on); \
                  d = b.wait('c', timeout=60, poll=0.05); \
                  print('python waited', d.status['state'], d.depends_on[0]['prerequisite']); \
                  exec('try:\\n b.apply_graph([{\\'kind\\': \\'add_dependency\\', \\'dependent\\': \\'c\\', \\'prerequisite\\': \\'a\\'}], 0)\\nexcept b.StaleRevisionError as e:\\n print(\\'python stale\\', e.kind)')";
    let prompt = [
        "SH by graph show --json".to_owned(),
        format!("SH by graph apply --edits '{edits}' --expected-revision 0 --json"),
        format!("SH python3 -c \"{script}\""),
        "SH by graph show --json".to_owned(),
        "SH by graph apply --edits '[]' --expected-revision 0 --json".to_owned(),
    ]
    .join("\n");
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    for expected in [
        "\"revision\": 0",
        "\"state\": \"waiting\"",
        "python rev 1",
        "python applied 2 running ['b']",
        "python waited ready b",
        "python stale stale_revision",
        "\"kind\": \"denied\"",
    ] {
        assert!(said.contains(expected), "{expected:?} missing from\n{said}");
    }
    let graph = repo.json(&["graph", "show", "root", "--json"]);
    assert_eq!(graph["revision"], 2);
    assert_eq!(graph["dependencies"].as_array().unwrap().len(), 2);
    for child in graph["children"].as_array().unwrap() {
        assert_eq!(child["status"]["state"], "ready", "{graph}");
    }
    let text = stdout(&repo.by(&["graph", "show", "root"]));
    assert!(text.contains("root's graph, revision 2"), "{text}");
    assert!(text.contains("after a"), "{text}");
}

/// A person applies a graph from a file outside a harness: the command
/// waits for the children and what they start; a spawn with --depends-on
/// waits for its sibling; a failed prerequisite blocks.
#[test]
fn a_person_applies_a_graph_and_spawns_dependents() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "say hi", "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let proposal = repo.dir.join("proposal.json");
    fs::write(
        &proposal,
        r#"{"expected_revision": 0, "edits": [
            {"kind": "spawn", "prompt": "EXIT", "name": "bad"},
            {"kind": "spawn", "prompt": "WRITE n.txt=1", "name": "next", "depends_on": ["bad"]}
        ]}"#,
    )
    .unwrap();
    let applied = repo.json(&[
        "graph",
        "apply",
        proposal.to_str().unwrap(),
        "--parent",
        "root",
        "--yes",
        "--json",
    ]);
    assert_eq!(applied["revision"], 1);
    assert_eq!(applied["spawned"][1]["status"]["state"], "waiting");
    let next = repo.json(&["inspect", "next", "--json"]);
    assert_eq!(next["status"]["state"], "blocked", "{next}");
    assert!(next["status"]["reason"]
        .as_str()
        .unwrap()
        .contains("its prerequisite bad failed"));
    // The same file again is stale now, and changes nothing.
    let stale = repo.by(&[
        "graph",
        "apply",
        proposal.to_str().unwrap(),
        "--parent",
        "root",
        "--json",
    ]);
    assert_eq!(stale.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&stale.stdout).unwrap();
    assert_eq!(error["error"]["kind"], "stale_revision");
    assert_eq!(
        repo.json(&["graph", "show", "root", "--json"])["revision"],
        1
    );
    // by spawn --depends-on: created waiting, started when its sibling
    // settles, before the command returns.
    let spawned = repo.json(&[
        "spawn",
        "WRITE s.txt=1",
        "--parent",
        "root",
        "--name",
        "after-next",
        "--depends-on",
        "next",
        "--yes",
        "--json",
    ]);
    assert_eq!(spawned["status"]["state"], "blocked", "{spawned}");
    let spawned = repo.json(&[
        "spawn",
        "WRITE t.txt=1",
        "--parent",
        "root",
        "--name",
        "later",
        "--depends-on",
        "after-next",
        "--after",
        "integrated",
        "--json",
    ]);
    assert_eq!(spawned["status"]["state"], "blocked", "{spawned}");
    let usage = repo.by(&["graph", "apply", "--parent", "root"]);
    assert_eq!(usage.status.code(), Some(2));
    let usage = repo.by(&["spawn", "x", "--parent", "root", "--after", "soon"]);
    assert_eq!(usage.status.code(), Some(2));
}

#[test]
fn help_version_typos_and_exit_codes() {
    let repo = Repo::new();
    let help = repo.by(&["--help"]);
    assert!(help.status.success());
    let text = stdout(&help);
    for heading in [
        "Work on branches:",
        "Inspect:",
        "Servers:",
        "Options:",
        "Examples:",
    ] {
        assert!(text.contains(heading), "{heading}: {text}");
    }
    assert_eq!(stdout(&repo.by(&[])), text, "no command prints the help");
    assert_eq!(stdout(&repo.by(&["help"])), text);
    assert_eq!(
        stdout(&repo.by(&["-V"])),
        format!("by {}\n", env!("CARGO_PKG_VERSION"))
    );
    let run = repo.by(&["help", "run"]);
    assert!(run.status.success());
    assert!(stdout(&run).contains("Usage: by run [OPTIONS] [PROMPT]"));
    assert_eq!(stdout(&repo.by(&["run", "--help"])), stdout(&run));
    let nested = repo.by(&["help", "graph", "apply"]);
    assert!(
        stdout(&nested).contains("Usage: by graph apply"),
        "{}",
        stdout(&nested)
    );

    let typo = repo.by(&["mrege", "b"]);
    assert_eq!(typo.status.code(), Some(2));
    assert!(stdout(&typo).is_empty());
    assert!(
        // `review`, `recipe` and `remote` are close to `mrege` too, so clap lists them.
        stderr(&typo)
            .contains("tip: some similar subcommands exist: 'review', 'recipe', 'remote', 'merge'"),
        "{}",
        stderr(&typo)
    );
    let flag = repo.by(&["run", "go", "--budget", "1"]);
    assert_eq!(flag.status.code(), Some(2));
    assert!(
        stderr(&flag).contains("'--budget-usd'"),
        "{}",
        stderr(&flag)
    );
    let checked = repo.by(&["run", "go", "--image", "alpine"]);
    assert_eq!(checked.status.code(), Some(2));
    assert!(stderr(&checked).contains("error: --image needs --provider microsandbox"));
    assert!(stderr(&checked).contains("Usage: by run"));
}

#[test]
fn completions_and_the_man_page_print() {
    let repo = Repo::new();
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let out = repo.by(&["completions", shell]);
        assert!(out.status.success(), "{shell}: {}", stderr(&out));
        let script = stdout(&out);
        assert!(
            script.contains("spawn") && script.contains("budget-usd"),
            "{shell}"
        );
    }
    assert_eq!(repo.by(&["completions", "tcsh"]).status.code(), Some(2));
    let man = repo.by(&["man"]);
    assert!(man.status.success());
    assert!(stdout(&man).starts_with(".ie"), "{}", &stdout(&man)[..80]);
    assert!(stdout(&man).contains(".TH by"));
}

#[test]
fn global_options_come_from_anywhere_and_blank_variables_are_unset() {
    let repo = Repo::new();
    // Before or after the command, the same check: remote options need a
    // server.
    for args in [&["--repo", "app", "ls"][..], &["ls", "--repo", "app"]] {
        let out = repo.by(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(stderr(&out).contains("apply to remote mode"), "{args:?}");
    }
    let from_env = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .arg("ls")
        .env("BRANCHYARD_REMOTE", "http://127.0.0.1:9")
        .env_remove("BRANCHYARD_TOKEN_FILE")
        .output()
        .unwrap();
    assert_eq!(from_env.status.code(), Some(1));
    assert!(
        stderr(&from_env).contains("remote mode needs a token"),
        "{}",
        stderr(&from_env)
    );
    let blank = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["ls", "--json"])
        .env("BRANCHYARD_REMOTE", " ")
        .env("BRANCHYARD_REPO", "")
        .output()
        .unwrap();
    assert!(blank.status.success(), "{}", stderr(&blank));
    assert_eq!(stdout(&blank).trim(), "[]");
    let empty_flag = repo.by(&["--remote=", "ls"]);
    assert_eq!(empty_flag.status.code(), Some(2));
}

#[test]
fn serve_and_worker_hand_their_arguments_to_the_server() {
    let repo = Repo::new();
    for (args, usage) in [
        (&["help", "serve"][..], "Usage: by serve [OPTIONS]"),
        (&["serve", "--help"], "Usage: by serve [OPTIONS]"),
        (&["worker", "--help"], "Usage: by worker [OPTIONS]"),
        (
            &["--repo", "x", "help", "worker"],
            "Usage: by worker [OPTIONS]",
        ),
    ] {
        let out = repo.by(args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        assert!(stdout(&out).contains(usage), "{args:?}: {}", stdout(&out));
        assert!(stdout(&out).contains("--webhook-events <KINDS>"));
    }
    // `--repo` after `serve` is the server's own NAME=PATH, not by's.
    let bad = repo.by(&["serve", "--repo", "nopath"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(stderr(&bad).contains("needs NAME=PATH"), "{}", stderr(&bad));
    let token = repo.by(&[
        "serve", "token", "new", "--tenant", "acme", "--scopes", "read",
    ]);
    assert!(token.status.success(), "{}", stderr(&token));
    let credential: Value = serde_json::from_slice(&token.stdout).unwrap();
    assert_eq!(credential["tenant"], "acme");
    assert_eq!(credential["scopes"], serde_json::json!(["read"]));
    let remote = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["serve", "--listen", "127.0.0.1:0"])
        .env("BRANCHYARD_REMOTE", "http://127.0.0.1:9")
        .output()
        .unwrap();
    assert_eq!(remote.status.code(), Some(2));
    assert!(stderr(&remote).contains("does not take --remote"));
}

#[test]
fn checkpoints_show_and_log_then_rewind_and_fork_at() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "WRITE r.txt=1", "--name", "cp", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = repo.by_agent(&["send", "cp", "WRITE r.txt=2", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let show = stdout(&repo.by(&["show", "cp"]));
    assert!(show.contains("checkpoints"), "{show}");
    assert!(show.contains("  0  "), "{show}");
    assert!(
        show.contains("  1  ") && show.contains("WRITE r.txt=1"),
        "{show}"
    );
    assert!(show.contains("* 2  "), "{show}");
    let json = repo.json(&["show", "cp", "--json"]);
    assert_eq!(json["checkpoints"]["current"], 2);
    let list = json["checkpoints"]["checkpoints"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert!(list[0]["git_ref"]
        .as_str()
        .unwrap()
        .starts_with("refs/branchyard/cp/"));
    let log = stdout(&repo.by(&["log", "cp"]));
    assert!(log.contains("checkpoint 1 at "), "{log}");
    let events = repo.json(&["log", "cp", "--json"]);
    assert!(events
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["activity"] == "checkpoint" && e["checkpoint"]["turn"] == 2));

    // Without a terminal a rewind is not confirmed.
    let refused = repo.by(&["rewind", "cp", "--to", "1"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("pass --yes"),
        "{}",
        stderr(&refused)
    );
    let worktree = repo.root.join(".branchyard/worktrees/cp");
    assert_eq!(fs::read_to_string(worktree.join("r.txt")).unwrap(), "2\n");

    let rewound = repo.by(&["rewind", "cp", "--to", "1", "--yes"]);
    assert!(rewound.status.success(), "{}", stderr(&rewound));
    let text = stdout(&rewound);
    assert!(
        text.starts_with("rewound cp from 2 to checkpoint 1"),
        "{text}"
    );
    assert!(
        text.contains("fresh session with a summary of turn 1"),
        "{text}"
    );
    assert_eq!(fs::read_to_string(worktree.join("r.txt")).unwrap(), "1\n");
    let forward = repo.json(&["rewind", "cp", "--to", "2", "--yes", "--json"]);
    assert_eq!(forward["to"], 2);
    assert_eq!(forward["session"]["mode"], "native");
    assert!(stdout(&repo.by(&["log", "cp"])).contains("rewound from 1 to checkpoint 2"));

    let agent = fake_agent().display().to_string();
    let forked = repo.by(&[
        "fork",
        "cp",
        "WHOAMI",
        "--at",
        "1",
        "--name",
        "f1",
        "--yes",
        "--command",
        &agent,
    ]);
    assert!(forked.status.success(), "{}", stderr(&forked));
    assert!(
        stderr(&forked).contains("forked from cp at checkpoint 1; f1 starts a fresh session"),
        "{}",
        stderr(&forked)
    );
    assert!(
        stdout(&forked).contains("resumed=false"),
        "{}",
        stdout(&forked)
    );
    assert_eq!(
        fs::read_to_string(repo.root.join(".branchyard/worktrees/f1/r.txt")).unwrap(),
        "1\n"
    );
    let at_and_fresh = repo.by(&["fork", "cp", "x", "--at", "1", "--fresh-session"]);
    assert_eq!(at_and_fresh.status.code(), Some(2));
    let missing = repo.by(&["rewind", "cp", "--to", "7", "--yes"]);
    assert!(
        stderr(&missing).contains("no checkpoint 7"),
        "{}",
        stderr(&missing)
    );
}

#[test]
fn compare_try_and_pick_after_a_fan() {
    let repo = Repo::new();
    let out = repo.by_agent(&[
        "fan",
        "WRITE f.txt=x",
        "--harness",
        "gemini-cli,qwen-code",
        "--yes",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let (a, b) = ("write-f-txt-x-gemini-cli", "write-f-txt-x-qwen-code");
    let out = repo.by_agent(&["send", b, "WRITE g.txt=y", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let table = stdout(&repo.by(&["compare", "--fan", "write-f-txt-x"]));
    let lines: Vec<&str> = table.lines().collect();
    assert!(
        lines[0].starts_with("BRANCH") && lines[0].contains("UNIQUE FILES"),
        "{table}"
    );
    assert!(
        lines[1].starts_with(a) && lines[1].contains(" none "),
        "{table}"
    );
    assert!(
        lines[2].starts_with(b) && lines[2].contains("g.txt"),
        "{table}"
    );
    let json = repo.json(&["compare", a, b, "--json"]);
    let attempts = json.as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1]["turns"], 2);
    assert_eq!(attempts[1]["unique_files"], serde_json::json!(["g.txt"]));
    assert_eq!(attempts[0]["unique_files"], serde_json::json!([]));
    let diff = stdout(&repo.by(&["compare", "--diff", a, b]));
    assert!(diff.contains("+++ b/g.txt"), "{diff}");

    // Try one, swap to the other, and restore.
    let tried = repo.by(&["try", a]);
    assert!(tried.status.success(), "{}", stderr(&tried));
    assert!(
        stdout(&tried).contains("trying write-f-txt-x-gemini-cli"),
        "{}",
        stdout(&tried)
    );
    assert_eq!(fs::read_to_string(repo.root.join("f.txt")).unwrap(), "x\n");
    let swapped = repo.by(&["try", b]);
    assert!(stdout(&swapped).contains(&format!("turned off the try of {a}")));
    assert!(repo.root.join("g.txt").exists());
    let status = repo.json(&["try", "--status", "--json"]);
    assert_eq!(status["branch"], b);
    let off = repo.by(&["try", "--off"]);
    assert!(off.status.success(), "{}", stderr(&off));
    assert!(!repo.root.join("f.txt").exists() && !repo.root.join("g.txt").exists());
    assert_eq!(
        repo.git(&["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
    fs::write(repo.root.join("dirty.txt"), "mine\n").unwrap();
    let dirty = repo.by(&["try", a]);
    assert!(
        stderr(&dirty).contains("uncommitted changes"),
        "{}",
        stderr(&dirty)
    );
    fs::remove_file(repo.root.join("dirty.txt")).unwrap();

    let picked = repo.by(&[
        "compare",
        "--fan",
        "write-f-txt-x",
        "--pick",
        b,
        "--discard-others",
        "--yes",
    ]);
    assert!(picked.status.success(), "{}", stderr(&picked));
    let text = stdout(&picked);
    assert!(text.contains(&format!("merged {b} into main")), "{text}");
    assert!(text.contains(&format!("removed {a}")), "{text}");
    assert_eq!(fs::read_to_string(repo.root.join("g.txt")).unwrap(), "y\n");
    let left = repo.json(&["ls", "--json"]);
    assert_eq!(left.as_array().unwrap().len(), 1);
    assert_eq!(left[0]["name"], b);
}
