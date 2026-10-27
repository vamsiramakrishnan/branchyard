//! `by setup`, bare `by` and `by init plugin --status` end to end: the built
//! binary in a throwaway repository, a private HOME, and fake harness
//! executables first on `PATH` (shell scripts that print a version). No
//! model is called, no real harness runs, and nothing is written outside
//! the scratch directory.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use branchyard_testkit::Repo;
use serde_json::Value;

/// The fake harnesses: the program on `PATH`, and the harness it is.
const FAKES: [(&str, &str); 4] = [
    ("claude", "claude-code"),
    ("codex", "codex"),
    ("gemini", "gemini-cli"),
    ("qwen", "qwen-code"),
];

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `text` on one line, its runs of white space single spaces: the card
/// wraps prose, and a phrase may fall across two lines.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A repository with a Cargo.toml (so the files suggest `cargo test`), a
/// private HOME, and four fake harnesses first on a `PATH` that has, besides
/// the system's tools, `by` itself when asked.
struct Setup {
    repo: Repo,
    home: PathBuf,
}

impl Setup {
    fn new() -> Setup {
        Setup::with(true)
    }

    fn with(by_on_path: bool) -> Setup {
        let mut repo = Repo::init_with(
            Path::new(env!("CARGO_BIN_EXE_by")),
            &[("Cargo.toml", "[package]\nname = \"app\"\n")],
        );
        let home = repo.dir.join("home");
        let fakebin = repo.dir.join("fakebin");
        let tools = repo.dir.join("tools");
        for dir in [&home, &fakebin, &tools] {
            fs::create_dir_all(dir).unwrap();
        }
        for (program, _) in FAKES {
            let path = fakebin.join(program);
            fs::write(&path, format!("#!/bin/sh\necho \"{program} 9.9.9\"\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        if by_on_path {
            symlink(env!("CARGO_BIN_EXE_by"), tools.join("by")).unwrap();
        }
        repo.set_env("HOME", &home.display().to_string());
        // The detection script needs the system's tools, so those stay.
        repo.set_env(
            "PATH",
            &format!("{}:{}:/usr/bin:/bin", fakebin.display(), tools.display()),
        );
        repo.remove_env("XDG_CONFIG_HOME");
        // The credential variables setup reports, by name: none of this
        // machine's, so the logins it finds are the test's choice.
        for name in branchyard_setup::probe::KNOWN_VARIABLES {
            repo.remove_env(name);
        }
        Setup { repo, home }
    }

    fn by(&self, args: &[&str]) -> Output {
        self.repo.by(args)
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.by(args);
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("by {args:?}: {e}\n{}", text(&out)))
    }

    fn file(&self, relative: &str) -> PathBuf {
        self.home.join(relative)
    }

    /// Every file under HOME and the repository (not `.git`), by path:
    /// what `by setup` changed shows as a difference between two calls.
    fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        for root in [&self.home, &self.repo.root] {
            let mut stack = vec![root.clone()];
            while let Some(dir) = stack.pop() {
                for entry in fs::read_dir(&dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.file_name().is_some_and(|n| n == ".git") {
                        continue;
                    }
                    match path.is_dir() {
                        true => stack.push(path),
                        false => {
                            let body = fs::read(&path).unwrap();
                            files.push((path, body));
                        }
                    }
                }
            }
        }
        files.sort();
        files
    }

    /// The lines of `by`'s output, for a card's length.
    fn lines(out: &Output) -> usize {
        stdout(out).lines().count()
    }
}

