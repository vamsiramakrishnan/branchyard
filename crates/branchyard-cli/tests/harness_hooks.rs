//! `by hook --harness ID` and `by statusline --harness ID` through the built
//! `by`, against one recorded payload per harness and event in
//! `tests/fixtures/hooks/<id>/`: `<name>.json` is the harness's stdin
//! (`@CWD@` stands for the repository), `<name>.out` the exact stdout (empty
//! where the harness drops what that event prints). Most payloads are
//! recorded from the real CLI; Copilot CLI's, Droid's, Cursor's,
//! Antigravity's and Kiro's are written from their documentation.
//! Hermetic: no real harness, no network. Requires `git` and `sh`.

#![allow(clippy::panic, clippy::unwrap_used, clippy::unwrap_in_result)] // tests: a panic is the failure report
use std::path::{Path, PathBuf};
use std::process::Output;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hooks")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `fixture`'s payload with the repository's root in place of `@CWD@`.
fn payload(repo: &branchyard_testkit::Repo, fixture: &Path) -> String {
    let root = serde_json::to_string(&repo.root.display().to_string()).unwrap();
    let root = &root[1..root.len() - 1];
    std::fs::read_to_string(fixture)
        .unwrap()
        .replace("@CWD@", root)
}

/// A repository with one branch finished and ready to integrate.
fn yard_with_a_ready_branch() -> branchyard_testkit::Repo {
    let repo = branchyard_testkit::repo!();
    let out = repo.by_agent(&["run", "WRITE hello.txt=hi", "--name", "hello"]);
    assert!(out.status.success(), "{out:?}");
    repo
}

fn run(repo: &branchyard_testkit::Repo, args: &[&str], input: &str) -> Output {
    let out = repo.by_with_stdin(args, input);
    assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
    out
}

#[test]
fn each_harness_hears_in_its_own_shape_once() {
    let repo = yard_with_a_ready_branch();
    let hooks = repo.root.join(".branchyard/hooks");
    let mut harnesses: Vec<PathBuf> = std::fs::read_dir(fixtures())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    harnesses.sort();
    assert!(harnesses.len() >= 12, "{harnesses:?}");
    for dir in harnesses {
        let id = dir.file_name().unwrap().to_str().unwrap().to_owned();
        for event in ["session-start", "user-prompt-submit"] {
            let input = payload(&repo, &dir.join(format!("{event}.json")));
            let expected = std::fs::read_to_string(dir.join(format!("{event}.out"))).unwrap();
            // Each event as a session's first call.
            if hooks.exists() {
                std::fs::remove_dir_all(&hooks).unwrap();
            }
            let args = ["hook", "--harness", &id, event];
            let out = run(&repo, &args, &input);
            assert_eq!(stdout(&out), expected, "{id} {event}");
            assert!(out.stderr.is_empty(), "{id} {event}: {out:?}");
            // A harness that drops this event's output leaves the cursor
            // alone, so the news waits for an event it hears.
            assert_eq!(hooks.exists(), !expected.is_empty(), "{id} {event}");
            // Nothing new the second time.
            assert_eq!(stdout(&run(&repo, &args, &input)), "", "{id} {event}");
        }
        let status = dir.join("statusline.json");
        if status.exists() {
            let expected = std::fs::read_to_string(dir.join("statusline.out")).unwrap();
            let out = run(
                &repo,
                &["statusline", "--harness", &id],
                &payload(&repo, &status),
            );
            assert_eq!(stdout(&out), expected, "{id} statusline");
        }
    }
}

#[test]
fn no_flag_is_claude_code_byte_for_byte() {
    let repo = yard_with_a_ready_branch();
    let input = |session: &str| {
        serde_json::json!({"session_id": session, "cwd": repo.root, "hook_event_name": "SessionStart"})
            .to_string()
    };
    let plain = stdout(&run(&repo, &["hook", "session-start"], &input("a")));
    let named = stdout(&run(
        &repo,
        &["hook", "--harness", "claude-code", "session-start"],
        &input("b"),
    ));
    assert!(!plain.is_empty());
    assert_eq!(plain, named);
    let line = serde_json::json!({"workspace": {"current_dir": repo.root}}).to_string();
    assert_eq!(
        stdout(&run(&repo, &["statusline"], &line)),
        stdout(&run(
            &repo,
            &["statusline", "--harness", "claude-code"],
            &line
        )),
    );
}

#[test]
fn an_unknown_harness_prints_nothing_and_a_note() {
    let repo = yard_with_a_ready_branch();
    let input = serde_json::json!({"session_id": "u", "cwd": repo.root}).to_string();
    for args in [
        &["hook", "--harness", "nope", "session-start"][..],
        &["statusline", "--harness", "nope"],
    ] {
        let out = run(&repo, args, &input);
        assert_eq!(stdout(&out), "", "{args:?}");
        let note = String::from_utf8_lossy(&out.stderr);
        assert_eq!(note.lines().count(), 1, "{note}");
        assert!(note.contains("unknown harness \"nope\""), "{note}");
    }
    assert!(!repo.root.join(".branchyard/hooks").exists());
}

#[test]
fn a_status_line_without_a_directory_reads_the_process_one() {
    let repo = yard_with_a_ready_branch();
    for id in [
        "generic",
        "cursor",
        "antigravity",
        "github-copilot",
        "droid",
    ] {
        let out = run(&repo, &["statusline", "--harness", id], "{}");
        assert_eq!(stdout(&out), "by: 1 ready\n", "{id}");
    }
}
