//! `by sync` through the built binary, to a `file://` remote: a branch made
//! by a turn of the fake ACP agent synced, shown in `by sync status` and
//! `by sync ls`, pulled into a clone on "another machine", held, removed,
//! collected and scrubbed; then the same through an encrypted remote.
//! Hermetic: no network. Requires `git` and `sh`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use branchyard_testkit::fake_agent;
use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct World {
    dir: PathBuf,
    config: PathBuf,
}

impl World {
    fn new(sync: &str) -> World {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-sync-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let config = dir.join("config.toml");
        fs::write(
            &config,
            format!(
                "[sync]\nremote = \"file://{}\"\ninterval = \"5m\"\n{sync}",
                dir.join("bucket").display()
            ),
        )
        .unwrap();
        World { dir, config }
    }

    fn command(&self, root: &Path, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env("BRANCHYARD_USER_CONFIG", &self.config)
            .env("BRANCHYARD_HOME", self.dir.join("home"))
            .env("PAGER", "cat");
        for var in [
            "BRANCHYARD_DELEGATION",
            "BRANCHYARD_BRANCH",
            "BRANCHYARD_ROOT",
            "BRANCHYARD_BY",
            "BRANCHYARD_REMOTE",
            "BRANCHYARD_SYNC_PASSPHRASE",
        ] {
            command.env_remove(var);
        }
        command
    }

