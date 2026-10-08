//! The built `by` binary end to end, against a temporary repository and the
//! fake ACP agent from branchyard-runtime. Hermetic: no real harness, no
//! network. Requires `git` and `sh`.

#![allow(clippy::let_underscore_must_use, clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use branchyard_testkit::{fake_agent, wait};
use serde_json::Value;

/// The kit's repository, plus what this file adds.
struct Repo(branchyard_testkit::Repo);

impl std::ops::Deref for Repo {
    type Target = branchyard_testkit::Repo;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Repo {
    fn new() -> Repo {
        Repo(branchyard_testkit::repo!())
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
    let agent = fake_agent!().display().to_string();
    let running = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["run", "HANG", "--name", "held", "--harness", "gemini-cli"])
        .args(["--command", &agent])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let runner = std::thread::spawn(move || running.wait_with_output().unwrap());
    wait::until("the turn to start", || {
        stdout(&repo.by(&["log", "held"])).contains("prompt: HANG")
    });
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
    // Settled: a second cancel changes nothing and says what to use.
    let again = stdout(&repo.by(&["cancel", "held"]));
    assert!(
        again.starts_with("held had already stopped (interrupted), so the cancel changed nothing")
            && again.contains("by discard held"),
        "{again}"
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
    let agent = fake_agent!().display().to_string();
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
    let exited = wait::try_until_for(std::time::Duration::from_secs(10), || {
        child.try_wait().unwrap()
    });
    match exited {
        Ok(status) => assert!(status.success()),
        Err(_) => {
            let _ = child.kill();
            panic!("by watch did not exit on q");
        }
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
    use std::time::Duration;
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
        .stderr(Stdio::null());
    let mut process = watch.spawn().unwrap();
    let mut keys = process.stdin.take().unwrap();
    // Every row the terminal has shown, recorded whenever the cursor
    // leaves it: ratatui redraws only changed cells, and several frames
    // can arrive in one read, so neither the raw stream nor a screen per
    // read holds every phrase. Keys are typed once the screen is ready.
    let screens = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let reader = {
        let screens = screens.clone();
        let mut out = process.stdout.take().unwrap();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut term = Screen::new(30, 120);
            let mut buf = [0u8; 4096];
            while let Ok(n) = out.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let rows = term.feed(&buf[..n]);
                screens.lock().unwrap().extend(rows);
            }
        })
    };
    let drawn = || screens.lock().unwrap().len();
    let shows_after = |from: usize, text: &str| {
        let screens = screens.lock().unwrap();
        screens[from.min(screens.len())..]
            .iter()
            .any(|screen| screen.contains(text))
    };
    let wait_for = |from: usize, text: &str| {
        wait::until(&format!("{text:?} to be drawn"), || {
            if shows_after(from, text) {
                return Ok(());
            }
            // The screen as it is now: the last frame's rows, and any
            // error `by watch` printed on its way out.
            let screens = screens.lock().unwrap();
            Err(format!(
                "the screen:\n{}",
                screens[screens.len().saturating_sub(30)..].join("\n")
            ))
        })
    };
    // The dashboard draws after entering raw mode, so keys typed once a
    // ready row is shown are not flushed with the cooked-mode buffer.
    wait_for(0, "ready");
    // A follow-up first: `s`, a line of text, Enter; it runs in the
    // background while the dashboard carries on.
    keys.write_all(b"sWRITE x.txt=2\r").unwrap();
    wait::until("the send to finish", || {
        let shown = repo.json(&["show", "w", "--json"]);
        if shown["turns"] == 2 && shown["status"]["state"] == "ready" {
            Ok(())
        } else {
            Err(shown)
        }
    });
    // `m` asks only once the dashboard has seen the branch ready again;
    // until its question (its `n not now` choice) is drawn, ask again.
    wait::until("m to ask to merge", || {
        let from = drawn();
        keys.write_all(b"m").unwrap();
        wait::try_until_for(Duration::from_secs(2), || shows_after(from, "not now")).is_ok()
    });
    let from = drawn();
    keys.write_all(b"y").unwrap();
    wait::until("the merge to happen", || {
        fs::read_to_string(repo.root.join("x.txt")).is_ok_and(|t| t == "2\n")
    });
    // The result reaches the screen, then quit.
    wait_for(from, "merged w into main (");
    keys.write_all(b"qq").unwrap();
    drop(keys);
    let status = process.wait().unwrap();
    reader.join().unwrap();
    let screens = screens.lock().unwrap();
    assert!(
        status.success(),
        "{}",
        screens.last().cloned().unwrap_or_default()
    );
    // The send ran in the background with its output kept: its notice
    // can be replaced before a frame shows it, so look for the log.
    let logs: Vec<_> = fs::read_dir(repo.root.join(".branchyard/watch"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("w-send-"))
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    for expected in [
        "running: by send w 'WRITE x.txt=2'",
        "┌ merge w ─",
        "candidate ",
        "merged w into main (",
        "◆ merged into main",
    ] {
        assert!(
            screens.iter().any(|screen| screen.contains(expected)),
            "{expected:?} never shown; last screen drawn:\n{}",
            screens
                .iter()
                .rev()
                .find(|s| !s.trim().is_empty())
                .cloned()
                .unwrap_or_default()
        );
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
    let agent = fake_agent!().display().to_string();
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

/// The battery's harness mismatch: `--harness claude-code` came back as
/// `claude-code-stream-json` with nothing relating the two. Every surface
/// names a harness by the ID typed and the profile it resolved to.
#[test]
fn a_harness_is_named_by_the_id_typed_and_its_profile_everywhere() {
    let repo = Repo::new();
    let retry = repo.dir.join("retry.py");
    fs::write(
        &retry,
        "import branchyard as b\ntry:\n    b.send('kid', retry=True)\n\
         except b.DeniedError as e:\n    print('python', e.kind)\n",
    )
    .unwrap();
    let prompt = [
        "SH by spawn 'WRITE kid.txt=k' --name kid --harness gemini-cli".to_owned(),
        "SH by inspect".to_owned(),
        "SH by inspect --json".to_owned(),
        "SH by spawn x --name other --harness qwen-code".to_owned(),
        "SH by send kid --retry".to_owned(),
        format!("SH python3 {}", retry.display()),
    ]
    .join("\n");
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    for expected in [
        "spawned kid on gemini-cli (gemini-cli-acp) from",
        "harnesses: gemini-cli (gemini-cli-acp) only (its own)",
        "root may not delegate to qwen-code (qwen-code-acp); allowed: gemini-cli \
         (gemini-cli-acp) only (its own)",
        // Nothing of kid's was cut off, so there is nothing to retry.
        "kid has no cut-off turn to retry",
        "python denied",
    ] {
        assert!(said.contains(expected), "{expected:?} missing from\n{said}");
    }
    assert!(
        said.lines()
            .any(|l| l.starts_with("harness ") && l.ends_with(" gemini-cli (gemini-cli-acp)")),
        "{said}"
    );
    let (_, me) = sh_json(&said, 2);
    assert_eq!(me["envelope"]["harnesses"], serde_json::json!([]));
    assert_eq!(
        me["allowed_harnesses"],
        serde_json::json!(["gemini-cli-acp"])
    );
    let events = stdout(&repo.by(&["log", "root"]));
    assert!(
        events.contains("started on gemini-cli (gemini-cli-acp)"),
        "{events}"
    );
}

/// The battery's inherited whole-suite check: each child inherits its
/// parent's, which passes only once every sibling is in, and integrating
/// one alone failed with nothing but the check's output. `by inspect`
/// shows the check and who shares it, and the failure names the siblings
/// and the command, in text, JSON and Python.
#[test]
fn a_failed_shared_check_names_the_siblings_to_integrate_together() {
    let repo = Repo::new();
    let script = "import branchyard as b\n\
                  try:\n    b.integrate('a')\n\
                  except b.CheckFailedError as e:\n    print('python', e.kind, e.integrate_together)\n";
    let file = repo.dir.join("shared-check.py");
    fs::write(&file, script).unwrap();
    let prompt = [
        "SH by spawn 'WRITE a.txt=a' --name a --wait --json".to_owned(),
        "SH by spawn 'WRITE b.txt=b' --name b --wait --json".to_owned(),
        "SH by inspect a".to_owned(),
        "SH by integrate a".to_owned(),
        "SH by integrate a --json".to_owned(),
        format!("SH python3 {}", file.display()),
        "SH by inspect a --json".to_owned(),
        "SH by integrate a b".to_owned(),
    ]
    .join("\n");
    let out = repo.by_agent(&[
        "run",
        &prompt,
        "--name",
        "root",
        "--delegate",
        "--yes",
        "--check",
        "test -f a.txt -a -f b.txt",
    ]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    for expected in [
        "test -f a.txt -a -f b.txt (inherited from root); shared with b, so they are \
         integrated together: by integrate a b",
        "Siblings b share root's check, which they inherited, which may pass only with all \
         of them: integrate them together, `by integrate a b`",
        "python check_failed ['a', 'b']",
        "merged a into by/root",
        ", after `test -f a.txt -a -f b.txt` passed once on the result",
    ] {
        assert!(said.contains(expected), "{expected:?} missing from\n{said}");
    }
    // Merges stack, and each line shows its own range: b's starts where
    // a's ended, and the target moved once over both.
    let range = |prefix: &str| -> (String, String) {
        let line = said.lines().find(|l| l.starts_with(prefix)).unwrap();
        let inner = line.rsplit_once('(').unwrap().1.trim_end_matches(')');
        let inner = inner.split(',').next().unwrap();
        let (from, to) = inner.split_once("..").unwrap();
        (from.to_owned(), to.to_owned())
    };
    let (a_from, a_to) = range("merged a into by/root");
    let (b_from, b_to) = range("merged b into by/root");
    assert_eq!(a_to, b_from, "{said}");
    let moved = said
        .lines()
        .find(|l| l.starts_with("by/root moved once: "))
        .unwrap();
    assert!(moved.contains(&format!("{a_from}..{b_to}")), "{said}");
    let (code, failed) = sh_json(&said, 4);
    assert_eq!(code, 1, "{said}");
    assert_eq!(failed["error"]["kind"], "check_failed");
    let detail = &failed["error"]["detail"];
    assert_eq!(detail["integrate_together"], serde_json::json!(["a", "b"]));
    assert_eq!(detail["siblings"], serde_json::json!(["b"]));
    assert_eq!(detail["inherited_from"], "root");
    let (_, a) = sh_json(&said, 6);
    assert_eq!(
        a["check"],
        serde_json::json!(["test", "-f", "a.txt", "-a", "-f", "b.txt"])
    );
    assert_eq!(a["check_inherited"], true);
    assert_eq!(a["check_shared_with"], serde_json::json!(["b"]));
}

/// The battery's crash scenario: after its engine stopped, the meta was
/// continued with `by send` and no budget, and its own `by inspect` had
/// no budget line: a root's limits lived only in the turn that was given
/// them. A turn that gives none now keeps the ones given before.
#[test]
fn a_send_without_limits_keeps_the_ones_given_before() {
    let repo = Repo::new();
    let out = repo.by_agent(&[
        "run",
        "WRITE r.txt=r",
        "--name",
        "root",
        "--delegate",
        "--yes",
        "--budget-usd",
        "5",
        "--max-turns",
        "9",
    ]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let out = repo.by_agent(&["send", "root", "SH by inspect --json\nSH by inspect"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    let (_, me) = sh_json(&said, 0);
    assert_eq!(me["max_usd"], 5.0, "{said}");
    assert!(said.contains(" of $5.00 left"), "{said}");
    // A send that gives a limit replaces it.
    let out = repo.by_agent(&["send", "root", "SH by inspect --json", "--budget-usd", "7"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    // The log holds every turn's replies: this is the third command.
    let (_, me) = sh_json(&reply(&repo, "root"), 2);
    assert_eq!(me["max_usd"], 7.0);
}

/// The battery's recovery scenario: a child's question was steered into
/// its parent's running turn and counted delivered once written to the
/// harness, which reads it only at its next step; the parent's
/// `by inbox --unread` in that step printed `empty`. Its own inbox now
/// reads such a message as unread until the turn ends.
#[test]
fn a_message_steered_into_the_running_turn_is_still_unread_in_it() {
    let repo = Repo::new();
    let prompt = [
        "SH by spawn 'SH by report tests-pass' --name kid",
        "SH by wait kid > /dev/null",
        "SH by inbox --unread --json",
        "SH by inbox --unread",
    ]
    .join("\n");
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    let (code, unread) = sh_json(&said, 2);
    assert_eq!(code, 0, "{said}");
    let messages = unread["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1, "{said}");
    assert_eq!(messages[0]["text"], "tests-pass");
    assert!(said.contains("from kid [unread]: tests-pass"), "{said}");
}

/// The battery's knowledge-work campaign: `by spawn` took its prompt only
/// as an argument, so a meta with a long task spawned from Python. It
/// reads one from a file, or from standard input with `-`.
#[test]
fn spawn_reads_its_prompt_from_a_file_or_stdin() {
    let repo = Repo::new();
    let task = repo.dir.join("task.md");
    fs::write(&task, "WRITE file.txt=f\n\n").unwrap();
    let empty = repo.dir.join("empty.md");
    fs::write(&empty, "\n").unwrap();
    let prompt = [
        format!(
            "SH by spawn --prompt-file {} --name from-file --wait --json",
            task.display()
        ),
        "SH printf 'WRITE stdin.txt=s' | by spawn --prompt-file - --name from-stdin --wait --json"
            .to_owned(),
        format!("SH by spawn --prompt-file {} --name none", empty.display()),
        format!("SH by spawn inline --prompt-file {}", task.display()),
    ]
    .join("\n");
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    for (n, name) in [(0, "from-file"), (1, "from-stdin")] {
        let (code, child) = sh_json(&said, n);
        assert_eq!(code, 0, "{said}");
        assert_eq!(child["name"], name);
        assert_eq!(child["status"]["state"], "ready", "{child}");
    }
    assert_eq!(
        repo.json(&["show", "from-file", "--json"])["prompt"],
        "WRITE file.txt=f"
    );
    assert!(said.contains("holds no prompt"), "{said}");
    assert!(said.contains("cannot be used with"), "{said}");
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
        matches!(steered["state"]["state"].as_str(), Some("accepted")),
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
    let agent = fake_agent!().display().to_string();
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
    wait::until("live to start", || {
        let log = repo.by(&["log", "live", "--json"]);
        log.status.success() && stdout(&log).contains("waiting for steering")
    });
    let limited = repo.by(&["send", "live", "--steer", "x", "--budget-usd", "1"]);
    assert!(!limited.status.success());
    assert!(stderr(&limited).contains("send --steer takes only --json"));
    let out = repo.by(&["send", "live", "--steer", "try the other file"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out)
            .starts_with("accepted: it joined live's running turn, and the model reads it at once"),
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
                  w = b.wait_all(c.name); s = w.settled[0]; \
                  print('typed', s.status.state, s.state, s.status == 'ready', s.status.reason); \
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
        "typed ready ready True None",
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
    let agent = fake_agent!().display().to_string();
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
    let cancelled = repo.json(&["cancel", "kid", "--json"]);
    assert_eq!(cancelled["cancelled"], serde_json::json!([]));
    assert_eq!(cancelled["already"], true, "{cancelled}");
    assert!(
        cancelled["note"]
            .as_str()
            .unwrap()
            .contains("by integrate kid"),
        "{cancelled}"
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
            "mcp tools: spawn,inspect,events,send,steer,propose_integration,cancel,discard,children"
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
    let agent = fake_agent!().display().to_string();
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
    // The seat still bounds its workers. (The envelope's max_children
    // would not: it counts live children, and these have settled.)
    assert!(
        third["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already has 2 children in seat worker, the seat's instances"),
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

    let agent = fake_agent!().display().to_string();
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

/// A depth-0 child, which may not spawn, still acts as itself: it publishes
/// and reads its storage, asks and reports to its parent and reads its own
/// inbox, through `by` on its `PATH`. Everything outside its branch is
/// refused. Before, it had no token, so its `by` refused all of it.
#[test]
fn a_leaf_child_uses_its_storage_and_messages_its_parent_but_nothing_else() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "say hi", "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    repo.json(&[
        "spawn", "say hi", "--parent", "root", "--name", "sib", "--wait", "--yes", "--json",
    ]);
    let file = repo.dir.join("sib.txt");
    fs::write(&file, "sibling's\n").unwrap();
    let theirs = repo.json(&[
        "artifact",
        "publish",
        file.to_str().unwrap(),
        "--media-type",
        "text/plain",
        "--branch",
        "sib",
        "--json",
    ]);
    // --media-type is recorded, outside a harness as inside one.
    assert_eq!(theirs["media_type"], "text/plain", "{theirs}");
    let theirs = theirs["id"].as_str().unwrap().to_owned();
    let prompt = [
        "SH printf hi > data.txt".to_owned(),
        "SH by artifact publish data.txt --media-type application/json --json".to_owned(),
        "SH by artifact list --json".to_owned(),
        "SH by inspect --json".to_owned(),
        "SH by report halfway --json".to_owned(),
        "SH by ask 'which file?' --json".to_owned(),
        "SH by inbox --json".to_owned(),
        "SH test \"$BRANCHYARD_BY\" = \"$(command -v by)\"".to_owned(),
        "SH by spawn x --name grandkid --json".to_owned(),
        "SH by inspect root --json".to_owned(),
        "SH by artifact list --branch root --json".to_owned(),
        format!("SH by artifact get {theirs} --out x.txt --json"),
        "SH by cancel sib --json".to_owned(),
        "SH by discard sib --json".to_owned(),
        "SH by integrate sib --json".to_owned(),
    ]
    .join("\n");
    let leaf = repo.json(&[
        "spawn", &prompt, "--parent", "root", "--name", "leaf", "--wait", "--yes", "--json",
    ]);
    assert_eq!(leaf["envelope"]["max_depth"], 0, "{leaf}");
    let said = reply(&repo, "leaf");
    let (code, published) = sh_json(&said, 1);
    assert_eq!(code, 0, "{said}");
    assert_eq!(published["publisher_branch"], "leaf");
    assert_eq!(published["media_type"], "application/json");
    let (code, listed) = sh_json(&said, 2);
    assert_eq!(code, 0, "{said}");
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["publisher_branch"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["leaf"], "a sibling's artifact is not listed");
    let (code, me) = sh_json(&said, 3);
    assert_eq!((code, me["name"].as_str()), (0, Some("leaf")), "{said}");
    let (code, reported) = sh_json(&said, 4);
    assert_eq!((code, reported["to"].as_str()), (0, Some("root")), "{said}");
    let (code, asked) = sh_json(&said, 5);
    assert_eq!(code, 0, "{said}");
    assert_eq!(asked["message"]["kind"], "question");
    let (code, inbox) = sh_json(&said, 6);
    assert_eq!(
        (code, inbox["branch"].as_str()),
        (0, Some("leaf")),
        "{said}"
    );
    assert!(said.contains("sh: 0\n"), "{said}");
    let found_by = said.split("sh: ").nth(8).unwrap();
    assert!(
        found_by.starts_with("0\n"),
        "BRANCHYARD_BY is by on PATH: {said}"
    );
    for (n, why) in [
        (8, "leaf may not spawn: its envelope's max_depth is 0"),
        (9, "leaf may not act on root"),
        (10, "acts only as leaf"),
        (11, "may not read"),
        (12, "leaf may not cancel"),
        (13, "leaf may not discard"),
        (14, "leaf may not integrate"),
    ] {
        let (code, refused) = sh_json(&said, n);
        assert_eq!(code, 1, "command {n}: {said}");
        assert_eq!(refused["error"]["kind"], "denied", "command {n}: {refused}");
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(message.contains(why), "command {n}: {message}");
    }
    // The parent got the report and the question, which it answers.
    let messages = repo.json(&["inbox", "--as", "root", "--unread", "--json"]);
    let messages = messages["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "{messages:?}");
    // The same token reaches the MCP server, with the same scope.
    let sent = repo.json(&[
        "send",
        "leaf",
        "MCP list_artifacts {}\nMCP spawn {\"prompt\": \"x\"}",
        "--wait",
        "--yes",
        "--json",
    ]);
    assert_ne!(sent["status"]["state"], "failed", "{sent}");
    let said = reply(&repo, "leaf");
    assert!(said.contains("mcp list_artifacts: ["), "{said}");
    assert!(
        said.contains("mcp spawn error: denied: leaf may not spawn"),
        "{said}"
    );
}

/// The ask/answer protocol with a leaf: it asks and waits, its parent
/// answers from another process, and the wait returns the answer.
#[test]
fn a_leaf_asks_and_waits_for_its_parents_answer() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "say hi", "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let mut leaf = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["spawn", "SH by ask 'tabs or spaces?' --wait 60 --json"])
        .args([
            "--parent", "root", "--name", "leaf", "--wait", "--yes", "--json",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let question = wait::until("the leaf's question", || {
        let inbox = repo.json(&["inbox", "--as", "root", "--json"]);
        inbox["messages"]
            .as_array()
            .and_then(|m| m.first())
            .and_then(|m| m["id"].as_u64())
    });
    repo.json(&[
        "answer",
        &question.to_string(),
        "tabs",
        "--as",
        "root",
        "--json",
    ]);
    assert!(leaf.wait().unwrap().success());
    let (code, asked) = sh_json(&reply(&repo, "leaf"), 0);
    assert_eq!(code, 0);
    assert_eq!(asked["answer"]["text"], "tabs", "{asked}");
}

/// Every harness Branchyard starts finds `by`, delegated or not.
#[test]
fn every_harness_gets_by_on_its_path() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "ENV BRANCHYARD_BY", "--name", "plain", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let said = reply(&repo, "plain");
    assert!(said.contains("BRANCHYARD_BY=/"), "{said}");
    assert!(said.contains(env!("CARGO_BIN_EXE_by")), "{said}");
}

/// `by discard` sets a settled child aside, inside a harness and out:
/// discarded with the reason, its record kept, never run again. Like any
/// settled child it holds no slot in `max_children`, which counts the live
/// ones and points to `by discard`. `by cancel` on a settled child says to
/// use it, and `by rm` releases the lease it deletes without a warning.
#[test]
fn discard_sets_a_settled_child_aside_and_frees_its_slot() {
    let repo = Repo::new();
    let mut prompt = vec![
        "SH by spawn 'say hi' --name k --wait --json".to_owned(),
        "SH by cancel k --json".to_owned(),
        "SH by discard k --reason 'not needed' --json".to_owned(),
        "SH python3 -c \"import branchyard as b; print('py', b.discard('k').status['state'])\""
            .to_owned(),
    ];
    // k holds no slot: four live children fill the envelope beside it.
    for name in ["a", "b", "c", "d"] {
        prompt.push(format!("SH by spawn AWAIT_STEER --name {name} --json"));
    }
    prompt.push("SH by spawn 'say hi' --name e --json".to_owned());
    for name in ["a", "b", "c", "d"] {
        prompt.push(format!("SH by send {name} --steer 'that is all' --json"));
    }
    let out = repo.by_agent(&[
        "run",
        &prompt.join("\n"),
        "--name",
        "root",
        "--delegate",
        "--yes",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let said = reply(&repo, "root");
    let (code, cancelled) = sh_json(&said, 1);
    assert_eq!(code, 0, "{said}");
    assert_eq!(cancelled["cancelled"], serde_json::json!([]));
    assert!(
        cancelled["note"].as_str().unwrap().contains("by discard k"),
        "{cancelled}"
    );
    let (code, discarded) = sh_json(&said, 2);
    assert_eq!(code, 0, "{said}");
    assert_eq!(
        discarded["status"],
        serde_json::json!({"state": "discarded", "reason": "not needed"})
    );
    assert!(said.contains("py discarded"), "{said}");
    for n in 4..8 {
        assert_eq!(sh_json(&said, n).0, 0, "{said}");
    }
    let (code, full) = sh_json(&said, 8);
    assert_eq!(code, 1, "{said}");
    let message = full["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("4 live children") && message.contains("by discard"),
        "{full}"
    );
    for n in 9..13 {
        assert_eq!(sh_json(&said, n).0, 0, "{said}");
    }
    let cancel = repo.by(&["cancel", "a"]);
    assert!(cancel.status.success(), "{}", stderr(&cancel));
    assert!(
        stdout(&cancel).contains("by discard a"),
        "{}",
        stdout(&cancel)
    );
    let text = repo.by(&["discard", "a", "--reason", "lost the race"]);
    assert!(text.status.success(), "{}", stderr(&text));
    // One line per discard, not the whole inspection.
    assert_eq!(
        stdout(&text),
        "discarded a: lost the race; its record, worktree and cost stay until `by rm a`\n"
    );
    // The discarded child keeps its name until it is removed, and the
    // refusal says how to free it.
    let taken = repo.by(&["spawn", "x", "--parent", "root", "--name", "a", "--yes"]);
    assert!(!taken.status.success());
    assert!(
        stderr(&taken).contains("branch a already exists")
            && stderr(&taken).contains("`by rm a` frees it"),
        "{}",
        stderr(&taken)
    );
    let shown = repo.json(&["show", "a", "--json"]);
    assert_eq!(shown["status"]["state"], "discarded");
    // A discarded child runs no more turns.
    let again = repo.by(&["send", "a", "say more", "--yes", "--json"]);
    assert_eq!(again.status.code(), Some(1));
    let again: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert!(
        again["error"]["message"]
            .as_str()
            .unwrap()
            .contains("runs no more turns"),
        "{again}"
    );
    assert_eq!(
        repo.json(&["show", "a", "--json"])["status"]["state"],
        "discarded"
    );
    repo.json(&[
        "spawn", "say hi", "--parent", "root", "--name", "e", "--wait", "--yes", "--json",
    ]);
    // Removing a settled child is quiet: no lease warning, no escapes.
    let removed = repo.by(&["rm", "b"]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    let err = stderr(&removed);
    assert!(!err.contains("WARN") && !err.contains("fenced"), "{err}");
    assert!(
        !err.contains('\x1b') && !stdout(&removed).contains('\x1b'),
        "{err}"
    );
    // One list of children: inspect and children agree once b is removed,
    // and its name is free again.
    let inspected = repo.json(&["inspect", "root", "--json"]);
    let listed: Vec<String> = repo.json(&["children", "root", "--json"])["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["parent"] == "root")
        .map(|d| d["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(inspected["children"], serde_json::json!(listed));
    assert!(!listed.contains(&"b".to_owned()), "{listed:?}");
    repo.json(&[
        "spawn", "say hi", "--parent", "root", "--name", "b", "--wait", "--yes", "--json",
    ]);
    // The envelope names the harness and profile it allows.
    let shown = stdout(&repo.by(&["inspect", "root"]));
    let root = repo.json(&["show", "root", "--json"]);
    let (harness, profile) = (
        root["harness"].as_str().unwrap(),
        root["profile"].as_str().unwrap(),
    );
    assert!(
        shown.contains(&format!("harnesses: {harness} ({profile}) only (its own)")),
        "{shown}"
    );
}

/// `by run --deny` denies a tool before any permission answer, `--yes`
/// included, and the branch keeps it for later sends.
#[test]
fn run_denies_tools_and_the_branch_keeps_the_denial() {
    let repo = Repo::new();
    let out = repo.by_agent(&[
        "run",
        "PERMISSION WRITE marker.txt=x",
        "--name",
        "kept",
        "--deny",
        "write*",
        "--yes",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        reply(&repo, "kept").contains("denied"),
        "{}",
        reply(&repo, "kept")
    );
    let sent = repo.by_agent(&["send", "kept", "PERMISSION WRITE marker.txt=x", "--yes"]);
    assert!(sent.status.success(), "{}", stderr(&sent));
    assert!(
        reply(&repo, "kept").contains("denied"),
        "{}",
        reply(&repo, "kept")
    );
    assert_eq!(
        repo.json(&["show", "kept", "--json"])["candidate"],
        Value::Null
    );
    // A cost refusal names the flag as well as the limit.
    let root = repo.by_agent(&[
        "run",
        "SH by spawn x --budget-usd 2 --json",
        "--name",
        "root",
        "--delegate",
        "--budget-usd",
        "1",
        "--yes",
    ]);
    assert!(root.status.success(), "{}", stderr(&root));
    let (code, refused) = sh_json(&reply(&repo, "root"), 0);
    assert_eq!(code, 1, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_usd (--budget-usd) 2 exceeds"),
        "{refused}"
    );
}

/// `--mcp NAME=https://URL` reaches the harness's MCP configuration; a
/// header is a secret, so it needs a private home, and names a server.
#[test]
fn mcp_takes_an_http_server_and_its_headers() {
    let repo = Repo::new();
    let record = repo.dir.join("launch.json");
    let agent = fake_agent!().display().to_string();
    let command = format!("{agent} --record-launch {}", record.display());
    let out = repo.by(&[
        "run",
        "say hi",
        "--name",
        "web",
        "--harness",
        "claude-code-stream-json",
        "--command",
        &command,
        "--mcp",
        "search=https://mcp.example.invalid/mcp",
        "--yes",
    ]);
    assert!(record.is_file(), "{}\n{}", stdout(&out), stderr(&out));
    let launched: Value = serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
    assert_eq!(
        launched["mcp_config"]["servers"],
        serde_json::json!(["search"]),
        "{launched}"
    );
    let header = repo.by(&[
        "run",
        "say hi",
        "--mcp",
        "search=https://mcp.example.invalid/mcp",
        "--mcp-header",
        "search:Authorization=@/nonexistent",
        "--harness",
        "claude-code-stream-json",
        "--command",
        &command,
        "--yes",
    ]);
    assert!(!header.status.success());
    assert!(
        stderr(&header).contains("--isolated"),
        "{}",
        stderr(&header)
    );
    let unknown = repo.by(&[
        "run",
        "say hi",
        "--mcp-header",
        "other:Authorization=VAR",
        "--harness",
        "claude-code-stream-json",
        "--command",
        &command,
    ]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        stderr(&unknown).contains("no --mcp other="),
        "{}",
        stderr(&unknown)
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

/// Just enough of a terminal to read ratatui's output: absolute cursor
/// moves, clears, carriage returns and newlines; colours and modes are
/// ignored, and every character is one cell wide. Bytes that end in the
/// middle of a character or an escape wait for the next read.
struct Screen {
    cells: Vec<Vec<char>>,
    row: usize,
    col: usize,
    pending: Vec<u8>,
    seen: Vec<String>,
}

impl Screen {
    fn new(rows: usize, cols: usize) -> Screen {
        Screen {
            cells: vec![vec![' '; cols]; rows],
            row: 0,
            col: 0,
            pending: Vec::new(),
            seen: Vec::new(),
        }
    }

    /// Takes the bytes read; returns the rows shown since the last call,
    /// each as it was when the cursor left it, then every row as it is.
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) => e.valid_up_to(),
        };
        let text = String::from_utf8(self.pending[..valid].to_vec()).unwrap();
        let used = self.apply(&text);
        self.pending.drain(..used);
        let mut rows = std::mem::take(&mut self.seen);
        rows.extend(self.cells.iter().map(|row| row.iter().collect::<String>()));
        rows
    }

    /// Applies what it can; returns how many bytes it used, stopping
    /// before an escape sequence that is not complete yet.
    fn apply(&mut self, text: &str) -> usize {
        let mut chars = text.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            match c {
                '\x1b' => match chars.next() {
                    None => return at,
                    Some((_, '[')) => {
                        let mut params = String::new();
                        let mut last = None;
                        for (_, c) in chars.by_ref() {
                            if ('@'..='~').contains(&c) {
                                last = Some(c);
                                break;
                            }
                            params.push(c);
                        }
                        if last.is_none() {
                            return at;
                        }
                        self.csi(&params, last);
                    }
                    Some((_, ']')) => {
                        let mut ended = false;
                        while let Some((_, c)) = chars.next() {
                            if c == '\x07'
                                || (c == '\x1b' && chars.next_if(|&(_, c)| c == '\\').is_some())
                            {
                                ended = true;
                                break;
                            }
                        }
                        if !ended {
                            return at;
                        }
                    }
                    Some(_) => {}
                },
                '\r' => {
                    self.leave();
                    self.col = 0;
                }
                '\n' => {
                    self.leave();
                    self.row += 1;
                }
                c if c.is_control() => {}
                c => {
                    if let Some(cell) = self
                        .cells
                        .get_mut(self.row)
                        .and_then(|r| r.get_mut(self.col))
                    {
                        *cell = c;
                    }
                    self.col += 1;
                }
            }
        }
        text.len()
    }

    /// Records the row the cursor is leaving.
    fn leave(&mut self) {
        if let Some(row) = self.cells.get(self.row) {
            self.seen.push(row.iter().collect());
        }
    }

    fn csi(&mut self, params: &str, last: Option<char>) {
        let numbers: Vec<usize> = params
            .split(';')
            .map(|n| n.trim_start_matches('?').parse().unwrap_or(0))
            .collect();
        match last {
            Some('H' | 'f') => {
                self.leave();
                self.row = numbers.first().copied().unwrap_or(1).max(1) - 1;
                self.col = numbers.get(1).copied().unwrap_or(1).max(1) - 1;
            }
            Some('J') if numbers.first() == Some(&2) || params.is_empty() => {
                self.leave();
                for row in &mut self.cells {
                    row.fill(' ');
                }
            }
            Some('K') => {
                if let Some(row) = self.cells.get_mut(self.row) {
                    for cell in row.iter_mut().skip(self.col) {
                        *cell = ' ';
                    }
                }
            }
            // Leaving the alternate screen shows the primary one again.
            Some('l') if params == "?1049" => {
                self.leave();
                for row in &mut self.cells {
                    self.seen.push(row.iter().collect());
                    row.fill(' ');
                }
            }
            _ => {}
        }
    }
}

/// [`sh_json`] for a reply whose JSON may itself hold `sh: `, such as a
/// child's last message: only a line that starts with it counts.
fn sh_line_json(reply: &str, n: usize) -> (i32, Value) {
    let start = reply
        .match_indices("sh: ")
        .map(|(at, _)| at)
        .filter(|at| *at == 0 || reply[..*at].ends_with('\n'))
        .nth(n)
        .unwrap_or_else(|| panic!("no command {n} in {reply}"));
    let (status, rest) = reply[start + 4..].split_once('\n').unwrap();
    let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
    (status.parse().unwrap(), values.next().unwrap().unwrap())
}

/// A child's prompt that runs until `go` exists, then writes `file`: a
/// child still running when its parent's turn ends, until released.
fn held_child(go: &Path, file: &str) -> String {
    format!(
        "SH until [ -f {} ]; do sleep 0.05; done; echo k > {file}",
        go.display()
    )
}

/// `by run` in the background; on drop, releases what `go` holds and stops
/// it, so a failing test leaves nothing running.
struct Background {
    child: Option<std::process::Child>,
    go: std::path::PathBuf,
}

impl Drop for Background {
    fn drop(&mut self) {
        let _ = fs::write(&self.go, "");
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The battery's depth2 and pysdk scenarios (M1), end to end through `by
/// run`: Claude Code moved its wait into a background task and ended its
/// turn ("I'll get a notification when they finish"). Branchyard closed the
/// session, the parent ended `no changes`, and its child was never
/// integrated. Now the parent waits on its child, `by run` keeps waiting,
/// and the parent's next turn starts when the child settles: here it
/// integrates the child.
#[test]
fn by_run_wakes_a_parent_whose_turn_ended_while_its_child_ran() {
    let mut repo = Repo::new();
    let go = repo.dir.join("go");
    let wake = repo.dir.join("wake.txt");
    fs::write(&wake, "SH by integrate kid --json\n").unwrap();
    repo.0.set_env("FAKE_ACP_WAKE", wake.to_str().unwrap());
    let agent = fake_agent!().display().to_string();
    let prompt = format!(
        "SH by spawn '{}' --name kid --json",
        held_child(&go, "kid.txt")
    );
    let mut running = Background {
        child: Some(
            repo.command(env!("CARGO_BIN_EXE_by"))
                .args(["run", &prompt, "--name", "root", "--delegate", "--yes"])
                .args(["--harness", "gemini-cli", "--command", &agent])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        ),
        go: go.clone(),
    };
    let state = wait::until("root's first turn to end", || {
        let out = repo.by(&["show", "root", "--json"]);
        let shown: Value = serde_json::from_slice(&out.stdout).ok()?;
        let state = shown["status"]["state"].as_str()?.to_owned();
        (state != "running").then_some(state)
    });
    assert_eq!(state, "waiting_on_children", "the parent's turn ended");
    assert_eq!(
        repo.json(&["show", "kid", "--json"])["status"]["state"],
        "running"
    );
    fs::write(&go, "").unwrap();
    let out = running.child.take().unwrap().wait_with_output().unwrap();
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let root = repo.json(&["show", "root", "--json"]);
    assert_eq!(root["turns"], 2, "{root}");
    assert_eq!(root["status"]["state"], "ready", "{root}");
    let kid = repo.json(&["show", "kid", "--json"]);
    assert_eq!(kid["status"]["state"], "merged", "{kid}");
    assert_eq!(kid["status"]["target"], "by/root");
    // The wake's prompt said what kid did; the woken turn integrated it.
    let log = stdout(&repo.by(&["log", "root"]));
    assert!(log.contains("<branchyard-wake>"), "{log}");
    let said = reply(&repo, "root");
    assert!(said.contains(r#""target": "by/root""#), "{said}");
    assert_eq!(repo.git(&["show", "by/root:kid.txt"]), "k\n");
}

/// A parked parent that may not be woken (its `--max-turns` is spent)
/// settles as its turn ended when its children do, and a sibling waiting
/// for it starts then, while a cousin still runs (so `by run`'s own sweep
/// for waiting branches, which runs once nothing does, cannot be what
/// started it): the settle goes through the one path every settle takes.
#[test]
fn a_parked_branch_settled_without_a_wake_starts_its_dependents() {
    let repo = Repo::new();
    let go = repo.dir.join("go");
    let hold = repo.dir.join("hold");
    // Releases `other` however the test ends.
    let _other = Background {
        child: None,
        go: hold.clone(),
    };
    let grandchild = repo.dir.join("gk.prompt");
    fs::write(&grandchild, held_child(&go, "gk.txt")).unwrap();
    let lead = repo.dir.join("lead.prompt");
    fs::write(
        &lead,
        format!(
            "SH by spawn \"$(cat {})\" --name gk --json",
            grandchild.display()
        ),
    )
    .unwrap();
    let prompt = [
        format!(
            "SH by spawn \"$(cat {})\" --name lead --max-turns 1 --json",
            lead.display()
        ),
        "SH by spawn 'WRITE s.txt=s' --name sib --depends-on lead --json".to_owned(),
        format!(
            "SH by spawn '{}' --name other --json",
            held_child(&hold, "other.txt")
        ),
        "SH until by inspect lead --json | grep -q waiting_on_children; do sleep 0.05; done"
            .to_owned(),
        format!("SH touch {}", go.display()),
    ]
    .join("\n");
    let agent = fake_agent!().display().to_string();
    let _running = Background {
        child: Some(
            repo.command(env!("CARGO_BIN_EXE_by"))
                .args(["run", &prompt, "--name", "root", "--delegate=2", "--yes"])
                .args(["--harness", "gemini-cli", "--command", &agent])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        ),
        go: go.clone(),
    };
    wait::until("sib to start once lead settles", || {
        let out = repo.by(&["show", "sib", "--json"]);
        let shown: Value = serde_json::from_slice(&out.stdout).ok()?;
        let state = shown["status"]["state"].as_str()?.to_owned();
        (state == "ready").then_some(())
    });
    assert_eq!(
        repo.json(&["show", "other", "--json"])["status"]["state"],
        "running"
    );
    let lead = repo.json(&["show", "lead", "--json"]);
    assert_eq!(lead["turns"], 1, "{lead}");
    assert_eq!(lead["status"]["state"], "no_changes", "{lead}");
    let log = stdout(&repo.by(&["log", "lead"]));
    assert!(
        log.contains("max_turns is spent; it was not woken"),
        "{log}"
    );
}

/// `by wait` blocks until children settle, inside a harness and outside
/// one; with --timeout it gives up and says so (M1). The delegate skill
/// told agents to use `by inspect --wait`, which never existed.
#[test]
fn by_wait_blocks_until_children_settle() {
    let repo = Repo::new();
    let go = repo.dir.join("go");
    let prompt = [
        format!(
            "SH by spawn '{}' --name kid --json",
            held_child(&go, "kid.txt")
        ),
        "SH by wait --timeout 0.2 --json".to_owned(),
        format!("SH touch {}", go.display()),
        "SH by wait --json".to_owned(),
        "SH by wait kid --any --json".to_owned(),
    ]
    .join("\n");
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    let (code, timed_out) = sh_line_json(&said, 1);
    assert_eq!(code, 1, "{said}");
    assert_eq!(timed_out["timed_out"], true);
    assert_eq!(timed_out["pending"], serde_json::json!(["kid"]));
    let (code, waited) = sh_line_json(&said, 3);
    assert_eq!(code, 0, "{said}");
    assert_eq!(waited["pending"], serde_json::json!([]));
    assert_eq!(waited["settled"][0]["name"], "kid");
    assert_eq!(waited["settled"][0]["status"]["state"], "ready");
    let (code, any) = sh_line_json(&said, 4);
    assert_eq!((code, any["settled"][0]["name"].as_str()), (0, Some("kid")));
    // Outside a harness, with the branches named.
    let outside = repo.json(&["wait", "kid", "--json"]);
    assert_eq!(outside["settled"][0]["status"]["state"], "ready");
    let unnamed = repo.by(&["wait"]);
    assert!(!unnamed.status.success());
    assert!(
        stderr(&unnamed).contains("needs the branches"),
        "{}",
        stderr(&unnamed)
    );
    let help = stdout(&repo.by(&["wait", "--help"]));
    assert!(
        help.contains("--any") && help.contains("--timeout"),
        "{help}"
    );
}

/// The battery's calc4, conflict3, envelope, graph and recovery scenarios
/// (M2): a check the children inherit runs the whole suite, which no child
/// passes alone, so each `by integrate` was refused; agents merged
/// siblings into each other, wrote per-test checks, or ran `git merge`
/// themselves around the gate. `by integrate a b` merges them together and
/// checks the result once.
#[test]
fn by_integrate_merges_siblings_together_and_checks_once() {
    let repo = Repo::new();
    let prompt = [
        "SH by spawn 'WRITE a.part=a' --name a --wait --json",
        "SH by spawn 'WRITE b.part=b' --name b --wait --json",
        "SH by integrate a --json",
        "SH by integrate a b --json",
        "SH by integrate b --json",
        "SH by spawn 'WRITE c.part=c' --name c",
    ]
    .join("\n");
    let out = repo.by_agent(&[
        "run",
        &prompt,
        "--name",
        "root",
        "--delegate",
        "--yes",
        "--check",
        "sh -c 'test -f a.part && test -f b.part'",
    ]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    let (code, alone) = sh_json(&said, 2);
    assert_eq!(
        (code, alone["error"]["kind"].as_str()),
        (1, Some("check_failed"))
    );
    let (code, together) = sh_json(&said, 3);
    assert_eq!(code, 0, "{said}");
    assert_eq!(together["target"], "by/root");
    let names: Vec<&str> = together["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["branch"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a", "b"]);
    // One move of the parent: two merges on its line, then nothing else.
    let head = together["commit"].as_str().unwrap();
    assert_eq!(
        repo.git(&["rev-parse", &format!("{head}^1^1")]).trim(),
        together["previous"].as_str().unwrap()
    );
    let (code, again) = sh_json(&said, 4);
    assert_eq!(
        (code, again["already"].as_bool()),
        (0, Some(true)),
        "{said}"
    );
    for name in ["a", "b"] {
        assert_eq!(
            repo.json(&["show", name, "--json"])["status"]["state"],
            "merged"
        );
    }
    // The spawn says which check the child inherits.
    assert!(
        said.contains(
            "its check, inherited from its parent: sh -c test -f a.part && test -f b.part"
        ),
        "{said}"
    );
    let help = stdout(&repo.by(&["spawn", "--help"]));
    assert!(help.contains("defaults to its parent's"), "{help}");
    assert!(help.contains("by integrate a b"), "{help}");
}

/// The battery's conflict3, envelope, depth2 and recovery scenarios (M3):
/// a child whose work its parent already contains (brought in by a sibling
/// that merged it, or by a `git merge` the parent ran) stayed `ready`
/// forever, and integrating it was an error. Status is now reconciled with
/// git: it is recorded as merged through the merge that brought it in.
#[test]
fn a_child_its_parent_already_contains_is_recorded_merged() {
    let repo = Repo::new();
    let prompt = [
        "SH by spawn 'WRITE bounds.txt=b' --name bounds --wait --json",
        "SH by spawn 'WRITE words.txt=w' --name words --base by/bounds --wait --json",
        "SH by integrate words --json",
        "SH by children --json",
        "SH by integrate bounds --json",
        "SH by spawn 'WRITE ids.txt=i' --name ids --wait --json",
        "SH git merge --no-edit -q by/ids",
    ]
    .join("\n");
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    let (code, merged) = sh_json(&said, 2);
    assert_eq!(code, 0, "{said}");
    let (_, children) = sh_json(&said, 3);
    let bounds = children["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "bounds")
        .unwrap()
        .clone();
    assert_eq!(bounds["status"]["state"], "merged", "{children}");
    assert_eq!(bounds["status"]["commit"], merged["commit"]);
    let (code, again) = sh_json(&said, 4);
    assert_eq!(code, 0, "{said}");
    assert_eq!(again["already"], true);
    assert!(again["via"].as_str().unwrap().contains("Merge"), "{again}");
    // A merge the parent's harness ran itself is recorded when its turn
    // ends.
    let ids = repo.json(&["show", "ids", "--json"]);
    assert_eq!(ids["status"]["state"], "merged", "{ids}");
    assert_eq!(ids["status"]["target"], "by/root");
}

/// The Python module's `wait_any`, `wait_all` and `integrate(*names)` (M1,
/// M2): the battery's pysdk scenario waited in a background loop instead.
#[test]
fn the_python_module_waits_for_children_and_integrates_them_together() {
    let repo = Repo::new();
    let go = repo.dir.join("go");
    let script = format!(
        "import branchyard as b\n\
         b.spawn('WRITE a.part=a', name='a')\n\
         b.spawn('{held}', name='held')\n\
         first = b.wait_any('a', 'held')\n\
         print('first', [i.name for i in first.settled], first.pending)\n\
         try:\n    b.wait_all('held', timeout=0.2)\nexcept b.RunningError as e:\n    print('timed out', e.kind)\n\
         open('{go}', 'w').close()\n\
         done = b.wait_all()\n\
         print('all', sorted(i.name for i in done.settled), done.pending)\n\
         merged = b.integrate('a', 'held')\n\
         print('merged', merged.target, [m.branch for m in merged.branches])\n\
         again = b.integrate('a')\n\
         print('again', again.already)\n",
        held = held_child(&go, "held.part"),
        go = go.display(),
    );
    let file = repo.dir.join("orchestrate.py");
    fs::write(&file, script).unwrap();
    let prompt = format!("SH python3 {}", file.display());
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    for expected in [
        "first ['a'] ['held']",
        "timed out running",
        "all ['held'] []",
        "merged by/root ['a', 'held']",
        "again True",
    ] {
        assert!(said.contains(expected), "{expected:?} missing from\n{said}");
    }
}
