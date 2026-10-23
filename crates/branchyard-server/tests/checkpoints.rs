//! Rewinding to a checkpoint and forking at one over real HTTP on
//! 127.0.0.1, with the fake ACP agent as the `gemini-cli` harness.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use branchyard::{Activity, BranchStatus};
use branchyard_client::api::{ForkRequest, OperationState, PolicySpec, SendRequest};
use branchyard_client::new_key;
use branchyard_testkit::wait;
use common::{await_operation, post, raw, run, task, Fixture, Server, TOKEN};
use serde_json::Value;

fn json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body:?}"))
}

#[test]
fn a_branch_is_rewound_and_forked_at_a_checkpoint_over_http() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");
    let done = run(&client, &task("WRITE one.txt=1", "cp"));
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let sent = repo
        .send(
            "cp",
            &SendRequest {
                prompt: "WRITE two.txt=2".into(),
                policy: PolicySpec::allow_all(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    await_operation(&client, &sent.id);

    // Back to turn 1: two.txt leaves the candidate, and the rewind is
    // recorded as the local one is.
    let rewound = repo.rewind("cp", 1).unwrap();
    assert_eq!(
        (rewound.branch.as_str(), rewound.from, rewound.to),
        ("cp", Some(2), 1)
    );
    let diff = repo.diff("cp").unwrap();
    assert!(
        diff.contains("one.txt") && !diff.contains("two.txt"),
        "{diff}"
    );
    assert_eq!(repo.branch("cp").unwrap().status, BranchStatus::Ready);
    let events = repo.events("cp", 0).unwrap().events;
    assert!(events
        .iter()
        .any(|e| matches!(e.activity, Activity::Rewound { to: 1, .. })));
    // A checkpoint it does not have is refused with the SDK's message.
    let missing = repo.rewind("cp", 9).unwrap_err();
    assert_eq!(missing.code(), Some("unknown_checkpoint"), "{missing}");
    assert!(
        missing.to_string().contains("cp has no checkpoint 9;"),
        "{missing}"
    );
    // And forward again.
    assert_eq!(repo.rewind("cp", 2).unwrap().from, Some(1));
    assert!(repo.diff("cp").unwrap().contains("two.txt"));

    // A fork at turn 1 has turn 1's files, and says how its session goes on.
    let op = repo
        .fork(
            "cp",
            &ForkRequest {
                prompt: "WHOAMI".into(),
                name: Some("cp-at".into()),
                at: Some(1),
                policy: PolicySpec::allow_all(),
                ..ForkRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    assert_eq!(op.branches, ["cp-at"]);
    let forked = await_operation(&client, &op.id);
    assert_eq!(forked.state, OperationState::Succeeded, "{forked:?}");
    let info = &forked.result.unwrap().branches[0];
    assert_eq!(info.parent.as_deref(), Some("cp"));
    // Its base is turn 1's commit, so its own diff is empty.
    assert_eq!(info.base, rewound.commit);
    assert_eq!(repo.diff("cp-at").unwrap(), "");
    let events = repo.events("cp-at", 0).unwrap().events;
    assert!(events.iter().any(|e| matches!(
        &e.activity,
        Activity::ForkedAt { branch, turn: 1, .. } if branch == "cp"
    )));

    // Refused before anything is queued: a checkpoint it does not have,
    // and `at` with `fresh_session`.
    let refused = repo
        .fork(
            "cp",
            &ForkRequest {
                prompt: "WHOAMI".into(),
                at: Some(7),
                ..ForkRequest::default()
            },
            &new_key(),
        )
        .unwrap_err();
    assert_eq!(refused.code(), Some("unknown_checkpoint"), "{refused}");
    let (status, _, body) = raw(
        server.addr,
        &post(
            "/v1/repos/app/branches/cp/fork",
            Some(TOKEN),
            "",
            r#"{"prompt": "x", "at": 1, "fresh_session": true}"#,
        ),
    );
    assert_eq!(status, 400, "{body}");
    assert_eq!(json(&body)["error"]["code"], "invalid_request");
    assert!(repo.branch("cp-1").is_err());
}

#[test]
fn a_rewind_is_refused_as_the_local_one_is_over_http() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");
    let op = repo.submit_task(&task("HANG", "held"), &new_key()).unwrap();
    wait::until("the prompt to be submitted", || {
        common::started(&client, "app", "held")
    });
    // The operation running its turn holds the branch.
    let busy = repo.rewind("held", 0).unwrap_err();
    assert_eq!(busy.code(), Some("branch_busy"), "{busy}");
    repo.cancel("held").unwrap();
    await_operation(&client, &op.id);

    let missing = repo.rewind("nope", 0).unwrap_err();
    assert_eq!(missing.code(), Some("unknown_branch"));
    let (status, _, body) = raw(
        server.addr,
        &post(
            "/v1/repos/app/branches/held/rewind",
            Some(TOKEN),
            "",
            r#"{"turn": 1}"#,
        ),
    );
    assert_eq!(status, 400, "{body}");
    assert_eq!(json(&body)["error"]["code"], "invalid_request");
    // Without a token, nothing.
    let (status, _, _) = raw(
        server.addr,
        &post(
            "/v1/repos/app/branches/held/rewind",
            None,
            "",
            r#"{"to": 0}"#,
        ),
    );
    assert_eq!(status, 401);
}
