//! Artifacts and scratch areas over real HTTP: publish/list/get with digest
//! verification, the upload size limit, grants following the delegation
//! tree, scratch lock contention across two clients, and survival across a
//! restart. See `docs/storage.md`.

mod common;

use branchyard::{BranchStatus, Envelope};
use branchyard_client::api::{OperationState, PolicySpec, SendRequest, SpawnRequest, TaskRequest};
use branchyard_client::new_key;
use branchyard_testkit::wait;
use common::{await_operation, run, task, Fixture, Server};

#[test]
fn artifacts_publish_list_get_and_grants_over_http() {
    let f = Fixture::new();
    let mut config = f.config();
    config.allow_delegation = true;
    config.by_path = Some("/bin/true".into());
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");

    let root = run(
        &client,
        &TaskRequest {
            delegation: Some(Envelope::depth(2)),
            ..task("WRITE r.txt=1", "root")
        },
    );
    assert_eq!(root.state, OperationState::Succeeded, "{root:?}");

    let spawn = |name: &str| {
        let op = repo
            .spawn(
                "root",
                &SpawnRequest {
                    prompt: format!("WRITE {name}.txt=1"),
                    name: Some(name.into()),
                    policy: PolicySpec::allow_all(),
                    ..SpawnRequest::default()
                },
                &new_key(),
            )
            .unwrap();
        await_operation(&client, &op.id)
    };
    let a = spawn("a");
    assert_eq!(a.state, OperationState::Succeeded, "{a:?}");
    let b = spawn("b");
    assert_eq!(b.state, OperationState::Succeeded, "{b:?}");

    let bytes = b"hello artifact bytes, over HTTP";
    let labels = [("kind".to_owned(), "greeting".to_owned())];
    let artifact = repo
        .publish_artifact(
            "a",
            bytes,
            Some("greeting.txt"),
            Some("text/plain"),
            &labels,
            &new_key(),
        )
        .unwrap();
    assert_eq!(artifact.name, "greeting.txt");
    assert_eq!(artifact.media_type, "text/plain");
    assert_eq!(artifact.size, bytes.len() as u64);
    assert_eq!(artifact.digest, blake3::hash(bytes).to_hex().to_string());
    assert_eq!(artifact.publisher_branch, "a");
    assert_eq!(
        artifact.labels.get("kind").map(String::as_str),
        Some("greeting")
    );

    // Publishing the same bytes again from a different branch dedups the
    // blob but records its own provenance row.
    let again = repo
        .publish_artifact("root", bytes, None, None, &[], &new_key())
        .unwrap();
    assert_eq!(again.digest, artifact.digest);
    assert_ne!(again.id, artifact.id);

    // The publisher reads its own; an ancestor (root) reads it too.
    let list = repo.artifacts("a").unwrap();
    assert!(list.iter().any(|x| x.id == artifact.id));
    let (meta, downloaded) = repo.read_artifact("a", &artifact.id).unwrap();
    assert_eq!(meta, artifact);
    assert_eq!(downloaded, bytes);
    let (meta, downloaded) = repo.read_artifact("root", &artifact.id).unwrap();
    assert_eq!(meta, artifact);
    assert_eq!(downloaded, bytes);

    // A sibling (b) may not, until shared.
    let denied = repo.read_artifact("b", &artifact.id).unwrap_err();
    assert_eq!(denied.code(), Some("denied"));
    assert!(!repo
        .artifacts("b")
        .unwrap()
        .iter()
        .any(|x| x.id == artifact.id));
    repo.share_artifact("a", &artifact.id, "b", &new_key())
        .unwrap();
    let (meta, downloaded) = repo.read_artifact("b", &artifact.id).unwrap();
    assert_eq!(meta, artifact);
    assert_eq!(downloaded, bytes);

    // An unknown artifact and an unknown branch are refused clearly.
    assert_eq!(
        repo.read_artifact("a", "nope").unwrap_err().code(),
        Some("denied")
    );
    assert_eq!(
        repo.artifacts("nope").unwrap_err().code(),
        Some("unknown_branch")
    );
}

