//! A repository whose `.branchyard/state.db` has a write-ahead log with
//! another page size is refused before SQLite sees the pair
//! (RUSTSEC-2026-0459), through the built `by`: a command fails with the
//! message and a non-zero status, `by doctor` reports failed rows instead of
//! crashing, a hook that reads the store through `glance` says nothing on
//! standard output, names the refusal on standard error, exits 0 and leaves the
//! pair alone, and `by sync` refuses a forged
//! `.branchyard/sync.db` log. A real store with its real log, and a store a
//! killed writer left, still open. Hermetic; requires `git`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Leave a log beside `db` whose header says 512-byte pages.
fn plant(db: &Path) -> PathBuf {
    let wal = db.with_file_name(format!("{}-wal", db.file_name().unwrap().to_string_lossy()));
    let mut header = vec![0u8; 32];
    header[8..12].copy_from_slice(&512u32.to_be_bytes());
    fs::write(&wal, header).unwrap();
    wal
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn assert_refused(message: &str, wal: &Path) {
    assert!(message.contains(&wal.display().to_string()), "{message}");
    assert!(
        message.contains("512") && message.contains("4096"),
        "{message}"
    );
    assert!(message.contains("move both files away"), "{message}");
}

/// A repository `by` has been used in, with its store closed.
fn used() -> branchyard_testkit::Repo {
    let repo = branchyard_testkit::repo!();
    repo.ok(&["ls"]);
    repo
}

#[test]
fn commands_in_a_row_work_with_a_log_present() {
    let repo = used();
    repo.ok(&["ls"]);
    repo.ok(&["ls"]);
    let out = repo.by_agent(&["run", "WRITE hello.txt=hi", "--name", "hello"]);
    assert!(out.status.success(), "{out:?}");
    repo.ok(&["ls"]);
}

#[test]
fn a_command_refuses_a_forged_log_and_leaves_the_pair_alone() {
    let repo = used();
    let db = repo.root.join(".branchyard/state.db");
    let wal = plant(&db);
    let (db_bytes, wal_bytes) = (fs::read(&db).unwrap(), fs::read(&wal).unwrap());
    let out = repo.by(&["ls"]);
    assert!(!out.status.success(), "{out:?}");
    assert_refused(&text(&out.stderr), &wal);
    assert_eq!(fs::read(&db).unwrap(), db_bytes);
    assert_eq!(fs::read(&wal).unwrap(), wal_bytes);
    assert!(!repo.root.join(".branchyard/state.db-shm").exists());
}

#[test]
fn doctor_reports_failed_rows_for_a_forged_log() {
    let repo = used();
    let wal = plant(&repo.root.join(".branchyard/state.db"));
    let out = repo.by(&["doctor", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let schema = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "repository.schema")
        .unwrap();
    assert_eq!(schema["status"], "fail", "{schema}");
    assert_refused(schema["detail"].as_str().unwrap(), &wal);
    assert!(!repo.root.join(".branchyard/state.db-shm").exists());
}

#[test]
fn a_hook_reading_a_forged_store_prints_nothing_on_stdout_and_leaves_the_pair_alone() {
    let repo = used();
    let input = serde_json::json!({"session_id": "s", "cwd": repo.root}).to_string();
    let wal = plant(&repo.root.join(".branchyard/state.db"));
    let before = fs::read(&wal).unwrap();
    for event in ["session-start", "user-prompt-submit"] {
        let out = repo.by_with_stdin(&["hook", "--harness", "claude-code", event], &input);
        assert!(out.stdout.is_empty(), "{event}: {out:?}");
        assert_refused(&text(&out.stderr), &wal);
        assert_eq!(out.status.code(), Some(0), "{event}: {out:?}");
    }
    assert_eq!(fs::read(&wal).unwrap(), before);
    assert!(!repo.root.join(".branchyard/state.db-shm").exists());
}

#[test]
fn by_sync_refuses_a_forged_outbox_log() {
    let mut repo = used();
    let config = repo.dir.join("user-config.toml");
    fs::write(
        &config,
        format!(
            "[sync]\nremote = \"file://{}\"\ninterval = \"5m\"\n",
            repo.dir.join("bucket").display()
        ),
    )
    .unwrap();
    repo.set_env("BRANCHYARD_USER_CONFIG", &config.display().to_string());
    repo.ok(&["sync"]);
    let wal = plant(&repo.root.join(".branchyard/sync.db"));
    let out = repo.by(&["sync"]);
    assert!(!out.status.success(), "{out:?}");
    assert_refused(&text(&out.stderr), &wal);
}

#[test]
fn a_store_left_by_a_killed_writer_still_opens() {
    // A writer killed without closing leaves its log: copy a live pair
    // from a store that is still open into a repository of its own.
    let source = branchyard_testkit::repo!();
    let live = branchyard::Yard::open(&source.root).unwrap();
    let copy = branchyard_testkit::repo!();
    fs::create_dir_all(copy.root.join(".branchyard")).unwrap();
    for name in ["state.db", "state.db-wal"] {
        fs::copy(
            source.root.join(".branchyard").join(name),
            copy.root.join(".branchyard").join(name),
        )
        .unwrap();
    }
    assert!(
        copy.root
            .join(".branchyard/state.db-wal")
            .metadata()
            .unwrap()
            .len()
            >= 32,
        "a log was left"
    );
    copy.ok(&["ls"]);
    drop(live);
}

#[cfg(unix)]
#[test]
fn a_store_reached_through_links_opens_and_the_dotdot_trick_is_refused() {
    // `.branchyard/state.db` is a link into another directory: the built
    // `by` opens the file the link names (its log, removed when the last
    // connection closes, would be beside it, not beside the link).
    let repo = branchyard_testkit::repo!();
    let elsewhere = repo.dir.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::create_dir_all(repo.root.join(".branchyard")).unwrap();
    let link = repo.root.join(".branchyard/state.db");
    std::os::unix::fs::symlink(elsewhere.join("store.db"), &link).unwrap();
    repo.ok(&["ls"]);
    assert!(elsewhere.join("store.db").metadata().unwrap().len() > 0);
    assert!(!repo.root.join(".branchyard/state.db-wal").exists());
    repo.ok(&["ls"]);
    // The same file named through `nope/..`, with a forged log beside it:
    // SQLite would drop `nope/..` and open the forged pair; `by` refuses.
    fs::remove_file(&link).unwrap();
    let wal = plant(&elsewhere.join("store.db"));
    let tricky = format!("{}/nope/../store.db", elsewhere.display());
    std::os::unix::fs::symlink(&tricky, &link).unwrap();
    let out = repo.by(&["ls"]);
    assert!(!out.status.success(), "{out:?}");
    let message = text(&out.stderr);
    assert!(message.contains("cannot be resolved"), "{message}");
    // The forged log was not touched.
    assert_eq!(fs::read(&wal).unwrap().len(), 32);
}
