//! Exercise the adapter over real gRPC against an in-process fake `Control`.
//!
//! The fake models only the documented behavior these operations depend on.
//! Passing here shows the adapter's request mapping and error handling; it is
//! not evidence about a Substrate cluster.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use branchyard_sandbox::SandboxState;
use branchyard_substrate::pb::control_server::{Control, ControlServer};
use branchyard_substrate::{pb, Error, SubstrateProvider};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Server};
use tonic::{Request, Response, Status};

const ATESPACE: &str = "tenant-a";

#[derive(Default)]
struct State {
    actors: HashMap<String, pb::Actor>,
    tags: HashMap<String, pb::Tag>,
    next_uid: u32,
}

#[derive(Clone, Default)]
struct FakeControl(Arc<Mutex<State>>);

fn set_state(actor: &mut pb::Actor, state: pb::ActorState) {
    actor.status.get_or_insert_with(Default::default).state = state as i32;
}

fn state_of(actor: &pb::Actor) -> i32 {
    actor.status.as_ref().map_or(0, |s| s.state)
}

fn name_of(reference: Option<pb::ObjectRef>) -> Result<String, Status> {
    let reference = reference.ok_or_else(|| Status::invalid_argument("missing reference"))?;
    if reference.atespace != ATESPACE {
        return Err(Status::permission_denied("wrong atespace"));
    }
    Ok(reference.name)
}

impl FakeControl {
    fn transition(
        &self,
        reference: Option<pb::ObjectRef>,
        to: pb::ActorState,
    ) -> Result<pb::Actor, Status> {
        let name = name_of(reference)?;
        let mut state = self.0.lock().unwrap();
        let actor = state
            .actors
            .get_mut(&name)
            .ok_or_else(|| Status::not_found(name))?;
        set_state(actor, to);
        Ok(actor.clone())
    }
}

