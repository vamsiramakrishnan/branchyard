//! Exercise the lifecycle client over real gRPC against the in-process fake
//! `Control` in `branchyard_substrate::fake`.
//!
//! The fake models only the documented behavior these operations depend on.
//! Passing here shows the adapter's request mapping and error handling; it is
//! not evidence about a Substrate cluster.

mod common;

use branchyard_sandbox::SandboxState;
use branchyard_substrate::fake::FakeCluster;
use branchyard_substrate::{pb, Actors, Error};
use common::{Scratch, ATESPACE};
use tonic::transport::Channel;

fn template(name: &str, on_commit: pb::SnapshotContentScope) -> pb::ActorTemplate {
    pb::ActorTemplate {
        metadata: Some(pb::ResourceMetadata {
            atespace: ATESPACE.into(),
            name: name.into(),
            ..Default::default()
        }),
        snapshot_config: Some(pb::SnapshotConfig {
            on_commit: on_commit as i32,
            storage_location: "gs://bucket".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Run `test` against a fresh fake with templates `full` and `data`, whose
/// actors run no process.
fn with_fake(test: impl AsyncFnOnce(Actors, &FakeCluster)) {
    let scratch = Scratch::new("control");
    let fake = FakeCluster::start(ATESPACE, None, &scratch.path("fake"));
    fake.add_template(template("full", pb::SnapshotContentScope::Full));
    fake.add_template(template("data", pb::SnapshotContentScope::Data));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let channel = Channel::from_shared(fake.endpoint().to_owned())
            .unwrap()
            .connect()
            .await
            .unwrap();
        test(Actors::new(channel, ATESPACE), &fake).await;
    });
}

#[test]
fn capabilities_follow_the_template_commit_scope() {
    with_fake(async |mut substrate, _| {
        let full = substrate.capabilities("full").await.unwrap();
        let data = substrate.capabilities("data").await.unwrap();
        assert_ne!(full.branch, data.branch);
        assert!(!full.exec && full.ingress);
    });
}

#[test]
fn ensure_is_idempotent_and_refuses_a_different_template() {
    with_fake(async |mut substrate, _| {
        let first = substrate.ensure("root", "full").await.unwrap();
        let again = substrate.ensure("root", "full").await.unwrap();
        assert_eq!(first, again);
        assert_eq!(
            substrate.inspect(&first).await.unwrap(),
            SandboxState::Running
        );
        assert!(matches!(
            substrate.ensure("root", "data").await,
            Err(Error::TemplateMismatch { .. })
        ));
    });
}

#[test]
fn checkpoint_requires_a_stopped_actor_and_branches_get_new_identity() {
    with_fake(async |mut substrate, fake| {
        let root = substrate.ensure("root", "full").await.unwrap();
        assert!(matches!(
            substrate.checkpoint(&root, "before-parse").await,
            Err(Error::NotQuiescent {
                state: SandboxState::Running,
                ..
            })
        ));

        substrate.stop(&root).await.unwrap();
        let checkpoint = substrate.checkpoint(&root, "before-parse").await.unwrap();
        assert_eq!(checkpoint.atespace, ATESPACE);
        assert!(checkpoint.snapshot_uri.is_some());
        // A retried checkpoint adopts the tag it already created.
        assert_eq!(
            substrate.checkpoint(&root, "before-parse").await.unwrap(),
            checkpoint
        );

        let child = substrate
            .branch(&checkpoint, "child", "full")
            .await
            .unwrap();
        assert_ne!(child.uid, root.uid);
        assert_eq!(
            substrate.inspect(&child).await.unwrap(),
            SandboxState::Stopped
        );
        let stored = fake.actor("child").unwrap();
        assert_eq!(stored.source_tag.unwrap().name, "before-parse");
    });
}

#[test]
fn a_reused_name_is_never_mistaken_for_the_original_actor() {
    with_fake(async |mut substrate, _| {
        let original = substrate.ensure("worker", "full").await.unwrap();
        substrate.destroy(&original).await.unwrap();
        // Destroy is idempotent.
        substrate.destroy(&original).await.unwrap();

        let replacement = substrate.ensure("worker", "full").await.unwrap();
        assert!(matches!(
            substrate.inspect(&original).await,
            Err(Error::Replaced { .. })
        ));
        // The stale handle cannot delete the replacement.
        assert!(matches!(
            substrate.destroy(&original).await,
            Err(Error::Rpc(_))
        ));
        assert_eq!(
            substrate.inspect(&replacement).await.unwrap(),
            SandboxState::Running
        );
    });
}

#[test]
fn revert_returns_to_the_latest_suspend() {
    with_fake(async |mut substrate, _| {
        let actor = substrate.ensure("root", "full").await.unwrap();
        substrate.revert(&actor).await.unwrap();
        assert_eq!(
            substrate.inspect(&actor).await.unwrap(),
            SandboxState::Stopped
        );
    });
}
