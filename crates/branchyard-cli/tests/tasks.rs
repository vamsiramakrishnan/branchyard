//! `by task` end to end with the built `by` and the fake ACP agent
//! (docs/task-repos.md): a run is a task whose record is committed beside
//! each checkpoint and never merged; a folder task leaves the folder alone
//! until `by task accept`, refuses to overwrite a file changed outside the
//! task, and is removed without touching it; rewind and fork by task.
//! Hermetic: `BRANCHYARD_HOME` is the test's own directory.

#![allow(clippy::let_underscore_must_use, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use branchyard_testkit::fake_agent;
use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A repository at `repo/`, a folder to grant at `folder/`, and
/// Branchyard's per-user directory at `home/`.
struct Fixture {
    dir: PathBuf,
    root: PathBuf,
    folder: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-tasks-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        fs::create_dir_all(dir.join("folder")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let f = Fixture {
            root: dir.join("repo"),
            folder: dir.join("folder"),
            dir,
        };
        f.git(&["init", "-q", "-b", "main"]);
        f.git(&["config", "user.name", "Test"]);
        f.git(&["config", "user.email", "test@localhost"]);
        fs::write(f.root.join("a.txt"), "one\n").unwrap();
        f.git(&["add", "."]);
        f.git(&["commit", "-q", "-m", "initial"]);
        f
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>, cwd: &Path) -> Command {
        let mut command = Command::new(program);
        branchyard_testkit::hermetic(&mut command)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env("BRANCHYARD_HOME", self.dir.join("home"))
            .env(
                "BRANCHYARD_USER_CONFIG",
                "/nonexistent/branchyard-config.toml",
            )
            .env("PAGER", "cat");
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.command("git", &self.root).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn by_in(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_by"), cwd)
            .args(args)
            .output()
            .unwrap()
    }

    fn by(&self, args: &[&str]) -> Output {
        self.by_in(&self.root, args)
    }

    /// `by <args>` with the fake agent as the gemini-cli harness, allowed
    /// everything.
    fn by_agent(&self, cwd: &Path, args: &[&str]) -> Output {
        let agent = fake_agent!().display().to_string();
        let mut all: Vec<&str> = args.to_vec();
        all.extend(["--command", &agent, "--yes"]);
        // A send keeps its branch's harness, and a fork its parent's.
        if args[0] != "send" && args.get(1) != Some(&"fork") {
            all.extend(["--harness", "gemini-cli"]);
        }
        self.by_in(cwd, &all)
    }

    fn json(&self, cwd: &Path, args: &[&str]) -> Value {
        let out = self.by_in(cwd, args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

impl Drop for Fixture {
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

fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(out),
        stderr(out)
    );
}

#[test]
fn a_run_is_a_task_and_its_record_stays_out_of_the_merge() {
    let f = Fixture::new();
    ok(&f.by_agent(&f.root, &["run", "WRITE hello.txt=hi", "--name", "hello"]));
    let list = f.json(&f.root, &["task", "ls", "--json"]);
    let tasks = list.as_array().unwrap();
    assert_eq!(tasks.len(), 1, "{list}");
    let task = &tasks[0];
    assert_eq!(task["origin"], "run");
    assert_eq!(task["files"]["kind"], "repository");
    assert_eq!(task["attempts"][0]["name"], "hello");
    assert_eq!(task["attempts"][0]["conversation"][0], "1.jsonl");
    let id = task["id"].as_str().unwrap().to_owned();
    // `by task show` by an attempt's name or a prefix of the ID.
    let shown = f.json(&f.root, &["task", "show", &id[..6], "--json"]);
    assert_eq!(shown["id"], id.as_str());
    let text = stdout(&f.by(&["task", "show", "hello"]));
    assert!(text.contains(&format!("task       {id}")), "{text}");
    assert!(text.contains("conversation: 1.jsonl"), "{text}");
    let table = stdout(&f.by(&["task", "ls"]));
    assert!(table.starts_with("TASK"), "{table}");
    assert!(table.contains("1 (1 ready)"), "{table}");
    // `by show` names the task.
    let show = f.json(&f.root, &["show", "hello", "--json"]);
    assert_eq!(show["task"]["id"], id.as_str());
    assert_eq!(show["task"]["attempt"], 1);
    assert!(stdout(&f.by(&["show", "hello"])).contains(&format!("{id} (attempt 1 of 1)")));
    // The record is committed beside the checkpoint, not on the branch.
    let record = f.git(&[
        "ls-tree",
        "-r",
        "--name-only",
        "refs/branchyard/hello/1/record-1",
    ]);
    assert!(record.contains(".task/conversation/1.jsonl"), "{record}");
    assert!(record.contains("hello.txt"), "{record}");
    let diff = stdout(&f.by(&["diff", "hello"]));
    assert!(
        diff.contains("hello.txt") && !diff.contains(".task"),
        "{diff}"
    );
    ok(&f.by(&["task", "accept", &id]));
    let main = f.git(&["ls-tree", "-r", "--name-only", "main"]);
    assert_eq!(main, "a.txt\nhello.txt\n");
    assert_eq!(f.git(&["log", "--format=%H", "main", "--", ".task"]), "");
}

#[test]
fn a_folder_task_writes_the_folder_only_on_accept_and_never_over_an_outside_edit() {
    let f = Fixture::new();
    fs::write(f.folder.join("notes.txt"), "draft\n").unwrap();
    fs::write(f.folder.join("keep.txt"), "mine\n").unwrap();
    // Not inside any repository.
    let out = f.by_agent(
        &f.dir,
        &[
            "task",
            "new",
            "--folder",
            f.folder.to_str().unwrap(),
            "SH printf 'final\\n' > notes.txt\nSH printf 'added\\n' > extra.txt",
            "--name",
            "edit",
        ],
    );
    ok(&out);
    assert!(
        stderr(&out).contains("changes only when you accept"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        fs::read_to_string(f.folder.join("notes.txt")).unwrap(),
        "draft\n"
    );
    assert!(!f.folder.join("extra.txt").exists());
    assert_eq!(fs::read_dir(&f.folder).unwrap().count(), 2);
    let list = f.json(&f.dir, &["task", "ls", "--json"]);
    let task = &list.as_array().unwrap()[0];
    assert_eq!(task["files"]["kind"], "folder");
    let id = task["id"].as_str().unwrap().to_owned();
    assert!(Path::new(task["repository"].as_str().unwrap())
        .starts_with(f.dir.join("home").join("tasks").join(&id)));
    // An edit outside the task refuses the accept, naming it.
    fs::write(f.folder.join("notes.txt"), "my own edit\n").unwrap();
    let refused = f.by_in(&f.dir, &["task", "accept", &id]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("notes.txt: changed in the folder since the task began"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        fs::read_to_string(f.folder.join("notes.txt")).unwrap(),
        "my own edit\n"
    );
    assert!(!f.folder.join("extra.txt").exists());
    // Put back, the accept writes exactly the attempt's change.
    fs::write(f.folder.join("notes.txt"), "draft\n").unwrap();
    let accepted = f.json(&f.dir, &["task", "accept", &id, "--json"]);
    assert_eq!(accepted["attempt"], "edit");
    assert_eq!(
        accepted["written"],
        serde_json::json!(["extra.txt", "notes.txt"])
    );
    assert_eq!(
        fs::read_to_string(f.folder.join("notes.txt")).unwrap(),
        "final\n"
    );
    assert_eq!(
        fs::read_to_string(f.folder.join("extra.txt")).unwrap(),
        "added\n"
    );
    assert_eq!(
        fs::read_to_string(f.folder.join("keep.txt")).unwrap(),
        "mine\n"
    );
    assert!(!f.folder.join(".task").exists() && !f.folder.join(".git").exists());
    // Removing the task leaves the folder as it is.
    ok(&f.by_in(&f.dir, &["task", "rm", &id, "--yes"]));
    assert_eq!(
        f.json(&f.dir, &["task", "ls", "--json"]),
        serde_json::json!([])
    );
    assert_eq!(fs::read_dir(&f.folder).unwrap().count(), 3);
}

#[test]
fn a_task_is_rewound_and_forked_by_its_id() {
    let f = Fixture::new();
    ok(&f.by_agent(&f.root, &["run", "WRITE s.txt=one", "--name", "steps"]));
    ok(&f.by_agent(&f.root, &["send", "steps", "WRITE s.txt=two"]));
    let id = f.json(&f.root, &["task", "show", "steps", "--json"])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(&f.by(&["task", "rewind", &id, "--to", "1", "--yes"]));
    let worktree = f.root.join(".branchyard/worktrees/steps");
    assert_eq!(fs::read_to_string(worktree.join("s.txt")).unwrap(), "one\n");
    let shown = f.json(&f.root, &["task", "show", &id, "--json"]);
    assert_eq!(
        shown["attempts"][0]["conversation"],
        serde_json::json!(["1.jsonl"])
    );
    ok(&f.by_agent(
        &f.root,
        &[
            "task",
            "fork",
            &id,
            "WRITE t.txt=fork",
            "--at",
            "1",
            "--name",
            "steps-b",
        ],
    ));
    let shown = f.json(&f.root, &["task", "show", &id, "--json"]);
    let attempts = shown["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1]["name"], "steps-b");
    assert_eq!(
        attempts[1]["conversation"],
        serde_json::json!(["1.jsonl", "2.jsonl"])
    );
    // With two attempts, the one to accept must be named.
    let ambiguous = f.by(&["task", "accept", &id]);
    assert!(
        stderr(&ambiguous).contains("name one with --attempt"),
        "{}",
        stderr(&ambiguous)
    );
    let path = stdout(&f.by(&["task", "open", &id, "--attempt", "steps-b", "--print"]));
    assert_eq!(
        path.trim(),
        f.root
            .join(".branchyard/worktrees/steps-b")
            .display()
            .to_string()
    );
}
