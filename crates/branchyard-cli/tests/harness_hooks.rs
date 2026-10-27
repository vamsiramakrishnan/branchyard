//! `by hook --harness ID` and `by statusline --harness ID` through the built
//! `by`, against one recorded payload per harness and event in
//! `tests/fixtures/hooks/<id>/`: `<name>.json` is the harness's stdin
//! (`@CWD@` stands for the repository), `<name>.out` the exact stdout (empty
//! where the harness drops what that event prints). Most payloads are
//! recorded from the real CLI; Copilot CLI's, Droid's, Cursor's and Kiro's
//! are written from their documentation.
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

/// What `by hook` and `by statusline` noted of the calls in `dialect`,
/// beside the user configuration the kit gives every command.
fn heard(repo: &branchyard_testkit::Repo, dialect: &str) -> Option<serde_json::Value> {
    let path = repo
        .dir
        .join("user/hooks-heard")
        .join(format!("{dialect}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    Some(serde_json::from_str(&text).unwrap())
}

/// The first twelve hex digits of `id`'s SHA-256, as a note keeps it.
fn digest(id: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(id.as_bytes()))[..12].to_owned()
}

/// `by <args>` run in `dir` with `envs` set and `input` on its standard
/// input; it must exit 0.
fn run_in(
    repo: &branchyard_testkit::Repo,
    dir: &Path,
    envs: &[(&str, &str)],
    args: &[&str],
    input: &str,
) -> Output {
    use std::io::Write as _;
    use std::process::Stdio;
    let mut child = repo
        .command(repo.by_path())
        .args(args)
        .current_dir(dir)
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
    out
}

#[test]
fn each_real_call_is_noted_as_heard_with_its_event_session_digest_and_repository() {
    let repo = yard_with_a_ready_branch();
    let root = repo.root.display().to_string();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(fixtures())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    dirs.sort();
    for dir in dirs {
        let id = dir.file_name().unwrap().to_str().unwrap().to_owned();
        for event in ["session-start", "user-prompt-submit"] {
            let input = payload(&repo, &dir.join(format!("{event}.json")));
            run(&repo, &["hook", "--harness", &id, event], &input);
            let sent: serde_json::Value = serde_json::from_str(&input).unwrap();
            let noted = heard(&repo, &id).unwrap_or_else(|| panic!("{id}: nothing noted"));
            assert_eq!(noted["dialect"], id.as_str());
            let call = &noted["calls"][event];
            assert!(call["at_ms"].as_u64().unwrap() > 0, "{id} {event}: {call}");
            // The repository the harness's directory is in (Cursor and
            // Antigravity name it in a list of their own).
            assert_eq!(call["repository"], root.as_str(), "{id} {event}: {call}");
            // The harness's own name for the event, where it sends one.
            let named = sent
                .get("hook_event_name")
                .or_else(|| sent.get("hookEventName"));
            assert_eq!(call.get("harness_event"), named, "{id} {event}");
            // The session as a digest, where the dialect finds one.
            let session = [
                "session_id",
                "sessionId",
                "conversation_id",
                "conversationId",
            ]
            .iter()
            .find_map(|key| sent.get(*key).and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty());
            assert_eq!(
                call.get("session").and_then(|v| v.as_str()),
                session.map(digest).as_deref(),
                "{id} {event}: {call}"
            );
            // Nothing else the harness sent: no prompt, no transcript.
            let text = noted.to_string();
            for key in [
                "prompt",
                "transcript_path",
                "transcriptPath",
                "lastUserInput",
            ] {
                if let Some(value) = sent.get(key).and_then(|v| v.as_str()) {
                    if !value.is_empty() {
                        assert!(!text.contains(value), "{id} {event} kept {key}: {text}");
                    }
                }
            }
            if let Some(session) = session {
                assert!(!text.contains(session), "{id} kept the session id: {text}");
            }
        }
        let status = dir.join("statusline.json");
        if status.exists() {
            run(
                &repo,
                &["statusline", "--harness", &id],
                &payload(&repo, &status),
            );
            let noted = heard(&repo, &id).unwrap();
            assert_eq!(
                noted["calls"]["statusline"]["repository"],
                root.as_str(),
                "{id}"
            );
        }
    }
    // No flag is Claude Code, and noted as such.
    let input = serde_json::json!({"session_id": "plain", "cwd": repo.root}).to_string();
    run(&repo, &["hook", "user-prompt-submit"], &input);
    let noted = heard(&repo, "claude-code").unwrap();
    assert_eq!(
        noted["calls"]["user-prompt-submit"]["session"],
        digest("plain").as_str()
    );
}

#[test]
fn a_call_outside_any_repository_is_noted_without_one() {
    let repo = yard_with_a_ready_branch();
    let outside = branchyard_testkit::Scratch::new("hook-outside");
    let input = serde_json::json!({"session_id": "o", "cwd": outside.path()}).to_string();
    let args = ["hook", "--harness", "codex", "session-start"];
    let out = run_in(&repo, outside.path(), &[], &args, &input);
    assert_eq!(stdout(&out), "", "no yard there, nothing to say");
    let call = &heard(&repo, "codex").unwrap()["calls"]["session-start"];
    assert!(call["at_ms"].as_u64().unwrap() > 0, "{call}");
    assert!(call.get("repository").is_none(), "{call}");
    assert_eq!(call["session"], digest("o").as_str());
}

#[test]
fn a_note_that_cannot_be_written_never_fails_the_hook() {
    use std::os::unix::fs::PermissionsExt as _;
    let repo = yard_with_a_ready_branch();
    let dir = fixtures().join("codex");
    let expected = std::fs::read_to_string(dir.join("session-start.out")).unwrap();
    let input = payload(&repo, &dir.join("session-start.json"));
    let args = ["hook", "--harness", "codex", "session-start"];
    // A file where the notes' directory would be: nothing can be written
    // there, whoever runs the test.
    let user = repo.dir.join("user");
    let notes = user.join("hooks-heard");
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(&notes, "not a directory").unwrap();
    let out = run(&repo, &args, &input);
    assert_eq!(stdout(&out), expected);
    assert!(out.stderr.is_empty(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "not a directory");
    // A directory that cannot be written to (where permissions apply, so
    // not for root): the same.
    std::fs::remove_file(&notes).unwrap();
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::remove_dir_all(repo.root.join(".branchyard/hooks")).unwrap();
    std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o555)).unwrap();
    let writable = std::fs::write(notes.join("probe"), "").is_ok();
    let out = run(&repo, &args, &input);
    assert_eq!(stdout(&out), expected);
    assert!(out.stderr.is_empty(), "{out:?}");
    match writable {
        true => eprintln!("note: this user writes into a 0555 directory; the file case stands"),
        false => assert!(heard(&repo, "codex").is_none()),
    }
    std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn setups_own_check_no_json_and_an_unknown_name_are_not_noted() {
    let repo = yard_with_a_ready_branch();
    let input = serde_json::json!({"session_id": "c", "cwd": repo.root}).to_string();
    // `by setup`'s canned call says so, and is still answered.
    let out = run_in(
        &repo,
        &repo.root,
        &[("BRANCHYARD_HOOK_CHECK", "1")],
        &["hook", "--harness", "codex", "session-start"],
        &input,
    );
    assert!(!stdout(&out).is_empty(), "it still answers");
    // No JSON (a person at a terminal sends none), an unknown event or
    // harness.
    run(&repo, &["hook", "--harness", "codex", "session-start"], "");
    run(&repo, &["statusline", "--harness", "codex"], "not json");
    run(&repo, &["hook", "--harness", "codex", "stop"], &input);
    run(
        &repo,
        &["hook", "--harness", "nope", "session-start"],
        &input,
    );
    assert!(!repo.dir.join("user/hooks-heard").exists());
}
