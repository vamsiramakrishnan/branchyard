//! The plugin end to end, against `by serve` with the fake ACP agent and a
//! fake `herdr` on `PATH` that records every call and hands out pane IDs.
//! Hermetic; requires `git`, `sh`, `mkdir`, `sleep` and `kill`.

use branchyard_testkit::{wait, Scratch};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

const PLUGIN: &str = env!("CARGO_BIN_EXE_branchyard-herdr");

/// The `by` binary, built once next to the plugin's.
fn by_exe() -> &'static Path {
    branchyard_testkit::built("branchyard-cli", "by", Path::new(PLUGIN))
}

fn testkit_agent() -> &'static Path {
    branchyard_testkit::fake_agent(Path::new(PLUGIN))
}

/// Records each call as its arguments separated by \x1f, one call a line.
/// `plugin pane open` hands out `w1:pN`; `pane get` and `pane
/// report-agent` fail with `pane_not_found` for a pane not in `panes`.
const FAKE_HERDR: &str = r#"#!/bin/sh
d="$FAKE_HERDR_DIR"
while ! mkdir "$d/lock" 2>/dev/null; do sleep 0.01; done
{ for a in "$@"; do printf '%s\037' "$a"; done; printf '\n'; } >> "$d/calls"
missing() {
    rmdir "$d/lock"
    printf '{"id":"cli","error":{"code":"pane_not_found","message":"pane %s not found"}}\n' "$1" >&2
    exit 1
}
case "$1 $2 $3" in
"plugin pane open")
    n=$(cat "$d/next" 2>/dev/null || echo 1)
    echo $((n + 1)) > "$d/next"
    echo "w1:p$n" >> "$d/panes"
    rmdir "$d/lock"
    printf '{"id":"cli:plugin","result":{"type":"plugin_pane_opened","plugin_pane":{"plugin_id":"branchyard","entrypoint":"log","pane":{"pane_id":"w1:p%s","tab_id":"w1:t%s","workspace_id":"w1"}}}}\n' "$n" "$n"
    exit 0
    ;;
"pane get "*|"pane report-agent "*)
    grep -qx "$3" "$d/panes" 2>/dev/null || missing "$3"
    ;;
esac
rmdir "$d/lock"
printf '{"id":"cli","result":{"type":"ok"}}\n'
"#;

struct Fake {
    dir: PathBuf,
    bin: PathBuf,
}

impl Fake {
    fn new(root: &Path) -> Fake {
        let dir = root.join("herdr");
        let bin = root.join("bin");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let script = bin.join("herdr");
        fs::write(&script, FAKE_HERDR).unwrap();
        assert!(Command::new("chmod")
            .arg("+x")
            .arg(&script)
            .status()
            .unwrap()
            .success());
        Fake { dir, bin }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                line.split('\u{1f}')
                    .filter(|a| !a.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .collect()
    }

    /// Branch tabs opened: (branch, pane) in order.
    fn opened(&self) -> Vec<(String, String)> {
        let mut panes = 0;
        let mut out = Vec::new();
        for call in self.calls() {
            if call.starts_with(&["plugin".into(), "pane".into(), "open".into()]) {
                panes += 1;
                if flag(&call, "--entrypoint") == Some("log") {
                    let branch = call
                        .iter()
                        .find_map(|a| a.strip_prefix("BRANCHYARD_HERDR_BRANCH="))
                        .unwrap()
                        .to_owned();
                    out.push((branch, format!("w1:p{panes}")));
                }
            }
        }
        out
    }

    fn pane(&self, branch: &str) -> String {
        self.opened()
            .into_iter()
            .rev()
            .find(|(b, _)| b == branch)
            .unwrap_or_else(|| panic!("no pane for {branch}: {:?}", self.calls()))
            .1
    }

    /// `state (message)` for each report on `pane`, in order.
    fn reports(&self, pane: &str) -> Vec<String> {
        self.calls()
            .iter()
            .filter(|c| c.len() > 2 && c[0] == "pane" && c[1] == "report-agent" && c[2] == pane)
            .map(|c| {
                assert_eq!(flag(c, "--source"), Some("custom:branchyard"));
                assert_eq!(flag(c, "--agent"), Some("branchyard"));
                assert!(flag(c, "--seq").unwrap().parse::<u64>().is_ok());
                match flag(c, "--message") {
                    Some(m) => format!("{} ({m})", flag(c, "--state").unwrap()),
                    None => flag(c, "--state").unwrap().to_owned(),
                }
            })
            .collect()
    }

    fn last_report(&self, branch: &str) -> Option<String> {
        let pane = self
            .opened()
            .into_iter()
            .rev()
            .find(|(b, _)| b == branch)?
            .1;
        self.reports(&pane).pop()
    }

    fn close(&self, pane: &str) {
        let path = self.dir.join("panes");
        let kept: String = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| *l != pane)
            .map(|l| format!("{l}\n"))
            .collect();
        fs::write(path, kept).unwrap();
    }
}

