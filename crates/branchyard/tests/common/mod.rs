//! Temporary repositories and the fake ACP agent, for hermetic engine tests.
//! Requires `git` and `sh`; runs no real harness and makes no network calls.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Once, OnceLock};

use branchyard::{Activity, Event, RecordedEvent, TaskBuilder, TaskOptions, Yard};

static COUNTER: AtomicU64 = AtomicU64::new(0);
static HERMETIC: Once = Once::new();

/// The `fake-acp-agent` binary from branchyard-runtime, built once per test
/// binary into this build's target directory.
pub fn fake_agent() -> &'static Path {
    static AGENT: OnceLock<PathBuf> = OnceLock::new();
    AGENT.get_or_init(|| built("branchyard-runtime", "fake-acp-agent"))
}

/// The `branchyard-bridge` binary, built once per test binary.
pub fn bridge_binary() -> &'static Path {
    static BRIDGE: OnceLock<PathBuf> = OnceLock::new();
    BRIDGE.get_or_init(|| built("branchyard-bridge", "branchyard-bridge"))
}

/// Build `bin` of `package` into this build's target directory. Cargo
/// exposes a binary's path only to its own package's tests, so it is built
/// here.
fn built(package: &str, bin: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    // target/<profile>/deps/<test binary>
    let profile_dir = exe.parent().and_then(Path::parent).unwrap().to_path_buf();
    let target_dir = profile_dir.parent().unwrap();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .args(["build", "--quiet", "--offline", "--manifest-path"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
        .args(["-p", package, "--bin", bin])
        .env("CARGO_TARGET_DIR", target_dir);
    match profile_dir.file_name().and_then(|n| n.to_str()) {
        Some("debug") => {}
        Some("release") => {
            command.arg("--release");
        }
        Some(other) => {
            command.args(["--profile", other]);
        }
        None => panic!("unexpected test binary location {}", exe.display()),
    }
    let status = command
        .status()
        .unwrap_or_else(|e| panic!("run cargo to build {bin}: {e}"));
    assert!(status.success(), "building {bin} failed");
    let path = profile_dir.join(bin);
    assert!(path.is_file(), "{} was not built", path.display());
    path
}

/// A temporary directory holding a repository at `repo/` with one commit
/// on `main`. Removed on drop.
pub struct Fixture {
    pub dir: PathBuf,
    pub root: PathBuf,
    pub yard: Yard,
}

impl Fixture {
    pub fn new() -> Fixture {
        HERMETIC.call_once(|| {
            // Keep the host's git configuration out, and set a nested-session
            // marker and a Branchyard variable the engine must strip.
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
            std::env::set_var("CLAUDECODE", "1");
            std::env::set_var("BY_TEST_VISIBLE", "yes");
            std::env::set_var("BRANCHYARD_REMOTE", "http://127.0.0.1:9");
        });
        // Build the agent before any test body runs. The first build can
        // recompile dependencies (a single-package build unifies features
        // differently from `cargo test --workspace`), which must not count
        // against a test's own timing.
        fake_agent();
        let dir = std::env::temp_dir().join(format!(
            "branchyard-sdk-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let root = dir.join("repo");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@localhost"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        let yard = Yard::open(&root).unwrap();
        Fixture { dir, root, yard }
    }

    pub fn git(&self, args: &[&str]) -> String {
        git(&self.root, args)
    }

    /// Options that run the fake agent through an ACP profile.
    pub fn options(&self) -> TaskOptions {
        TaskOptions {
            harness: Some("gemini-cli".into()),
            command: Some(vec![fake_agent().display().to_string()]),
            ..TaskOptions::default()
        }
    }

    pub fn task(&self, prompt: &str) -> TaskBuilder {
        self.yard.task(prompt).options(self.options())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The text the harness sent, joined.
pub fn text(events: &[RecordedEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Harness(Event::MessageDelta { text, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Change a branch's stored record in `.branchyard/state.db` directly,
/// standing in for state the engine did not write.
pub fn edit_record(root: &Path, name: &str, edit: impl FnOnce(&mut serde_json::Value)) {
    let db = rusqlite::Connection::open(root.join(".branchyard/state.db")).unwrap();
    let text: String = db
        .query_row("SELECT record FROM branches WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .unwrap();
    let mut record: serde_json::Value = serde_json::from_str(&text).unwrap();
    edit(&mut record);
    db.execute(
        "UPDATE branches SET record = ?2 WHERE name = ?1",
        [name, &record.to_string()],
    )
    .unwrap();
}

/// A branch's stored record, as JSON.
pub fn stored_record(root: &Path, name: &str) -> serde_json::Value {
    let db = rusqlite::Connection::open(root.join(".branchyard/state.db")).unwrap();
    let text: String = db
        .query_row("SELECT record FROM branches WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .unwrap();
    serde_json::from_str(&text).unwrap()
}
