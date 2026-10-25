//! Failures `by` survives are seen, and a reader or a disk that goes away
//! does not lose a turn that completed; an unreadable configuration file is
//! left as it is. The built `by` against a temporary repository and the fake
//! ACP agent. Hermetic: no real harness, no network. Requires `git` and
//! `sh`; the tests that write to `/dev/full` need Linux.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs::{self, File, OpenOptions};
use std::process::{Output, Stdio};

use branchyard_testkit::fake_agent;
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

    /// `by <args>` with the fake agent, standard output going to `stdout`
    /// and standard error captured.
    fn run_into(&self, name: &str, stdout: Stdio) -> Output {
        let agent = fake_agent(self.by_path()).display().to_string();
        self.command(self.by_path())
            .args([
                "run",
                "WRITE hello.txt=hi",
                "--name",
                name,
                "--command",
                &agent,
                "--harness",
                "gemini-cli",
            ])
            .stdout(stdout)
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn branch(&self, name: &str) -> Value {
        self.json(&["show", name, "--json"])
    }
}

/// A file that fails every write with "No space left on device", or `None`
/// where there is no such device.
fn dev_full() -> Option<File> {
    let full = OpenOptions::new().write(true).open("/dev/full").ok();
    if full.is_none() {
        eprintln!("skipped: this system has no /dev/full");
    }
    full
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// `by --help` printed to a standard output that fails: the survived
/// failure is on standard error as `by: warning: ...`. It was dropped
/// before: no subscriber was installed on this path, so `RUST_LOG=trace by
/// --help` printed nothing at all.
#[test]
fn a_message_that_cannot_be_printed_is_a_warning_on_stderr() {
    let Some(full) = dev_full() else { return };
    let repo = Repo::new();
    let out = repo
        .command(repo.by_path())
        .arg("--help")
        .stdout(full)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(
        text.contains("by: warning: print the help or usage message: No space left on device"),
        "{text}"
    );
    assert_eq!(
        text.lines().count(),
        1,
        "one line, not a log record: {text}"
    );

    // What the request asked to see: RUST_LOG=trace shows something.
    let full = dev_full().unwrap();
    let traced = repo
        .command(repo.by_path())
        .arg("--help")
        .env("RUST_LOG", "trace")
        .stdout(full)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!traced.stderr.is_empty());
}

/// `by --help | head -1`: the reader went away, which is its own choice and
/// not a warning.
#[test]
fn help_into_a_closed_pipe_is_quiet() {
    let repo = Repo::new();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = repo
        .command(repo.by_path())
        .arg("--help")
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stderr(&out), "");
}

/// A command whose output cannot be written carries on, and says so once
/// (not once per line): the branch is still finished and has its candidate.
#[test]
fn a_run_whose_output_cannot_be_written_finishes_and_warns_once() {
    let Some(full) = dev_full() else { return };
    let repo = Repo::new();
    let out = repo.run_into("full", Stdio::from(full));
    let text = stderr(&out);
    assert_ne!(out.status.code(), Some(101), "it panicked:\n{text}");
    let warnings: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("by: warning: write the console output"))
        .collect();
    assert_eq!(warnings.len(), 1, "{text}");
    assert!(warnings[0].contains("No space left on device"), "{text}");
    let branch = repo.branch("full");
    assert_eq!(branch["status"]["state"], "ready", "{branch}");
    assert_eq!(branch["candidate"]["files_changed"], 1);
}

/// `by run ... 2>&1 | head -1`: standard output and standard error are both
/// a pipe nobody reads. Printing to either used to panic, and a panic ended
/// the command before the turn was written down: the branch was left
/// `interrupted` with no turn and no candidate though its file was written.
#[test]
fn a_run_into_a_closed_pipe_keeps_its_completed_turn() {
    let repo = Repo::new();
    let agent = fake_agent(repo.by_path()).display().to_string();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let status = repo
        .command(repo.by_path())
        .args([
            "run",
            "WRITE hello.txt=hi",
            "--name",
            "piped",
            "--command",
            &agent,
            "--harness",
            "gemini-cli",
        ])
        .stdout(writer.try_clone().unwrap())
        .stderr(writer)
        .status()
        .unwrap();
    assert_ne!(status.code(), Some(101), "it panicked");
    assert!(status.success(), "{status}");
    let branch = repo.branch("piped");
    assert_eq!(branch["status"]["state"], "ready", "{branch}");
    assert_eq!(branch["turns"], 1, "{branch}");
    assert_eq!(branch["candidate"]["files_changed"], 1, "{branch}");
    assert!(repo
        .root
        .join(".branchyard/worktrees/piped/hello.txt")
        .is_file());
}

/// `by harnesses login X --api-key` with a user configuration that cannot
/// be read must not replace it, nor leave a key file behind. (`store_key`
/// used to take the unreadable text for an empty file and write one holding
/// only `[secrets]`; the startup load of the configuration stops this case
/// first, `crate::user_config` is the second check, and both must hold.)
#[test]
fn a_user_config_that_cannot_be_read_is_left_as_it_is() {
    let repo = Repo::new();
    let config = repo.dir.join("user/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    // Not UTF-8: reading it as text fails.
    let bytes: &[u8] = b"[defaults]\nharness = \"codex\"\n# caf\xe9 \xff\xfe\n";
    fs::write(&config, bytes).unwrap();
    let out = repo.by_with_stdin(
        &["harnesses", "login", "claude-code", "--api-key"],
        "not-a-real-key",
    );
    assert!(!out.status.success());
    let text = stderr(&out);
    assert!(text.contains(&config.display().to_string()), "{text}");
    assert_eq!(fs::read(&config).unwrap(), bytes, "byte for byte");
    assert!(
        !repo.dir.join("user/secrets").exists(),
        "no key file was written either"
    );
}

/// Unlike an unreadable file, a missing one is an empty configuration.
#[test]
fn a_missing_user_config_is_created_by_a_login() {
    let repo = Repo::new();
    let config = repo.dir.join("user/config.toml");
    assert!(!config.exists());
    let out = repo.by_with_stdin(
        &["harnesses", "login", "claude-code", "--api-key"],
        "not-a-real-key",
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.contains("[secrets]"), "{text}");
    assert!(text.contains("ANTHROPIC_API_KEY"), "{text}");
}