#[test]
fn setup_yes_writes_the_config_and_the_hooks_and_prints_the_card() {
    let s = Setup::new();
    let out = s.by(&["setup", "--yes"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let card = stdout(&out);
    for expected in [
        "Detected",
        "Claude Code 9.9.9",
        "Qwen Code 9.9.9",
        "wrote branchyard.toml: harness claude-code · check \"cargo test\" · local, no isolation",
        "installed 13 files (hooks, commands, skills, statusline)",
        "installed 1 file (hooks)",
        "hooks: by hook answers · never heard: start a new Codex session",
        "hooks: by hook answers · never heard: start a new Gemini CLI session",
        "Only you can do:",
        "Codex runs no hook until you trust it",
        "Next: by run \"fix the flaky test\" --check \"cargo test\"",
    ] {
        assert!(card.contains(expected), "{expected:?} missing:\n{card}");
    }
    assert!(!card.contains("silent"), "{card}");
    assert!(
        !card.contains("hooks: verified"),
        "answering is not proof:\n{card}"
    );
    // The config loads with the loader `by` uses, and names the check.
    let config = fs::read_to_string(s.repo.root.join("branchyard.toml")).unwrap();
    assert!(config.contains("check = \"cargo test\""), "{config}");
    let valid = s.by(&["config", "validate"]);
    assert!(valid.status.success(), "{}", text(&valid));
    // The hooks reach each harness's own files, merged.
    for (path, calls) in [
        (".claude/settings.json", "by hook session-start"),
        (".codex/hooks.json", "by hook --harness codex session-start"),
        (
            ".gemini/settings.json",
            "by hook --harness gemini-cli user-prompt-submit",
        ),
        (
            ".qwen/settings.json",
            "by hook --harness qwen-code session-start",
        ),
    ] {
        let body = fs::read_to_string(s.file(path)).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert!(body.contains(calls), "{path}:\n{body}");
    }
    assert!(s.file(".claude/skills/setup/SKILL.md").exists());
    // The hook check left no cursor behind.
    assert!(!s.repo.root.join(".branchyard/hooks").exists());
}

#[test]
fn a_second_run_writes_nothing_and_says_so() {
    let s = Setup::new();
    assert!(s.by(&["setup", "--yes"]).status.success());
    let before = s.snapshot();
    let again = s.by(&["setup", "--yes"]);
    assert_eq!(again.status.code(), Some(0), "{}", text(&again));
    let card = stdout(&again);
    assert!(card.contains("Nothing to write: already set up."), "{card}");
    assert!(
        card.contains("branchyard.toml exists; left as it is"),
        "{card}"
    );
    assert!(card.contains("already current"), "{card}");
    assert!(card.contains("hooks: by hook answers"), "{card}");
    // Nothing was created, changed or removed.
    assert_eq!(s.snapshot(), before);
    let json = s.json(&["setup", "--yes", "--json"]);
    assert_eq!(json["written"], serde_json::json!([]));
    assert_eq!(json["config"]["state"], "exists");
    for harness in json["installed"].as_array().unwrap() {
        assert_eq!(harness["state"], "current", "{harness}");
    }
}

#[test]
fn dry_run_and_a_missing_terminal_write_nothing() {
    let s = Setup::new();
    let before = s.snapshot();
    let dry = s.by(&["setup", "--dry-run", "--yes"]);
    assert_eq!(dry.status.code(), Some(0), "{}", text(&dry));
    let card = stdout(&dry);
    assert!(card.contains("would write branchyard.toml"), "{card}");
    assert!(card.contains("would install 13 files"), "{card}");
    assert!(card.contains("create ~/.claude/settings.json"), "{card}");
    assert!(card.contains("Nothing was written (--dry-run)."), "{card}");
    assert!(card.contains("Next: by setup --yes"), "{card}");
    assert!(!card.contains("by hook answers"), "nothing to check yet");
    assert!(
        !card.contains("heard"),
        "nothing installed to hear:\n{card}"
    );
    assert_eq!(s.snapshot(), before);
    // No terminal and no --yes: the same, and it says how to write.
    let piped = s.by(&["setup"]);
    assert_eq!(piped.status.code(), Some(0), "{}", text(&piped));
    let card = stdout(&piped);
    assert!(
        card.contains("no terminal to ask on. Rerun with --yes"),
        "{card}"
    );
    assert!(card.contains("Next: by setup --yes"), "{card}");
    assert_eq!(s.snapshot(), before);
}

#[test]
fn json_is_one_object_with_what_was_detected_written_installed_and_left() {
    let s = Setup::new();
    // Without --yes it is a dry run: it says what it would write.
    let dry = s.json(&["setup", "--json"]);
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["written"], serde_json::json!([]));
    let would: Vec<&str> = dry["would_write"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(would.contains(&"branchyard.toml"), "{would:?}");
    assert_eq!(dry["next"], "by setup --yes");
    assert!(!s.repo.root.join("branchyard.toml").exists());

    let done = s.json(&["setup", "--json", "--yes"]);
    for key in [
        "detected",
        "written",
        "installed",
        "todo",
        "next",
        "dry_run",
    ] {
        assert!(done.get(key).is_some(), "{key} missing: {done}");
    }
    assert_eq!(done["dry_run"], false);
    assert_eq!(done["detected"]["check"], "cargo test");
    let harnesses: Vec<&str> = done["detected"]["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["harness"].as_str().unwrap())
        .collect();
    for (_, id) in FAKES {
        assert!(harnesses.contains(&id), "{id} not detected: {harnesses:?}");
    }
    assert_eq!(
        done["detected"]["default_harness"]["harness"],
        "claude-code"
    );
    assert_eq!(done["detected"]["default_harness"]["verified"], false);
    assert_eq!(
        done["detected"]["default_harness"]["auth_source"],
        "unknown"
    );
    let written: Vec<&str> = done["written"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(written.contains(&"branchyard.toml"), "{written:?}");
    assert!(
        written.iter().any(|p| p.ends_with(".codex/hooks.json")),
        "{written:?}"
    );
    let installed = done["installed"].as_array().unwrap();
    assert!(installed.len() >= 4, "{installed:?}");
    for harness in installed {
        assert_eq!(harness["state"], "installed", "{harness}");
        assert_eq!(harness["hooks"], "answers", "{harness}");
        assert_eq!(harness["heard"]["state"], "never", "{harness}");
        assert!(
            harness["heard"]["installed_at_ms"].as_u64().is_some(),
            "{harness}"
        );
    }
    let kinds: Vec<&str> = done["todo"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"login"), "{kinds:?}");
    assert!(kinds.contains(&"note"), "{kinds:?}");
    assert_eq!(
        done["next"],
        "by run \"fix the flaky test\" --check \"cargo test\""
    );
}

#[test]
fn flags_choose_what_is_installed() {
    let s = Setup::new();
    let out = s.by(&["setup", "--yes", "--harness", "codex"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(s.file(".codex/hooks.json").exists());
    assert!(!s.file(".claude").exists() && !s.file(".gemini").exists());
    assert!(s.repo.root.join("branchyard.toml").exists());

    let s = Setup::new();
    let out = s.by(&["setup", "--yes", "--no-hooks"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(s.repo.root.join("branchyard.toml").exists());
    for dir in [".claude", ".codex", ".gemini", ".qwen"] {
        assert!(!s.file(dir).exists(), "{dir}");
    }

    let s = Setup::new();
    let out = s.by(&["setup", "--yes", "--no-config"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!s.repo.root.join("branchyard.toml").exists());
    assert!(s.file(".codex/hooks.json").exists());
    assert!(
        stdout(&out).contains("skipped (--no-config)"),
        "{}",
        text(&out)
    );

    // A harness with no install is an error with the list; both flags
    // together are a usage error.
    let bad = s.by(&["setup", "--yes", "--harness", "nope"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(
        text(&bad).contains("no install for harness \"nope\""),
        "{}",
        text(&bad)
    );
    let both = s.by(&["setup", "--harness", "codex", "--no-hooks"]);
    assert_eq!(both.status.code(), Some(2), "{}", text(&both));
}

#[test]
fn an_unverified_login_is_never_called_logged_out_and_names_its_source() {
    let s = Setup::new();
    let out = s.by(&["setup", "--yes"]);
    let card = flat(&stdout(&out));
    assert!(
        card.contains("login") && card.contains("claude-code: not verified (checked:"),
        "{card}"
    );
    assert!(card.contains("Auth source: unknown."), "{card}");
    assert!(card.contains("a guess"), "{card}");
    assert!(card.contains("spends your money"), "{card}");
    assert!(
        card.contains("Claude Code: login not verified (checked:"),
        "{card}"
    );
    assert!(
        !card.contains("is logged out"),
        "a guess is not said as fact:\n{card}"
    );

    // A credentials file is evidence, and the source is named.
    let s = Setup::new();
    fs::create_dir_all(s.file(".claude")).unwrap();
    fs::write(s.file(".claude/.credentials.json"), "{}").unwrap();
    let out = s.by(&["setup", "--yes"]);
    let card = flat(&stdout(&out));
    assert!(
        card.contains("claude-code: credentials found. Auth source:"),
        "{card}"
    );
    assert!(!card.contains("Claude Code: login not verified"), "{card}");
    let json = s.json(&["setup", "--json"]);
    let default = &json["detected"]["default_harness"];
    assert_eq!(default["login"], "logged_in");
    assert_ne!(default["auth_source"], "unknown", "{default}");
}

#[test]
fn workspace_scripts_setup_wrote_are_shown_and_trusted_with_yes() {
    let s = Setup::new();
    let dry = stdout(&s.by(&["setup", "--dry-run"]));
    assert!(dry.contains("[workspace] scripts"), "{dry}");
    assert!(dry.contains("cargo fetch"), "{dry}");
    assert!(
        dry.contains("With --yes, setup trusts the ones it writes."),
        "{dry}"
    );

    let out = s.by(&["setup", "--yes"]);
    let card = stdout(&out);
    assert!(card.contains("trusted by this run"), "{card}");
    assert!(card.contains("by workspace untrust"), "{card}");
    assert!(!card.contains("not trusted"), "{card}");
    let shown = s.json(&["workspace", "show", "--json"]);
    assert_eq!(shown["trust"], "trusted", "{shown}");
    let json = s.json(&["setup", "--json"]);
    assert!(json["todo"]
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t["kind"] != "workspace"));
}

#[test]
fn hooks_are_silent_when_by_is_not_on_path_and_setup_says_so_once() {
    let s = Setup::with(false);
    let out = s.by(&["setup", "--yes"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let card = flat(&stdout(&out));
    assert_eq!(
        card.matches("hooks: by hook is silent (see below)").count(),
        4,
        "{card}"
    );
    assert!(!card.contains("by hook answers"), "{card}");
    assert!(
        !card.contains("never heard"),
        "the reason below is the news:\n{card}"
    );
    assert_eq!(
        card.matches("`by` is not on PATH").count(),
        1,
        "one line for all four:\n{card}"
    );
    assert!(
        card.contains("The hooks of Claude Code, Codex, Gemini CLI, Qwen Code are silent"),
        "{card}"
    );
    let json = s.json(&["setup", "--yes", "--json"]);
    for harness in json["installed"].as_array().unwrap() {
        assert_eq!(harness["hooks"], "silent", "{harness}");
        assert!(harness["hooks_detail"]
            .as_str()
            .unwrap()
            .contains("not on PATH"));
    }
}

/// Whether a line of `text` has `head`, then an age (`just now`, `12s
/// ago`: the machine may be slow), then `tail`.
fn has_line(text: &str, head: &str, tail: &str) -> bool {
    text.lines().any(|line| {
        line.split_once(head)
            .is_some_and(|(_, rest)| rest.contains(" ago") || rest.starts_with("just now"))
            && line.trim_end().ends_with(tail)
    })
}

/// What a Codex session sends its `SessionStart` hook, in `repo`.
fn codex_session_start(repo: &Path, session: &str) -> String {
    serde_json::json!({
        "session_id": session,
        "transcript_path": "/home/me/.codex/sessions/rollout.jsonl",
        "cwd": repo,
        "hook_event_name": "SessionStart",
        "source": "startup",
    })
    .to_string()
}

#[test]
fn the_card_says_whether_a_harness_was_heard_not_only_that_by_hook_answers() {
    let s = Setup::new();
    assert!(s.by(&["setup", "--yes"]).status.success());
    // Codex starts a session in the repository: its hook runs `by hook`.
    let hook = ["hook", "--harness", "codex", "session-start"];
    let heard = s
        .repo
        .by_with_stdin(&hook, &codex_session_start(&s.repo.root, "s1"));
    assert_eq!(heard.status.code(), Some(0), "{}", text(&heard));
    let again = s.by(&["setup", "--yes"]);
    let card = stdout(&again);
    assert!(
        has_line(&card, "hooks: by hook answers · heard ", "(session-start)"),
        "{card}"
    );
    assert!(
        card.contains("hooks: by hook answers · never heard: start a new Claude Code session"),
        "{card}"
    );
    let json = s.json(&["setup", "--json"]);
    let codex = json["installed"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["harness"] == "codex")
        .unwrap();
    assert_eq!(codex["heard"]["state"], "heard", "{codex}");
    assert_eq!(codex["heard"]["kind"], "session-start", "{codex}");
    assert_eq!(codex["heard"]["harness_event"], "SessionStart", "{codex}");
    assert_eq!(
        codex["heard"]["repository"],
        s.repo.root.display().to_string(),
        "{codex}"
    );

    // Bare `by` says the same in one row.
    let bare = s.by(&[]);
    let card = stdout(&bare);
    assert!(Setup::lines(&bare) <= 12, "{card}");
    assert!(
        has_line(
            &card,
            "  hooks       heard: Codex ",
            " · never heard: Claude Code, Gemini CLI, Qwen Code (start a new session)"
        ),
        "{card}"
    );
    // So does `by init plugin --status`, in a column of its own.
    let status = s.json(&["init", "plugin", "--status", "--json"]);
    let rows = status["harnesses"].as_array().unwrap();
    let state = |id: &str| {
        rows.iter().find(|r| r["harness"] == id).unwrap()["heard"]["state"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(state("codex"), "heard");
    assert_eq!(state("gemini-cli"), "never");
    let table = stdout(&s.by(&["init", "plugin", "--status"]));
    assert!(table.contains("HEARD"), "{table}");

    // Setup writes Codex's hooks again: what was heard was the old file.
    fs::remove_file(s.file(".codex/hooks.json")).unwrap();
    let reinstalled = stdout(&s.by(&["setup", "--yes"]));
    assert!(
        has_line(
            &reinstalled,
            "hooks: by hook answers · last heard ",
            "(session-start), before by setup rewrote the hooks: start a new Codex session"
        ),
        "{reinstalled}"
    );
    let json = s.json(&["setup", "--json"]);
    let codex = json["installed"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["harness"] == "codex")
        .unwrap();
    assert_eq!(codex["heard"]["state"], "stale", "{codex}");
    assert!(stdout(&s.by(&[])).contains("not heard since by setup: Codex"));
    // The next session is heard again.
    let heard = s
        .repo
        .by_with_stdin(&hook, &codex_session_start(&s.repo.root, "s2"));
    assert_eq!(heard.status.code(), Some(0), "{}", text(&heard));
    let json = s.json(&["setup", "--json"]);
    let codex = json["installed"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["harness"] == "codex")
        .unwrap();
    assert_eq!(codex["heard"]["state"], "heard", "{codex}");
}

#[test]
fn a_file_that_differs_holds_back_that_harness_alone() {
    let s = Setup::new();
    fs::create_dir_all(s.file(".claude/skills/setup")).unwrap();
    let mine = "my own skill, not Branchyard's\n";
    fs::write(s.file(".claude/skills/setup/SKILL.md"), mine).unwrap();
    let out = s.by(&["setup", "--yes"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let card = flat(&stdout(&out));
    assert!(card.contains("not installed"), "{card}");
    assert!(card.contains("differ from the packaged copy"), "{card}");
    let json = s.json(&["setup", "--yes", "--json"]);
    let claude = json["installed"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["harness"] == "claude-code")
        .unwrap();
    assert_eq!(claude["state"], "refused", "{claude}");
    assert!(claude["reason"]
        .as_str()
        .unwrap()
        .contains("differ from the packaged copy"));
    assert!(
        card.contains("Run: by init plugin claude-code --dry-run"),
        "{card}"
    );
    assert_eq!(
        fs::read_to_string(s.file(".claude/skills/setup/SKILL.md")).unwrap(),
        mine
    );
    // The others went in.
    assert!(s.file(".codex/hooks.json").exists());
    assert!(s.file(".gemini/settings.json").exists());
    // Named on purpose, the same refusal is an error.
    let named = s.by(&["setup", "--yes", "--harness", "claude-code"]);
    assert_eq!(named.status.code(), Some(1));
    assert!(text(&named).contains("Next: by init plugin claude-code --dry-run"));
}

#[test]
fn bare_by_is_a_status_card_with_one_next_step() {
    let s = Setup::new();
    // Not set up.
    let bare = s.by(&[]);
    assert_eq!(bare.status.code(), Some(0), "{}", text(&bare));
    let card = stdout(&bare);
    assert!(Setup::lines(&bare) <= 12, "{card}");
    assert!(
        card.contains("not yet: branchyard.toml no · .branchyard/ no"),
        "{card}"
    );
    assert!(card.contains("Next: by setup"), "{card}");
    assert!(card.contains("All commands: by --help"), "{card}");
    assert!(
        !card.contains("Work on branches"),
        "not the sixty commands:\n{card}"
    );
    // Set up, no branches yet.
    assert!(s.by(&["setup", "--yes", "--no-hooks"]).status.success());
    let card = stdout(&s.by(&[]));
    assert!(card.contains("branchyard.toml yes"), "{card}");
    assert!(
        card.contains("Next: by run \"fix the flaky test\""),
        "{card}"
    );
    // A finished branch: counted, and the next step is to look at it. (The
    // file setup wrote would run `cargo fetch` and `cargo test` around it.)
    fs::write(s.repo.root.join("branchyard.toml"), "version = 1\n").unwrap();
    let run = s
        .repo
        .by_agent(&["run", "WRITE hello.txt=hi", "--name", "hello", "--yes"]);
    assert!(run.status.success(), "{}", text(&run));
    let bare = s.by(&[]);
    let card = stdout(&bare);
    assert!(Setup::lines(&bare) <= 12, "{card}");
    assert!(card.contains("1 branch · 1 ready"), "{card}");
    assert!(card.contains("Next: by show hello"), "{card}");
    // The full list is one flag away.
    let help = stdout(&s.by(&["--help"]));
    assert!(help.contains("Start:") && help.contains("Work:"), "{help}");
    assert!(help.contains("\n  setup "), "{help}");
}

#[test]
fn bare_by_outside_a_repository_says_so_and_points_at_setup() {
    let dir = branchyard_testkit::Scratch::new("nogit");
    let out = Command::new(env!("CARGO_BIN_EXE_by"))
        .current_dir(dir.path())
        .env("GIT_CEILING_DIRECTORIES", dir.path().parent().unwrap())
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let card = stdout(&out);
    assert!(card.contains("is not inside a git repository"), "{card}");
    assert!(card.contains("Next: cd <repo> && by setup"), "{card}");
    // And setup says the same, as an error.
    let setup = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["setup", "--yes"])
        .current_dir(dir.path())
        .env("GIT_CEILING_DIRECTORIES", dir.path().parent().unwrap())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(setup.status.code(), Some(1));
    assert!(
        text(&setup).contains("is not inside a git repository"),
        "{}",
        text(&setup)
    );
}

#[test]
fn init_plugin_status_says_what_is_installed_and_what_is_left() {
    let s = Setup::new();
    // Before: detected, missing.
    let before = s.json(&["init", "plugin", "--status", "--json"]);
    assert_eq!(before["by_on_path"], true);
    let rows = before["harnesses"].as_array().unwrap();
    let codex = rows.iter().find(|r| r["harness"] == "codex").unwrap();
    assert_eq!(codex["detected"], true);
    assert_eq!(codex["state"], "missing");
    assert_eq!(codex["notes"], serde_json::json!([]));
    assert!(before["hidden"].as_u64().unwrap() > 0);

    assert!(s.by(&["setup", "--yes"]).status.success());
    let after = s.json(&["init", "plugin", "--status", "--json"]);
    let rows = after["harnesses"].as_array().unwrap();
    for id in ["claude-code", "codex", "gemini-cli", "qwen-code"] {
        let row = rows.iter().find(|r| r["harness"] == id).unwrap();
        assert_eq!(row["state"], "current", "{id}");
    }
    let codex = rows.iter().find(|r| r["harness"] == "codex").unwrap();
    assert!(codex["notes"][0]
        .as_str()
        .unwrap()
        .contains("Trust all and continue"));
    let table = s.by(&["init", "plugin", "--status"]);
    let table = stdout(&table);
    assert!(table.contains("by on PATH:"), "{table}");
    assert!(table.contains("Still on your list"), "{table}");
    assert!(
        table.contains("Codex: Codex runs no hook until you trust it"),
        "{table}"
    );

    // A file gone: stale, with the command that fixes it.
    fs::remove_file(s.file(".gemini/commands/branchyard/wait.toml")).unwrap();
    let after = s.json(&["init", "plugin", "--status", "--json", "gemini-cli"]);
    assert_eq!(after["harnesses"][0]["state"], "stale");
    assert_eq!(after["harnesses"].as_array().unwrap().len(), 1);
    let table = stdout(&s.by(&["init", "plugin", "--status", "gemini-cli"]));
    assert!(
        table.contains("by init plugin gemini-cli --dry-run"),
        "{table}"
    );

    // Without `by` on PATH it says so.
    let off = Setup::with(false);
    let table = stdout(&off.by(&["init", "plugin", "--status"]));
    assert!(table.contains("by is not on PATH"), "{table}");
    // And --status writes nothing, takes no step, and is for plugin.
    let bad = off.by(&["init", "plugin", "--status", "--apply"]);
    assert_eq!(bad.status.code(), Some(2));
    let bad = off.by(&["init", "project", "--status"]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn init_without_a_terminal_exits_2_and_keeps_its_hint() {
    let s = Setup::new();
    let out = s.by(&["init"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(out.stdout.is_empty(), "{}", stdout(&out));
    let hint = String::from_utf8_lossy(&out.stderr);
    assert!(
        hint.contains("asks questions on a terminal, and none is attached"),
        "{hint}"
    );
    assert!(hint.contains("by init project --json --next"), "{hint}");
}

#[test]
fn the_model_gateway_is_opt_in_in_the_generated_file() {
    let s = Setup::new();
    // Provider keys set by name: the gateway is still left out.
    let mut keyed = Setup::new();
    keyed.repo.set_env("GEMINI_API_KEY", "not-a-real-key");
    keyed.repo.set_env("CODEX_API_KEY", "not-a-real-key");
    let plain = keyed.by(&["init", "project", "--defaults", "--dry-run"]);
    let plain = stdout(&plain);
    assert!(!plain.contains("[models"), "{plain}");
    assert!(plain.contains("docs/model-gateway.md"), "{plain}");
    let added = |text: &str| text.lines().filter(|l| l.starts_with("  +")).count();
    // Chosen, it is written.
    let answers = s.repo.dir.join("answers.json");
    fs::write(&answers, "{\"models\": true}").unwrap();
    let chosen = keyed.by(&[
        "init",
        "project",
        "--defaults",
        "--dry-run",
        "--answers",
        answers.to_str().unwrap(),
    ]);
    let chosen = stdout(&chosen);
    assert!(chosen.contains("[models.backends.gemini]"), "{chosen}");
    // The block nobody asked for was two thirds of the file.
    assert!(added(&chosen) >= added(&plain) + 15, "{plain}\n{chosen}");
    assert!(
        !chosen.contains("Optional: route harness model calls"),
        "{chosen}"
    );
    // And by setup never asks for it.
    assert!(keyed.by(&["setup", "--yes", "--no-hooks"]).status.success());
    let written = fs::read_to_string(keyed.repo.root.join("branchyard.toml")).unwrap();
    assert!(!written.contains("[models"), "{written}");
    assert!(written.contains("docs/model-gateway.md"), "{written}");
}

/// `by setup` on a pseudo-terminal from util-linux `script`, accepting every
/// default with Enter: it asks isolation, shows the files, asks once to
/// write, asks about the `[workspace]` scripts (the default is no), and
/// leaves them untrusted with the command that trusts them.
#[cfg(target_os = "linux")]
#[test]
fn on_a_terminal_it_asks_once_and_leaves_untrusted_scripts_to_the_person() {
    use std::io::Write as _;
    if !Path::new("/usr/bin/script").exists() {
        eprintln!("skipped: no /usr/bin/script for a pseudo-terminal");
        return;
    }
    let s = Setup::new();
    let by = env!("CARGO_BIN_EXE_by");
    let mut run = s.repo.command("/usr/bin/script");
    run.args(["-qfc", &format!("{by} setup"), "/dev/null"])
        .env("TERM", "xterm")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = run.spawn().unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all("\r".repeat(40).as_bytes())
        .unwrap();
    let out = process.wait_with_output().unwrap();
    let screen = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{screen}");
    for expected in [
        "Detected",
        "Should harnesses run isolated",
        "will write branchyard.toml",
        "will install 13 files",
        "Write ",
        "Trust the [workspace] scripts above now?",
        "wrote branchyard.toml",
        "Only you can do:",
        "The [workspace] scripts in branchyard.toml are not trusted",
        "Run: by workspace trust",
    ] {
        assert!(screen.contains(expected), "{expected:?} missing:\n{screen}");
    }
    assert!(s.repo.root.join("branchyard.toml").exists());
    assert!(s.file(".codex/hooks.json").exists());
    let shown = s.json(&["workspace", "show", "--json"]);
    assert_eq!(shown["trust"], "untrusted", "{shown}");
}
