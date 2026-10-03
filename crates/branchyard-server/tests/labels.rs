//! Worker labels over real HTTP: an operation that requires a label no
//! live worker carries stays queued and, after `unclaimable_after`, says
//! why in `GET /v1/operations/{id}` and `GET /v1/repos/{repo}/operations`;
//! a server whose worker carries the labels runs it after a restart on
//! the same data directory; work requiring nothing runs anywhere; a
//! malformed label is refused at admission.

mod common;

use std::time::Duration;

use branchyard_client::api::OperationState;
use branchyard_client::new_key;
use common::{eventually, run, task, wait, Fixture, Server};

#[test]
fn labeled_work_waits_for_a_worker_that_carries_its_labels_and_says_why() {
    let f = Fixture::new();
    let mut config = f.config();
    config.labels = vec!["linux".into()];
    config.unclaimable_after = Duration::ZERO;
    let server = Server::start(config);
    let client = server.client();

    let mut gpu = task("WRITE gpu.txt=1", "needs-gpu");
    gpu.require_labels = vec!["gpu".into(), "linux".into(), "gpu".into()];
    let op = client.repo("app").submit_task(&gpu, &new_key()).unwrap();
    assert_eq!(op.requires, ["gpu", "linux"]);
    // Work that requires nothing still runs while it waits.
    let plain = run(&client, &task("WRITE plain.txt=1", "plain"));
    assert_eq!(plain.state, OperationState::Succeeded);
    eventually("the operation to say why it waits", || {
        client.operation(&op.id).unwrap().waiting.is_some()
    });
    let waiting = client.operation(&op.id).unwrap();
    assert_eq!(waiting.state, OperationState::Queued);
    let reason = waiting.waiting.unwrap();
    assert!(reason.contains("gpu, linux"), "{reason}");
    assert!(reason.contains("[linux]"), "{reason}");
    let listed = client.repo("app").operations(Some("needs-gpu")).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, op.id);
    assert!(listed[0].waiting.is_some());
    assert!(client
        .repo("app")
        .operations(Some("plain"))
        .unwrap()
        .is_empty());

    // A malformed label is refused before anything is admitted.
    let mut bad = task("WRITE x.txt=1", "bad-label");
    bad.require_labels = vec!["GPU box".into()];
    let error = client
        .repo("app")
        .submit_task(&bad, &new_key())
        .unwrap_err();
    assert_eq!(error.code(), Some("invalid_request"), "{error}");

    // A worker with the labels takes it over.
    server.stop();
    let mut config = f.config();
    config.labels = vec!["linux".into(), "gpu".into()];
    let server = Server::start(config);
    let client = server.client();
    let done = wait(&client, &op.id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    assert_eq!(done.waiting, None);
}