#[tonic::async_trait]
impl Control for FakeControl {
    async fn get_actor_template(
        &self,
        request: Request<pb::GetActorTemplateRequest>,
    ) -> Result<Response<pb::ActorTemplate>, Status> {
        let name = name_of(request.into_inner().actor_template)?;
        let on_commit = match name.as_str() {
            "full" => pb::SnapshotContentScope::Full,
            "data" => pb::SnapshotContentScope::Data,
            _ => return Err(Status::not_found(name)),
        };
        Ok(Response::new(pb::ActorTemplate {
            snapshot_config: Some(pb::SnapshotConfig {
                on_commit: on_commit as i32,
                storage_location: "gs://bucket".into(),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn create_actor(
        &self,
        request: Request<pb::CreateActorRequest>,
    ) -> Result<Response<pb::Actor>, Status> {
        let mut actor = request
            .into_inner()
            .actor
            .ok_or_else(|| Status::invalid_argument("actor"))?;
        let mut state = self.0.lock().unwrap();
        let metadata = actor
            .metadata
            .as_mut()
            .ok_or_else(|| Status::invalid_argument("metadata"))?;
        if metadata.atespace != ATESPACE {
            return Err(Status::permission_denied("wrong atespace"));
        }
        if state.actors.contains_key(&metadata.name) {
            return Err(Status::already_exists(metadata.name.clone()));
        }
        if let Some(tag) = &actor.source_tag {
            if !state.tags.contains_key(&tag.name) {
                return Err(Status::failed_precondition("unknown tag"));
            }
        }
        state.next_uid += 1;
        metadata.uid = format!("uid-{}", state.next_uid);
        let name = metadata.name.clone();
        set_state(&mut actor, pb::ActorState::Suspended);
        state.actors.insert(name, actor.clone());
        Ok(Response::new(actor))
    }

    async fn get_actor(
        &self,
        request: Request<pb::GetActorRequest>,
    ) -> Result<Response<pb::Actor>, Status> {
        let name = name_of(request.into_inner().actor)?;
        let state = self.0.lock().unwrap();
        let actor = state
            .actors
            .get(&name)
            .cloned()
            .ok_or_else(|| Status::not_found(name))?;
        Ok(Response::new(actor))
    }

    async fn resume_actor(
        &self,
        request: Request<pb::ResumeActorRequest>,
    ) -> Result<Response<pb::ResumeActorResponse>, Status> {
        let actor = self.transition(request.into_inner().actor, pb::ActorState::Running)?;
        Ok(Response::new(pb::ResumeActorResponse {
            actor: Some(actor),
            resumed: true,
        }))
    }

    async fn suspend_actor(
        &self,
        request: Request<pb::SuspendActorRequest>,
    ) -> Result<Response<pb::SuspendActorResponse>, Status> {
        let actor = self.transition(request.into_inner().actor, pb::ActorState::Suspended)?;
        Ok(Response::new(pb::SuspendActorResponse {
            actor: Some(actor),
        }))
    }

    async fn revert_actor(
        &self,
        request: Request<pb::RevertActorRequest>,
    ) -> Result<Response<pb::RevertActorResponse>, Status> {
        let actor = self.transition(request.into_inner().actor, pb::ActorState::Suspended)?;
        Ok(Response::new(pb::RevertActorResponse {
            actor: Some(actor),
        }))
    }

    async fn create_tag(
        &self,
        request: Request<pb::CreateTagRequest>,
    ) -> Result<Response<pb::Tag>, Status> {
        let mut tag = request
            .into_inner()
            .tag
            .ok_or_else(|| Status::invalid_argument("tag"))?;
        if tag.scope != pb::TagScope::Atespace as i32 {
            return Err(Status::permission_denied(
                "fake only accepts atespace-scoped tags",
            ));
        }
        let source = name_of(tag.source_actor.clone())?;
        let mut state = self.0.lock().unwrap();
        let actor = state
            .actors
            .get(&source)
            .ok_or_else(|| Status::not_found(source.clone()))?;
        if state_of(actor) != pb::ActorState::Suspended as i32 {
            return Err(Status::failed_precondition(
                "source actor must be suspended",
            ));
        }
        let metadata = tag
            .metadata
            .as_mut()
            .ok_or_else(|| Status::invalid_argument("metadata"))?;
        if state.tags.contains_key(&metadata.name) {
            return Err(Status::already_exists(metadata.name.clone()));
        }
        metadata.uid = format!("tag-{}", metadata.name);
        tag.status = Some(pb::TagStatus {
            snapshot: Some(pb::ExternalSnapshot {
                snapshot_uri: format!("gs://bucket/{source}/{}", metadata.name),
                ..Default::default()
            }),
            ..Default::default()
        });
        state.tags.insert(metadata.name.clone(), tag.clone());
        Ok(Response::new(tag))
    }

    async fn get_tag(
        &self,
        request: Request<pb::GetTagRequest>,
    ) -> Result<Response<pb::Tag>, Status> {
        let name = name_of(request.into_inner().tag)?;
        let state = self.0.lock().unwrap();
        let tag = state
            .tags
            .get(&name)
            .cloned()
            .ok_or_else(|| Status::not_found(name))?;
        Ok(Response::new(tag))
    }

    async fn delete_actor(
        &self,
        request: Request<pb::DeleteActorRequest>,
    ) -> Result<Response<pb::Actor>, Status> {
        let request = request.into_inner();
        let name = name_of(request.actor)?;
        let mut state = self.0.lock().unwrap();
        let actor = state
            .actors
            .get(&name)
            .ok_or_else(|| Status::not_found(name.clone()))?;
        let uid = request.options.map(|o| o.uid).unwrap_or_default();
        if !uid.is_empty() && actor.metadata.as_ref().map(|m| m.uid.as_str()) != Some(uid.as_str())
        {
            return Err(Status::failed_precondition("uid precondition failed"));
        }
        Ok(Response::new(state.actors.remove(&name).unwrap()))
    }
}

async fn provider() -> (SubstrateProvider, FakeControl) {
    let fake = FakeControl::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = ControlServer::new(fake.clone());
    tokio::spawn(async move {
        Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let channel = Channel::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    (SubstrateProvider::new(channel, ATESPACE), fake)
}

#[tokio::test]
async fn capabilities_follow_the_template_commit_scope() {
    let (mut substrate, _) = provider().await;
    let full = substrate.capabilities("full").await.unwrap();
    let data = substrate.capabilities("data").await.unwrap();
    assert_ne!(full.branch, data.branch);
    assert!(!full.exec && full.ingress);
}

#[tokio::test]
async fn ensure_is_idempotent_and_refuses_a_different_template() {
    let (mut substrate, _) = provider().await;
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
}

#[tokio::test]
async fn checkpoint_requires_a_stopped_actor_and_branches_get_new_identity() {
    let (mut substrate, fake) = provider().await;
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
    let stored = fake.0.lock().unwrap().actors["child"].clone();
    assert_eq!(stored.source_tag.unwrap().name, "before-parse");
}

#[tokio::test]
async fn a_reused_name_is_never_mistaken_for_the_original_actor() {
    let (mut substrate, _) = provider().await;
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
}

#[tokio::test]
async fn revert_returns_to_the_latest_suspend() {
    let (mut substrate, _) = provider().await;
    let actor = substrate.ensure("root", "full").await.unwrap();
    substrate.revert(&actor).await.unwrap();
    assert_eq!(
        substrate.inspect(&actor).await.unwrap(),
        SandboxState::Stopped
    );
}