fn flag<'a>(call: &'a [String], name: &str) -> Option<&'a str> {
    let at = call.iter().position(|a| a == name)?;
    call.get(at + 1).map(String::as_str)
}

/// A scratch directory (`.0` is its path), removed on drop.
struct Dir(PathBuf, #[allow(dead_code)] Scratch); // the Scratch is held for its Drop

impl Dir {
    fn new() -> Dir {
        by_exe();
        testkit_agent();
        let scratch = Scratch::new("herdr");
        Dir(scratch.path().to_path_buf(), scratch)
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

fn command(program: impl AsRef<std::ffi::OsStr>, dir: &Path) -> Command {
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
        .env("PAGER", "cat");
    for name in [
        "BRANCHYARD_REMOTE",
        "BRANCHYARD_TOKEN_FILE",
        "BRANCHYARD_REPO",
        "BRANCHYARD_CA_FILE",
        "BRANCHYARD_BY",
        "HERDR_BIN_PATH",
        "HERDR_PLUGIN_CONFIG_DIR",
        "HERDR_PLUGIN_CONTEXT_JSON",
        "HERDR_PANE_ID",
        "HERDR_WORKSPACE_ID",
    ] {
        command.env_remove(name);
    }
    command.stdin(Stdio::null());
    command
}

/// `by serve` on loopback; stopped with SIGTERM on drop.
struct Served {
    child: Child,
    url: String,
    token_file: PathBuf,
}

impl Served {
    fn start(dir: &Path, data: &Path, repo: &Path, listen: &str) -> Served {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("server.log"))
            .unwrap();
        let mut child = command(by_exe(), dir)
            .args([
                "serve",
                "--listen",
                listen,
                "--quiet",
                "--shutdown-grace",
                "5",
            ])
            .args(["--allow-client-commands", "--data-dir"])
            .arg(data)
            .arg("--repo")
            .arg(format!("app={}", repo.display()))
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let url = line
            .trim()
            .strip_prefix("listening on ")
            .unwrap_or_else(|| {
                panic!(
                    "server did not start: {line:?}\n{}",
                    fs::read_to_string(dir.join("server.log")).unwrap_or_default()
                )
            })
            .to_owned();
        Served {
            child,
            url,
            token_file: data.join("token"),
        }
    }

    fn stop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let exited = wait::try_until_for(Duration::from_secs(20), || {
            self.child.try_wait().unwrap().is_some()
        });
        if exited.is_err() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn by(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut by = command(by_exe(), cwd);
        by.arg("--remote")
            .arg(&self.url)
            .arg("--token-file")
            .arg(&self.token_file)
            .args(args);
        if matches!(args[0], "run" | "send") {
            by.arg("--command").arg(testkit_agent());
        }
        if args[0] == "run" {
            by.args(["--harness", "gemini-cli"]);
        }
        by
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        let output = self.by(cwd, args).output().unwrap();
        assert!(
            output.status.success() || args.contains(&"EXIT"),
            "by {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.stop();
        }
    }
}

/// Kills its process on drop.
struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The plugin's environment inside Herdr, with the fake `herdr` on `PATH`.
fn plugin(dir: &Path, fake: &Fake, server: &Served, args: &[&str]) -> Command {
    let path = format!(
        "{}:{}",
        fake.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = command(PLUGIN, dir);
    command
        .env("PATH", path)
        .env("FAKE_HERDR_DIR", &fake.dir)
        .env("HERDR_PLUGIN_ID", "branchyard")
        .env("HERDR_PLUGIN_STATE_DIR", dir.join("state"))
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("BRANCHYARD_HERDR_DEBOUNCE_MS", "100")
        .env("BRANCHYARD_BY", by_exe())
        .env("BRANCHYARD_REMOTE", &server.url)
        .env("BRANCHYARD_TOKEN_FILE", &server.token_file)
        .env("BRANCHYARD_REPO", "app")
        .args(args);
    command
}

fn bridge(dir: &Path, fake: &Fake, server: &Served, log: &Path) -> Killed {
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .unwrap();
    Killed(
        plugin(dir, fake, server, &["bridge"])
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    )
}

/// Run an action as Herdr would, with `pane` focused.
fn action(dir: &Path, fake: &Fake, server: &Served, name: &str, pane: &str) -> Output {
    plugin(dir, fake, server, &["action", name])
        .env(
            "HERDR_PLUGIN_CONTEXT_JSON",
            format!(r#"{{"workspace_id":"w1","focused_pane_id":"{pane}"}}"#),
        )
        .output()
        .unwrap()
}

#[test]
fn branches_get_one_pane_each_and_their_states_follow_the_feed() {
    let dir = Dir::new();
    let root = &dir.0;
    let repo = dir.repo("repo");
    let data = root.join("data");
    let mut server = Served::start(root, &data, &repo, "127.0.0.1:0");
    let fake = Fake::new(root);
    let bridge_log = root.join("bridge.log");
    let log = || fs::read_to_string(&bridge_log).unwrap_or_default();
    let long = Duration::from_secs(60);

    // A branch from before the bridge started, and one already merged.
    server.run(
        root,
        &["run", "WRITE early.txt=1", "--name", "early", "--yes"],
    );
    server.run(
        root,
        &["run", "WRITE done.txt=1", "--name", "done", "--yes"],
    );
    server.run(root, &["merge", "done"]);

    let mut bridge_process = bridge(root, &fake, &server, &bridge_log);
    wait::until_with_context(
        "the listed branch",
        long,
        || fake.last_report("early").as_deref() == Some("idle (ready to merge)"),
        || format!("{:?}\n{}", fake.calls(), log()),
    );
    // A branch merged before the bridge saw it gets no pane.
    assert!(fake.opened().iter().all(|(b, _)| b != "done"));

    // Tabs open without focus, in the bridge's workspace, running the
    // plugin's log pane with the server settings; named after the branch.
    let early = fake.pane("early");
    let open = fake
        .calls()
        .into_iter()
        .find(|c| c.contains(&"BRANCHYARD_HERDR_BRANCH=early".to_owned()))
        .unwrap();
    for expected in [
        "--no-focus",
        "tab",
        "w1",
        "branchyard",
        "BRANCHYARD_REPO=app",
        &format!("BRANCHYARD_REMOTE={}", server.url),
    ] {
        assert!(open.iter().any(|a| a == expected), "{expected}: {open:?}");
    }
    assert!(fake.calls().contains(&vec![
        "pane".into(),
        "rename".into(),
        early.clone(),
        "by: early".into()
    ]));

    // A turn that hangs is working until it is cancelled from its pane.
    let hang = Killed(
        server
            .by(root, &["run", "HANG", "--name", "slow", "--yes"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait::until_with_context(
        "a working branch",
        long,
        || fake.last_report("slow").as_deref() == Some("working"),
        || format!("{:?}\n{}", fake.calls(), log()),
    );
    let slow = fake.pane("slow");

    // `by log --follow`, remotely, shows the turn as it happens.
    let follow_out = root.join("follow.out");
    let follow = Killed(
        server
            .by(root, &["log", "--follow", "slow"])
            .stdout(fs::File::create(&follow_out).unwrap())
            .spawn()
            .unwrap(),
    );
    wait::until_with_context(
        "by log --follow to print the prompt",
        long,
        || {
            fs::read_to_string(&follow_out)
                .unwrap_or_default()
                .contains("HANG")
        },
        || fs::read_to_string(&follow_out).unwrap_or_default(),
    );

    let cancelled = action(root, &fake, &server, "cancel", &slow);
    assert!(
        cancelled.status.success(),
        "{}",
        String::from_utf8_lossy(&cancelled.stderr)
    );
    wait::until_with_context(
        "the cancelled branch",
        long,
        || fake.last_report("slow").as_deref() == Some("idle (interrupted)"),
        || format!("{:?}\n{}", fake.calls(), log()),
    );
    assert_eq!(fake.reports(&slow), ["working", "idle (interrupted)"]);
    drop(hang);
    wait::until_with_context(
        "by log --follow to print the new status",
        long,
        || {
            fs::read_to_string(&follow_out)
                .unwrap_or_default()
                .contains("interrupted")
        },
        || fs::read_to_string(&follow_out).unwrap_or_default(),
    );
    drop(follow);
    assert!(fake.calls().iter().any(|c| c.starts_with(&[
        "notification".into(),
        "show".into(),
        "Cancelled slow".into()
    ])));

    // Failures carry their reason.
    server.run(root, &["run", "EXIT", "--name", "bad", "--yes"]);
    wait::until_with_context(
        "the failed branch",
        long,
        || {
            fake.last_report("bad")
                .is_some_and(|r| r.starts_with("idle (failed: "))
        },
        || format!("{:?}\n{}", fake.calls(), log()),
    );

    // Merge from the branch's pane.
    let merged = action(root, &fake, &server, "merge", &early);
    assert!(
        merged.status.success(),
        "{}",
        String::from_utf8_lossy(&merged.stderr)
    );
    wait::until_with_context(
        "the merged branch",
        long,
        || fake.last_report("early").as_deref() == Some("idle (merged into main)"),
        || format!("{:?}\n{}", fake.calls(), log()),
    );

    // Send opens the popup for the focused branch; the popup runs by send.
    let sent = action(root, &fake, &server, "send", &slow);
    assert!(sent.status.success());
    let popup = fake.calls().pop().unwrap();
    assert_eq!(flag(&popup, "--entrypoint"), Some("send"));
    assert_eq!(flag(&popup, "--placement"), Some("popup"));
    assert!(popup.contains(&"BRANCHYARD_HERDR_BRANCH=slow".to_owned()));
    let mut popup_process = plugin(root, &fake, &server, &["send"])
        .env("BRANCHYARD_HERDR_BRANCH", "slow")
        .env("BRANCHYARD_HERDR_SEND_ARGS", "--yes")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    popup_process
        .stdin
        .take()
        .unwrap()
        .write_all(b"say more\n")
        .unwrap();
    let popup_out = popup_process.wait_with_output().unwrap();
    assert!(
        popup_out.status.success(),
        "{}",
        String::from_utf8_lossy(&popup_out.stderr)
    );
    wait::until_with_context(
        "the sent branch",
        long,
        || fake.last_report("slow").as_deref() == Some("idle (no changes)"),
        || format!("{:?}\n{}", fake.calls(), log()),
    );

    // An action outside a branch pane says so.
    let lost = action(root, &fake, &server, "merge", "w9:p9");
    assert!(!lost.status.success());

    // Restart the server on the same port: the bridge resumes after its
    // cursor, so nothing is reported twice and no pane is opened again.
    let before: Vec<_> = fake.opened();
    let reports_before = fake.reports(&early);
    let listen = server.url.trim_start_matches("http://").to_owned();
    server.stop();
    let mut server = Served::start(root, &data, &repo, &listen);
    server.run(root, &["run", "hello", "--name", "after", "--yes"]);
    wait::until_with_context(
        "a branch after the restart",
        long,
        || fake.last_report("after").as_deref() == Some("idle (no changes)"),
        || format!("{:?}\n{}", fake.calls(), log()),
    );
    // The bridge logs this with `tracing`, as `... INFO reconnecting cursor=N`.
    let resumed = log()
        .lines()
        .filter(|l| l.contains("reconnecting"))
        .find_map(|l| {
            l.split_whitespace()
                .find_map(|word| word.strip_prefix("cursor="))
        })
        .map(|c| c.parse::<u64>().unwrap())
        .unwrap_or_else(|| panic!("no reconnect in\n{}", log()));
    assert!(resumed > 0);
    assert_eq!(fake.reports(&early), reports_before);
    let after = fake.opened();
    assert_eq!(&after[..before.len()], &before[..]);
    assert_eq!(after.len(), before.len() + 1);

    // A pane closed in Herdr is opened again at the branch's next change.
    let after_pane = fake.pane("after");
    fake.close(&after_pane);
    server.run(root, &["send", "after", "WRITE after.txt=1", "--yes"]);
    wait::until_with_context(
        "a second pane for a closed one",
        long,
        || fake.opened().iter().filter(|(b, _)| b == "after").count() == 2,
        || format!("{:?}\n{}", fake.calls(), log()),
    );

    // Reports change something each time: debounced, and never repeated.
    for (branch, pane) in fake.opened() {
        let reports = fake.reports(&pane);
        assert!(
            reports.windows(2).all(|pair| pair[0] != pair[1]),
            "{branch}: {reports:?}"
        );
    }

    // A restarted bridge reuses its panes: it checks them and reports the
    // current states on them without opening any.
    let opened = fake.opened().len();
    drop(bridge_process);
    let calls = fake.calls().len();
    bridge_process = bridge(root, &fake, &server, &bridge_log);
    wait::until_with_context(
        "the restarted bridge to report",
        long,
        || {
            fake.calls()[calls..]
                .iter()
                .filter(|c| c.len() > 1 && c[1] == "report-agent")
                .count()
                >= 4
        },
        || format!("{:?}\n{}", fake.calls(), log()),
    );
    let fresh = &fake.calls()[calls..];
    assert!(fresh.iter().any(|c| c[..2] == ["pane", "get"]));
    assert_eq!(fake.opened().len(), opened);
    drop(bridge_process);
    server.stop();
}

#[test]
fn by_log_follow_prints_local_events_as_they_are_recorded() {
    let dir = Dir::new();
    let repo = dir.repo("repo");
    let run = |args: &[&str]| {
        let output = command(by_exe(), &repo)
            .args(args)
            .arg("--command")
            .arg(testkit_agent())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&[
        "run",
        "first words",
        "--name",
        "b",
        "--harness",
        "gemini-cli",
        "--yes",
    ]);
    let out = dir.0.join("follow.out");
    let _follow = Killed(
        command(by_exe(), &repo)
            .args(["log", "--follow", "b"])
            .stdout(fs::File::create(&out).unwrap())
            .spawn()
            .unwrap(),
    );
    let read = || fs::read_to_string(&out).unwrap_or_default();
    wait::until_with_context(
        "the first turn",
        Duration::from_secs(30),
        || read().contains("echo: first words"),
        read,
    );
    run(&["send", "b", "second words", "--yes"]);
    wait::until_with_context(
        "the second turn",
        Duration::from_secs(30),
        || read().contains("echo: second words"),
        read,
    );
    // Each reply printed once, whole.
    assert_eq!(read().matches("echo: first words").count(), 1);

    // With --json, one event object per line.
    let json_out = dir.0.join("follow.json");
    let _json = Killed(
        command(by_exe(), &repo)
            .args(["log", "--follow", "--json", "b"])
            .stdout(fs::File::create(&json_out).unwrap())
            .spawn()
            .unwrap(),
    );
    let lines = || fs::read_to_string(&json_out).unwrap_or_default();
    wait::until_with_context(
        "JSON lines",
        Duration::from_secs(30),
        || lines().contains("second words"),
        lines,
    );
    for line in lines().lines() {
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(event["activity"].is_string(), "{line}");
    }
}