    fn git(&self, root: &Path, args: &[&str]) -> String {
        let out = self.command(root, "git").args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn by(&self, root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = self.command(root, env!("CARGO_BIN_EXE_by"));
        for (k, v) in env {
            command.env(k, v);
        }
        command.args(args).output().unwrap()
    }

    fn json(&self, root: &Path, args: &[&str], env: &[(&str, &str)]) -> Value {
        let out = self.by(root, args, env);
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            stdout(&out),
            stderr(&out)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// A repository with one commit and a branch `hello` made by a turn.
    fn machine(&self, env: &[(&str, &str)]) -> PathBuf {
        let root = self.dir.join("a");
        fs::create_dir_all(&root).unwrap();
        self.git(&root, &["init", "-q", "-b", "main"]);
        self.git(&root, &["config", "user.name", "Test"]);
        self.git(&root, &["config", "user.email", "test@localhost"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        self.git(&root, &["add", "."]);
        self.git(&root, &["commit", "-q", "-m", "initial"]);
        let agent = fake_agent!().display().to_string();
        let out = self.by(
            &root,
            &[
                "run",
                "WRITE hello.txt=hi",
                "--name",
                "hello",
                "--harness",
                "gemini-cli",
                "--command",
                &agent,
            ],
            env,
        );
        assert!(out.status.success(), "{}", stderr(&out));
        root
    }

    /// A clone on "another machine".
    fn clone(&self, from: &Path) -> PathBuf {
        let root = self.dir.join("b");
        self.git(
            &self.dir,
            &["clone", "-q", &from.display().to_string(), "b"],
        );
        self.git(&root, &["config", "user.name", "Test"]);
        self.git(&root, &["config", "user.email", "test@localhost"]);
        root
    }
}

impl Drop for World {
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
fn by_sync_pushes_pulls_holds_and_collects_through_a_directory() {
    let w = World::new("");
    let a = w.machine(&[]);
    let git_branch = w.json(&a, &["ls", "--json"], &[])[0]["git_branch"]
        .as_str()
        .unwrap_or("hello")
        .to_owned();
    let head = w.git(&a, &["rev-parse", &format!("refs/heads/{git_branch}")]);

    let synced = w.json(&a, &["sync", "--json"], &[]);
    let report = &synced["synced"][0];
    let task = report["task"].as_str().unwrap().to_owned();
    assert!(task.ends_with(".hello"), "{task}");
    assert_eq!(report["swapped"], true);
    assert!(w.dir.join("bucket/keyring.json").is_file());
    assert!(a.join(".branchyard/sync.db").is_file());

    // Status: synced, nothing queued; the remote's listing.
    let status = w.json(&a, &["sync", "status", "--json"], &[]);
    assert_eq!(status["tasks"][0]["state"], "synced");
    assert!(status["pending"].as_array().unwrap().is_empty());
    assert!(status["counters"]["swaps"].as_u64().unwrap() >= 1);
    let text = stdout(&w.by(&a, &["sync", "status"], &[]));
    assert!(text.contains("synced") && text.contains("device"), "{text}");
    let ls = w.json(&a, &["sync", "ls", "--json"], &[]);
    assert_eq!(ls["tasks"][0]["task"], task.as_str());

    // Another machine, a clone without the branch, pulls it by name.
    let b = w.clone(&a);
    let pulled = w.json(&b, &["sync", "pull", "hello", "--json"], &[]);
    assert_eq!(pulled["task"], task.as_str());
    assert_eq!(w.git(&b, &["rev-parse", "refs/heads/hello"]), head);
    assert_eq!(w.git(&b, &["show", "hello:hello.txt"]), "hi");

    // A legal hold blocks removal; released, the task goes, and gc
    // reclaims nothing yet (its grace period).
    let out = w.by(&a, &["sync", "hold", "hello", "--reason", "audit 7"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = w.by(&a, &["sync", "rm", "hello"], &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("legal hold"), "{}", stderr(&out));
    w.json(&a, &["sync", "hold", "hello", "--release", "--json"], &[]);
    let removed = w.json(&a, &["sync", "rm", "hello", "--json"], &[]);
    assert_eq!(removed["removed"], true);
    let gc = w.json(&a, &["sync", "gc", "--json"], &[]);
    assert!(gc["deferred"].is_null(), "{gc}");
    assert!(gc["unreferenced"].as_u64().unwrap() >= 1);
    assert!(gc["deleted"].as_array().unwrap().is_empty());
    let scrub = w.json(&a, &["sync", "scrub", "--sample", "1000", "--json"], &[]);
    assert!(scrub["corrupt"].as_array().unwrap().is_empty(), "{scrub}");

    // Sync is local: --remote is refused, and so is a missing [sync].
    let out = w.by(&a, &["--remote", "http://127.0.0.1:9", "sync"], &[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("without --remote"),
        "{}",
        stderr(&out)
    );
    fs::write(&w.config, "").unwrap();
    let out = w.by(&a, &["sync"], &[]);
    assert!(
        stderr(&out).contains("no [sync] remote"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn by_sync_through_an_encrypted_remote() {
    let w = World::new("encrypt = \"passphrase\"\n");
    let pass = [("BRANCHYARD_SYNC_PASSPHRASE", "correct horse battery staple")];
    let a = w.machine(&pass);
    let out = w.by(&a, &["sync"], &[]);
    assert!(!out.status.success(), "no passphrase: refused");
    assert!(
        stderr(&out).contains("BRANCHYARD_SYNC_PASSPHRASE"),
        "{}",
        stderr(&out)
    );
    let synced = w.json(&a, &["sync", "--json"], &pass);
    assert_eq!(synced["synced"][0]["swapped"], true);
    // Nothing in the bucket names the branch or holds its text.
    for entry in walk(&w.dir.join("bucket")) {
        let name = entry.display().to_string();
        assert!(!name.contains("hello"), "{name}");
        if !name.ends_with("keyring.json") {
            let bytes = fs::read(&entry).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("hello"), "{name}");
        }
    }
    let b = w.clone(&a);
    let wrong = [("BRANCHYARD_SYNC_PASSPHRASE", "the wrong passphrase")];
    let out = w.by(&b, &["sync", "pull", "hello"], &wrong);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("does not open"), "{}", stderr(&out));
    w.json(&b, &["sync", "pull", "hello", "--json"], &pass);
    assert_eq!(w.git(&b, &["show", "hello:hello.txt"]), "hi");
    let status = w.json(&b, &["sync", "status", "--json"], &pass);
    assert_eq!(status["encrypted"], true);
}

/// A folder task (a repository of its own under BRANCHYARD_HOME, not in
/// any repository) syncs beside the repository's branches, under its ULID.
#[test]
fn by_sync_carries_a_folder_task_beside_the_branches() {
    let w = World::new("");
    let a = w.machine(&[]);
    let folder = w.dir.join("folder");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("notes.txt"), "draft\n").unwrap();
    let agent = fake_agent!().display().to_string();
    let out = w.by(
        &w.dir,
        &[
            "task",
            "new",
            "--folder",
            folder.to_str().unwrap(),
            "WRITE notes.txt=final",
            "--name",
            "edit",
            "--harness",
            "gemini-cli",
            "--command",
            &agent,
            "--yes",
        ],
        &[],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let listed = w.json(&w.dir, &["task", "ls", "--json"], &[]);
    let id = listed
        .as_array()
        .and_then(|tasks| tasks.first())
        .or_else(|| listed["tasks"].as_array().and_then(|t| t.first()))
        .and_then(|t| t["id"].as_str().or_else(|| t["task"]["id"].as_str()))
        .unwrap_or_else(|| panic!("no task id in {listed}"))
        .to_owned();

    // From the repository: its branch and the folder task, each its own task.
    let synced = w.json(&a, &["sync", "--json"], &[]);
    let tasks: Vec<String> = synced["synced"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["task"].as_str().unwrap().to_owned())
        .collect();
    assert!(tasks.iter().any(|t| t.ends_with(".hello")), "{tasks:?}");
    assert!(tasks.contains(&id), "{id} not in {tasks:?}");
    let remote = w.json(&a, &["sync", "ls", "--json"], &[]);
    let listed: Vec<&str> = remote["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["task"].as_str().unwrap())
        .collect();
    assert!(listed.contains(&id.as_str()), "{listed:?}");
    // The task syncs by its ID too, and the folder was left alone.
    let one = w.json(&a, &["sync", &id, "--json"], &[]);
    assert_eq!(one["synced"][0]["task"], id.as_str(), "{one}");
    assert_eq!(
        fs::read_to_string(folder.join("notes.txt")).unwrap(),
        "draft\n"
    );
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}