#[test]
fn publish_over_the_configured_limit_is_refused() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_artifact_bytes = 1024;
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");
    let root = run(&client, &task("WRITE r.txt=1", "root"));
    assert_eq!(root.state, OperationState::Succeeded, "{root:?}");

    let small = vec![7u8; 8];
    let ok = repo
        .publish_artifact("root", &small, None, None, &[], &new_key())
        .unwrap();
    assert_eq!(ok.size, 8);

    let big = vec![7u8; 2048];
    let error = repo
        .publish_artifact("root", &big, None, None, &[], &new_key())
        .unwrap_err();
    assert_eq!(error.code(), Some("body_too_large"));

    // Nothing was written: the branch has just the one artifact.
    assert_eq!(repo.artifacts("root").unwrap().len(), 1);
}

#[test]
fn scratch_areas_are_created_and_lock_contention_is_enforced_across_two_clients() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client_a = server.client();
    let client_b = server.client();
    let repo_a = client_a.repo("app");
    let repo_b = client_b.repo("app");

    let root = run(&client_a, &task("WRITE r.txt=1", "root"));
    assert_eq!(root.state, OperationState::Succeeded, "{root:?}");
    let other = run(&client_a, &task("WRITE o.txt=1", "other"));
    assert_eq!(other.state, OperationState::Succeeded, "{other:?}");

    let area = repo_a
        .create_scratch("root", "shared-cache", &new_key())
        .unwrap();
    assert_eq!(area.name, "shared-cache");
    assert_eq!(area.owner_branch, "root");
    assert_eq!(
        repo_a.scratch_areas("root").unwrap(),
        std::slice::from_ref(&area)
    );
    // A sibling cannot even see it until shared.
    assert!(repo_a.scratch_areas("other").unwrap().is_empty());
    repo_a
        .share_scratch("root", "shared-cache", "other", &new_key())
        .unwrap();
    assert_eq!(repo_a.scratch_areas("other").unwrap(), [area]);

    // Keep root's turn running so its lock is not silently reclaimed
    // (a scratch lock is released on unlock, or once its holder's turn
    // ends; see docs/storage.md).
    let hang = repo_a
        .send(
            "root",
            &SendRequest {
                prompt: "HANG".into(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    wait::until("root running", || {
        matches!(repo_a.branch("root").unwrap().status, BranchStatus::Running)
    });

    let lock = repo_a
        .lock_scratch("root", "shared-cache", &new_key())
        .unwrap();
    assert_eq!(lock.holder_branch, "root");
    // Re-entrant for the holder itself, from a second client.
    let lock2 = repo_b
        .lock_scratch("root", "shared-cache", &new_key())
        .unwrap();
    assert_eq!(lock2.holder_branch, "root");
    // Another branch is refused while root's turn still runs.
    let refused = repo_b
        .lock_scratch("other", "shared-cache", &new_key())
        .unwrap_err();
    assert_eq!(refused.code(), Some("running"));

    // The lock's state is readable without acting as any branch.
    let state = repo_b.scratch_lock_state("shared-cache").unwrap().unwrap();
    assert_eq!(state.holder_branch, "root");

    // Unlocking from the other client releases it for the first to ask.
    repo_b
        .unlock_scratch("root", "shared-cache", &new_key())
        .unwrap();
    let lock3 = repo_a
        .lock_scratch("other", "shared-cache", &new_key())
        .unwrap();
    assert_eq!(lock3.holder_branch, "other");

    // Clean up the hung turn.
    repo_a.cancel("root").unwrap();
    let hang = await_operation(&client_a, &hang.id);
    assert_eq!(hang.state, OperationState::Succeeded, "{hang:?}");
}

#[test]
fn artifacts_and_scratch_areas_survive_a_restart() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");
    let root = run(&client, &task("WRITE r.txt=1", "root"));
    assert_eq!(root.state, OperationState::Succeeded, "{root:?}");

    let artifact = repo
        .publish_artifact("root", b"persisted bytes", None, None, &[], &new_key())
        .unwrap();
    let area = repo
        .create_scratch("root", "persisted-area", &new_key())
        .unwrap();
    let stopped = server.stop();
    assert_eq!(stopped.interrupted, 0);

    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");
    assert_eq!(
        repo.artifacts("root").unwrap(),
        std::slice::from_ref(&artifact)
    );
    let (meta, bytes) = repo.read_artifact("root", &artifact.id).unwrap();
    assert_eq!(meta, artifact);
    assert_eq!(bytes, b"persisted bytes");
    assert_eq!(repo.scratch_areas("root").unwrap(), [area]);
}
