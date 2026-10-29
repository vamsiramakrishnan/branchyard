//! Hermetic allocation recovery checks. No agent or provider is started.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report

use std::fs;
use std::path::Path;

use branchyard::tasks::{self, NewTask, TaskFiles, TaskReservation};
use branchyard::Yard;
use branchyard_workspace::Git;

fn hermetic() {
    static ENV: std::sync::Once = std::sync::Once::new();
    ENV.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    });
}

fn git(root: &Path, args: &[&str]) -> String {
    Git::new(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .run()
        .unwrap()
        .trim()
        .to_owned()
}

#[test]
fn reserve_no_files_before_effects_and_replay_after_restart() {
    hermetic();
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("managed");
    let reservation = tasks::reserve(&NewTask {
        prompt: "Research the supplied notes".into(),
        by: Some("subject:alice".into()),
        home: Some(home.clone()),
        ..NewTask::default()
    })
    .unwrap();
    assert!(!home.exists());
    assert!(reservation.inspect().unwrap().is_none());
    let queued = serde_json::to_vec(&reservation).unwrap();
    let replayed: TaskReservation = serde_json::from_slice(&queued).unwrap();
    assert_eq!(reservation, replayed);
    let (first, yard) = tasks::initialize_reserved(&replayed).unwrap();
    assert_eq!(first.id, reservation.id());
    assert_eq!(first.by, "subject:alice");
    assert_eq!(first.files, TaskFiles::NoFiles);
    assert_eq!(yard.root(), reservation.work_root());
    let main = git(yard.root(), &["rev-parse", "main"]);
    git(yard.root(), &["branch", "by/existing-session", "main"]);
    drop(yard);
    assert_eq!(reservation.inspect().unwrap(), Some(first.clone()));
    let (again, yard) = tasks::initialize_reserved(&replayed).unwrap();
    assert_eq!(again, first);
    assert_eq!(git(yard.root(), &["rev-parse", "main"]), main);
    assert_eq!(
        git(yard.root(), &["rev-parse", "by/existing-session"]),
        main
    );
    assert_eq!(fs::read_dir(home.join("tasks")).unwrap().count(), 1);
}

#[test]
fn resume_partial_git_initialization_at_the_reserved_location() {
    hermetic();
    let scratch = tempfile::tempdir().unwrap();
    let reservation = tasks::reserve(&NewTask {
        prompt: "Draft the report".into(),
        home: Some(scratch.path().join("managed")),
        ..NewTask::default()
    })
    .unwrap();
    let work = reservation.work_root();
    let dir = work.parent().unwrap();
    fs::create_dir_all(dir).unwrap();
    git(
        dir,
        &[
            "init",
            "--quiet",
            "--initial-branch=main",
            "--separate-git-dir",
            dir.join("git").to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    assert!(reservation.inspect().unwrap().is_none());
    let (task, yard) = tasks::initialize_reserved(&reservation).unwrap();
    assert_eq!(task.id, reservation.id());
    assert!(tasks::view(&yard, reservation.id())
        .unwrap()
        .attempts
        .is_empty());
}

#[test]
fn resume_after_first_commit_keeps_original_folder_snapshot() {
    hermetic();
    let scratch = tempfile::tempdir().unwrap();
    let folder = scratch.path().join("sources");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("notes.txt"), "original").unwrap();
    let reservation = tasks::reserve(&NewTask {
        prompt: "Research notes".into(),
        folder: Some(folder.clone()),
        home: Some(scratch.path().join("managed")),
        ..NewTask::default()
    })
    .unwrap();
    let (task, yard) = tasks::initialize_reserved(&reservation).unwrap();
    let first = git(yard.root(), &["rev-parse", "main"]);
    // Crash before publication: Git already has the original snapshot, but
    // the final task record was not durably published.
    fs::remove_file(
        yard.root()
            .join(".branchyard/tasks")
            .join(&task.id)
            .join("task.json"),
    )
    .unwrap();
    drop(yard);
    fs::write(folder.join("notes.txt"), "changed outside").unwrap();
    let (recovered, yard) = tasks::initialize_reserved(&reservation).unwrap();
    assert_eq!(recovered, task);
    assert_eq!(git(yard.root(), &["rev-parse", "main"]), first);
    assert_eq!(git(yard.root(), &["show", "main:notes.txt"]), "original");
    assert_eq!(
        fs::read_to_string(folder.join("notes.txt")).unwrap(),
        "changed outside"
    );
}

#[test]
fn reject_conflicting_or_malformed_reservations_before_overwriting() {
    hermetic();
    let scratch = tempfile::tempdir().unwrap();
    let reservation = tasks::reserve(&NewTask {
        prompt: "Original task".into(),
        home: Some(scratch.path().join("managed")),
        ..NewTask::default()
    })
    .unwrap();
    let (original, yard) = tasks::initialize_reserved(&reservation).unwrap();
    drop(yard);
    let mut value = serde_json::to_value(&reservation).unwrap();
    value["new"]["prompt"] = "Different task".into();
    let conflict: TaskReservation = serde_json::from_value(value.clone()).unwrap();
    assert!(tasks::initialize_reserved(&conflict).is_err());
    assert!(conflict.inspect().is_err());
    assert_eq!(reservation.inspect().unwrap(), Some(original));
    value["id"] = "../../escape".into();
    let malformed: TaskReservation = serde_json::from_value(value).unwrap();
    assert!(tasks::initialize_reserved(&malformed).is_err());
    assert!(malformed.inspect().is_err());
    assert!(!scratch.path().join("escape").exists());
}

#[test]
fn repository_task_reservation_is_idempotent_and_never_starts_an_attempt() {
    hermetic();
    let scratch = tempfile::tempdir().unwrap();
    git(
        scratch.path(),
        &["init", "--quiet", "--initial-branch=main"],
    );
    git(scratch.path(), &["config", "user.name", "Test"]);
    git(scratch.path(), &["config", "user.email", "test@localhost"]);
    git(
        scratch.path(),
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "initial",
        ],
    );
    let yard = Yard::open(scratch.path()).unwrap();
    let reservation =
        tasks::reserve_repository("Write documentation", "subject:alice", "read source").unwrap();
    assert!(tasks::list(&yard).unwrap().is_empty());
    let first = tasks::initialize_repository(&yard, &reservation).unwrap();
    assert_eq!(first, reservation);
    assert_eq!(
        tasks::initialize_repository(&yard, &reservation).unwrap(),
        first
    );
    let view = tasks::view(&yard, &first.id).unwrap();
    assert!(view.attempts.is_empty());
    assert_eq!(view.task.files, TaskFiles::Repository);
    let mut conflict = reservation.clone();
    conflict.asked = "Different task".into();
    assert!(tasks::initialize_repository(&yard, &conflict).is_err());
    conflict.id = "../escape".into();
    assert!(tasks::initialize_repository(&yard, &conflict).is_err());
}
