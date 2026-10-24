//! `by hook` and `by statusline` through the built `by`, as a Claude Code
//! session a person started runs them: the hook's JSON on stdin, a
//! temporary repository, the fake ACP agent for the branches. Hermetic: no
//! real harness, no network. Requires `git` and `sh`.

#![allow(clippy::panic, clippy::unwrap_used, clippy::unwrap_in_result)] // tests: a panic is the failure report
use std::process::Output;

use serde_json::{json, Value};

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `by hook <event>` for `session` in the repository; it must exit 0.
/// Returns its additionalContext, or `None` when it printed nothing.
fn hook(repo: &branchyard_testkit::Repo, event: &str, session: &str) -> Option<String> {
    let input = json!({
        "session_id": session,
        "cwd": repo.root,
        "hook_event_name": event,
        "prompt": "carry on",
    });
    hook_raw(repo, event, &input.to_string())
}

fn hook_raw(repo: &branchyard_testkit::Repo, event: &str, input: &str) -> Option<String> {
    let out = repo.by_with_stdin(&["hook", event], input);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = stdout(&out);
    if text.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(&text).unwrap();
    let output = &value["hookSpecificOutput"];
    let name = match event {
        "session-start" => "SessionStart",
        _ => "UserPromptSubmit",
    };
    assert_eq!(output["hookEventName"], name, "{value}");
    Some(output["additionalContext"].as_str().unwrap().to_owned())
}

fn statusline(repo: &branchyard_testkit::Repo, input: &Value) -> String {
    let out = repo.by_with_stdin(&["statusline"], &input.to_string());
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    stdout(&out)
}

#[test]
fn without_a_yard_both_print_nothing_and_create_nothing() {
    let repo = branchyard_testkit::repo!();
    assert_eq!(hook(&repo, "session-start", "s1"), None);
    assert_eq!(hook(&repo, "user-prompt-submit", "s1"), None);
    assert_eq!(statusline(&repo, &json!({"cwd": repo.root})), "");
    assert!(!repo.root.join(".branchyard").exists());
}

#[test]
fn malformed_input_or_an_unknown_event_prints_nothing_and_exits_0() {
    let repo = branchyard_testkit::repo!();
    repo.ok(&["ls"]);
    for input in ["", "not json", "[1, 2]", "{\"session_id\": "] {
        assert_eq!(
            hook_raw(&repo, "user-prompt-submit", input),
            None,
            "{input:?}"
        );
    }
    assert_eq!(hook_raw(&repo, "stop", "{}"), None);
    // A store this build cannot read is the same: nothing, exit 0.
    std::fs::write(
        repo.root.join(".branchyard/state.db"),
        "this is not a database, and is longer than a header",
    )
    .unwrap();
    assert_eq!(hook(&repo, "session-start", "s1"), None);
    assert_eq!(statusline(&repo, &json!({"cwd": repo.root})), "");
}

#[test]
fn a_parent_session_hears_once_that_a_branch_finished() {
    let repo = branchyard_testkit::repo!();
    repo.ok(&["ls"]);
    // Nothing yet: nothing said, and the session's cursor is kept.
    assert_eq!(hook(&repo, "session-start", "s1"), None);
    assert!(repo.root.join(".branchyard/hooks/s1.json").is_file());
    assert_eq!(
        statusline(&repo, &json!({"workspace": {"current_dir": repo.root}})),
        "by: no branches\n"
    );

    let out = repo.by_agent(&["run", "WRITE hello.txt=hi", "--name", "hello"]);
    assert!(out.status.success(), "{out:?}");
    let said = hook(&repo, "user-prompt-submit", "s1").unwrap();
    assert!(
        said.contains("- hello: finished, ready to integrate (1 file changed)"),
        "{said}"
    );
    // Told once.
    assert_eq!(hook(&repo, "user-prompt-submit", "s1"), None);

    // A new session is told what stands, not old news.
    let first = hook(&repo, "session-start", "s2").unwrap();
    assert!(first.contains("1 ready to integrate"), "{first}");
    assert!(!first.contains("hello:"), "{first}");
    assert_eq!(hook(&repo, "user-prompt-submit", "s2"), None);

    // A second turn that ends the same way is news again.
    let out = repo.by_agent(&["send", "hello", "WRITE again.txt=2"]);
    assert!(out.status.success(), "{out:?}");
    let said = hook(&repo, "user-prompt-submit", "s1").unwrap();
    assert!(said.contains("- hello: finished"), "{said}");

    assert_eq!(
        statusline(&repo, &json!({"cwd": repo.root})),
        "by: 1 ready\n"
    );
    // In the branch's worktree, the line names it.
    let worktree = repo.json(&["show", "hello", "--json"])["worktree"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        statusline(&repo, &json!({"workspace": {"current_dir": worktree}})),
        "by: hello · 1 ready\n"
    );
}

#[test]
fn a_branch_session_hears_its_unread_messages_once() {
    let mut repo = branchyard_testkit::repo!();
    let out = repo.by_agent(&[
        "run",
        "WRITE root.txt=r",
        "--name",
        "root",
        "--delegate=2",
        "--yes",
    ]);
    assert!(out.status.success(), "{out:?}");
    repo.json(&[
        "spawn",
        "SH by report tests-pass",
        "--parent",
        "root",
        "--name",
        "kid",
        "--wait",
        "--yes",
        "--json",
    ]);
    repo.set_env("BRANCHYARD_BRANCH", "root");
    let said = hook(&repo, "session-start", "b1").unwrap();
    assert!(said.contains("1 unread message for root"), "{said}");
    assert!(said.contains("report from kid: tests-pass"), "{said}");
    assert_eq!(hook(&repo, "user-prompt-submit", "b1"), None);
    // The hook only reads: the message is still unread for root's turns.
    repo.remove_env("BRANCHYARD_BRANCH");
    let inbox = repo.json(&["inbox", "--as", "root", "--unread", "--json"]);
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 1, "{inbox}");
}

#[test]
fn a_new_session_hears_of_failed_branches_as_the_status_line_counts_them() {
    let repo = branchyard_testkit::repo!();
    let out = repo.by_agent(&["run", "WRITE hello.txt=hi", "--name", "hello"]);
    assert!(out.status.success(), "{out:?}");
    let out = repo.by_agent(&["run", "EXIT", "--name", "exits"]);
    assert!(!out.status.success(), "{out:?}");
    let told = hook(&repo, "session-start", "fresh").unwrap();
    assert_eq!(told, "Branchyard: 1 ready to integrate, 1 failed (by ls).");
    let line = statusline(&repo, &json!({"workspace": {"current_dir": repo.root}}));
    assert_eq!(line, "by: 1 ready · 1 failed\n");
}
