//! Starting from an issue over HTTP: `TaskRequest::issue` and
//! `GET /v1/repos/{repo}/issue`. The server resolves an issue by running
//! the operator's `by issue --json`; here that is a shell stand-in that
//! answers like `by` would, so no `gh` and no tracker is reached. Requires
//! `git` and `sh`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use branchyard::BranchStatus;
use branchyard_client::api::{OperationState, TaskRequest};
use branchyard_client::new_key;
use common::{await_operation, Fixture, Server};

/// `by issue --json -- REF EXTRA`: logs its arguments, answers for `#12`,
/// and fails like `by` does for a Linear key without credentials.
const FAKE_BY: &str = r##"#!/bin/sh
printf '%s\n' "$*" >> "$(dirname "$0")/by.log"
[ "$1" = issue ] || { echo "fake by: unexpected: $*" >&2; exit 2; }
case "$4" in
"#12")
  extra="$5"
  printf '{"issue": {"number": 12, "url": "https://github.com/acme/widgets/issues/12", "title": "Parser crash"}, "branch": "issue-12-parser-crash", "prompt": "Resolve GitHub issue #12: Parser crash\\nhttps://github.com/acme/widgets/issues/12\\n\\nWRITE issue.txt=fixed\\n\\nAdditional instructions:\\n%s\\n"}' "$extra" ;;
linear:*)
  echo "by: fetching" >&2
  echo "error: --issue linear:ENG-1 needs LINEAR_API_KEY in the environment" >&2
  exit 3 ;;
*) echo "error: no such issue" >&2; exit 1 ;;
esac
"##;

fn server(f: &Fixture) -> Server {
    let by = f.dir.join("by");
    fs::write(&by, FAKE_BY).unwrap();
    fs::set_permissions(&by, fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = f.config();
    config.by_path = Some(by);
    Server::start(config)
}

#[test]
fn a_task_starts_from_an_issue_the_server_resolved() {
    let f = Fixture::new();
    let server = server(&f);
    let client = server.client();
    let repo = client.repo("app");

    // The preview names the issue and the branch, and starts nothing.
    let preview = repo.issue("#12").unwrap();
    assert_eq!(preview.issue.number, 12);
    assert_eq!(preview.issue.title, "Parser crash");
    assert_eq!(preview.branch, "issue-12-parser-crash");
    assert!(repo.branches().unwrap().is_empty());

    // The task: no prompt of its own beyond the extra instructions.
    let request = TaskRequest {
        prompt: "Keep it small.".into(),
        issue: Some("#12".into()),
        harness: Some("gemini-cli".into()),
        policy: branchyard_client::api::PolicySpec::allow_all(),
        ..TaskRequest::default()
    };
    let op = await_operation(&client, &repo.submit_task(&request, &new_key()).unwrap().id);
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    let branch = repo.branch("issue-12-parser-crash").unwrap();
    assert!(matches!(branch.status, BranchStatus::Ready), "{branch:?}");
    assert!(
        branch
            .prompt
            .starts_with("Resolve GitHub issue #12: Parser crash\n"),
        "{}",
        branch.prompt
    );
    assert!(
        branch
            .prompt
            .ends_with("Additional instructions:\nKeep it small.\n"),
        "{}",
        branch.prompt
    );
    // The fake agent did what the issue's text asked.
    assert_eq!(
        fs::read_to_string(branch.worktree.join("issue.txt"))
            .unwrap()
            .trim(),
        "fixed"
    );
    // The command got the reference and the extra text as plain arguments.
    let log = fs::read_to_string(f.dir.join("by.log")).unwrap();
    assert!(
        log.lines()
            .any(|l| l == "issue --json -- #12 Keep it small."),
        "{log}"
    );

    // A name given in the request still wins.
    let named = TaskRequest {
        issue: Some("#12".into()),
        name: Some("mine".into()),
        harness: Some("gemini-cli".into()),
        policy: branchyard_client::api::PolicySpec::allow_all(),
        ..TaskRequest::default()
    };
    await_operation(&client, &repo.submit_task(&named, &new_key()).unwrap().id);
    repo.branch("mine").unwrap();
}

#[test]
fn an_issue_the_server_cannot_fetch_is_refused_with_a_kind_and_a_next_step() {
    let f = Fixture::new();
    let server = server(&f);
    let client = server.client();
    let repo = client.repo("app");

    let error = repo.issue("linear:ENG-1").unwrap_err();
    assert_eq!(error.code(), Some("issue_unavailable"), "{error}");
    let text = error.to_string();
    assert!(text.contains("needs LINEAR_API_KEY"), "{text}");
    assert!(text.contains("next:"), "{text}");
    assert!(!text.contains("fetching"), "{text}");

    let request = TaskRequest {
        issue: Some("linear:ENG-1".into()),
        harness: Some("gemini-cli".into()),
        ..TaskRequest::default()
    };
    let error = repo.submit_task(&request, &new_key()).unwrap_err();
    assert_eq!(error.code(), Some("issue_unavailable"), "{error}");
    // Nothing was admitted.
    assert!(repo.branches().unwrap().is_empty());

    // A task with neither a prompt nor an issue is still refused.
    let empty = TaskRequest {
        harness: Some("gemini-cli".into()),
        ..TaskRequest::default()
    };
    let error = repo.submit_task(&empty, &new_key()).unwrap_err();
    assert_eq!(error.code(), Some("invalid_request"), "{error}");
}
