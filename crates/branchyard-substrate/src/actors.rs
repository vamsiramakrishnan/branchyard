use std::fmt;

use branchyard_sandbox::{Capabilities, SandboxState};
use tonic::transport::Channel;
use tonic::{Code, Status};

use crate::capabilities::{capabilities, state, TemplateError};
use crate::pb;
use crate::pb::control_client::ControlClient;

/// An actor bound to the identity Substrate assigned when it was created.
///
/// Names can be reused after deletion; the UID cannot. Delete is fenced by
/// UID in the request itself (`DeleteOptions.uid`), the only precondition
/// the API offers for these calls. Resume, suspend, revert and tag take a
/// name only, so each checks the UID immediately before the call and again
/// from the actor the call returns (or right after it, for a tag): an
/// actor that was replaced before the call is never acted on, and one
/// replaced during it is reported as [`Error::ReplacedDuring`], never
/// passed off as success. Because a UID is never reused, matching UIDs
/// before and after prove that the call acted on the handle's actor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActorHandle {
    pub atespace: String,
    pub name: String,
    pub uid: String,
}

/// A durable checkpoint: an atespace-scoped Substrate tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointRef {
    pub atespace: String,
    pub name: String,
    pub uid: String,
    /// The snapshot URI, once Substrate has finished capturing the tag.
    pub snapshot_uri: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    Rpc(Status),
    Template(TemplateError),
    /// An actor with this name exists but was created from another template.
    TemplateMismatch {
        actor: String,
        expected: String,
        actual: Option<String>,
    },
    /// The name now refers to a different actor than the handle.
    Replaced {
        actor: String,
        expected_uid: String,
        actual_uid: String,
    },
    /// The actor was replaced by another with the same name while
    /// `operation` ran, which may therefore have acted on the newer actor.
    /// A tag made from it has been deleted again.
    ReplacedDuring {
        actor: String,
        operation: &'static str,
        expected_uid: String,
        actual_uid: String,
    },
    /// A checkpoint needs a stopped actor; this one is not.
    NotQuiescent {
        actor: String,
        state: SandboxState,
    },
    /// Substrate returned a response without a field its API promises.
    MissingField(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Rpc(status) => write!(f, "substrate rpc failed: {status}"),
            Error::Template(error) => error.fmt(f),
            Error::TemplateMismatch {
                actor,
                expected,
                actual,
            } => write!(
                f,
                "actor {actor} exists from template {actual:?}, not {expected}"
            ),
            Error::Replaced {
                actor,
                expected_uid,
                actual_uid,
            } => write!(f, "actor {actor} is now {actual_uid}, not {expected_uid}"),
            Error::ReplacedDuring {
                actor,
                operation,
                expected_uid,
                actual_uid,
            } => write!(
                f,
                "actor {actor} was replaced during {operation}: it is now {actual_uid}, not \
                 {expected_uid}, and {operation} may have acted on the newer actor"
            ),
            Error::NotQuiescent { actor, state } => {
                write!(
                    f,
                    "actor {actor} must be stopped to checkpoint; it is {state:?}"
                )
            }
            Error::MissingField(field) => write!(f, "substrate response is missing {field}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Status> for Error {
    fn from(status: Status) -> Self {
        Error::Rpc(status)
    }
}

impl From<TemplateError> for Error {
    fn from(error: TemplateError) -> Self {
        Error::Template(error)
    }
}

/// Lifecycle operations on actors in one atespace.
///
/// The caller builds the channel, including TLS and credentials for the
/// Substrate API endpoint, and chooses the atespace. Branchyard should map one
/// tenant authorization domain to one atespace; this adapter never publishes a
/// tag outside it.
#[derive(Clone, Debug)]
pub struct Actors {
    client: ControlClient<Channel>,
    atespace: String,
}

impl Actors {
    pub fn new(channel: Channel, atespace: impl Into<String>) -> Self {
        Self {
            client: ControlClient::new(channel),
            atespace: atespace.into(),
        }
    }

    pub fn atespace(&self) -> &str {
        &self.atespace
    }

    /// The template itself, as Substrate stores it.
    pub async fn template(&mut self, template: &str) -> Result<pb::ActorTemplate, Error> {
        Ok(self
            .client
            .get_actor_template(pb::GetActorTemplateRequest {
                actor_template: self.reference(template),
            })
            .await?
            .into_inner())
    }

    /// The actor now named `name` and its state, or `None` if there is none.
    pub async fn find(&mut self, name: &str) -> Result<Option<(ActorHandle, SandboxState)>, Error> {
        match self.get(name).await {
            Ok(actor) => Ok(Some((handle(&actor)?, state(&actor)))),
            Err(Error::Rpc(status)) if status.code() == Code::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn reference(&self, name: &str) -> Option<pb::ObjectRef> {
        Some(pb::ObjectRef {
            atespace: self.atespace.clone(),
            name: name.into(),
        })
    }

    /// Capabilities of actors created from `template`.
    pub async fn capabilities(&mut self, template: &str) -> Result<Capabilities, Error> {
        let template = self.template(template).await?;
        Ok(capabilities(&template)?)
    }

    /// Create `name` from `template` if absent, then make it running.
    ///
    /// Retrying after an ambiguous failure is safe: an existing actor is
    /// adopted only if it came from the same template.
    pub async fn ensure(&mut self, name: &str, template: &str) -> Result<ActorHandle, Error> {
        let created = self
            .client
            .create_actor(pb::CreateActorRequest {
                actor: Some(pb::Actor {
                    metadata: Some(pb::ResourceMetadata {
                        atespace: self.atespace.clone(),
                        name: name.into(),
                        ..Default::default()
                    }),
                    actor_template: self.reference(template),
                    ..Default::default()
                }),
            })
            .await;
        let actor = match created {
            Ok(response) => response.into_inner(),
            Err(status) if status.code() == Code::AlreadyExists => {
                let existing = self.get(name).await?;
                let actual = existing.actor_template.as_ref().map(|t| t.name.clone());
                if actual.as_deref() != Some(template) {
                    return Err(Error::TemplateMismatch {
                        actor: name.into(),
                        expected: template.into(),
                        actual,
                    });
                }
                existing
            }
            Err(status) => return Err(status.into()),
        };
        let handle = handle(&actor)?;
        self.start(&handle).await?;
        Ok(handle)
    }

    /// Resume the actor from its latest snapshot. Running actors are unchanged.
    pub async fn start(&mut self, actor: &ActorHandle) -> Result<(), Error> {
        self.inspect(actor).await?;
        let acted = self
            .client
            .resume_actor(pb::ResumeActorRequest {
                actor: self.reference(&actor.name),
            })
            .await?
            .into_inner()
            .actor;
        self.confirm(actor, "ResumeActor", acted).await
    }

    /// Check that `acted`, the actor an operation returned (or, if it
    /// returned none, the actor now named), is the handle's actor.
    async fn confirm(
        &mut self,
        actor: &ActorHandle,
        operation: &'static str,
        acted: Option<pb::Actor>,
    ) -> Result<(), Error> {
        let acted = match acted {
            Some(acted) => acted,
            None => self.get(&actor.name).await?,
        };
        let uid = handle(&acted)?.uid;
        if uid != actor.uid {
            return Err(Error::ReplacedDuring {
                actor: actor.name.clone(),
                operation,
                expected_uid: actor.uid.clone(),
                actual_uid: uid,
            });
        }
        Ok(())
    }

    /// Observe the actor's state, confirming it is still the same actor.
    pub async fn inspect(&mut self, actor: &ActorHandle) -> Result<SandboxState, Error> {
        let current = self.get(&actor.name).await?;
        let uid = handle(&current)?.uid;
        if uid != actor.uid {
            return Err(Error::Replaced {
                actor: actor.name.clone(),
                expected_uid: actor.uid.clone(),
                actual_uid: uid,
            });
        }
        Ok(state(&current))
    }

    /// Suspend the actor to a new portable snapshot, releasing its worker.
    pub async fn stop(&mut self, actor: &ActorHandle) -> Result<(), Error> {
        self.inspect(actor).await?;
        let acted = self
            .client
            .suspend_actor(pb::SuspendActorRequest {
                actor: self.reference(&actor.name),
            })
            .await?
            .into_inner()
            .actor;
        self.confirm(actor, "SuspendActor", acted).await
    }

    /// Pause the actor on its node, keeping its node-local snapshot
    /// (`PauseActor`). Resume it with [`Actors::start`], or suspend it with
    /// [`Actors::stop`], which uploads that snapshot.
    pub async fn pause(&mut self, actor: &ActorHandle) -> Result<(), Error> {
        self.inspect(actor).await?;
        let acted = self
            .client
            .pause_actor(pb::PauseActorRequest {
                actor: self.reference(&actor.name),
            })
            .await?
            .into_inner()
            .actor;
        self.confirm(actor, "PauseActor", acted).await
    }

    /// Delete a checkpoint's tag. Deleting an absent tag succeeds.
    pub async fn delete_tag(&mut self, name: &str) -> Result<(), Error> {
        let deleted = self
            .client
            .delete_tag(pb::DeleteTagRequest {
                tag: self.reference(name),
                options: None,
            })
            .await;
        match deleted {
            Ok(_) => Ok(()),
            Err(status) if status.code() == Code::NotFound => Ok(()),
            Err(status) => Err(status.into()),
        }
    }

    /// Record the stopped actor's latest snapshot as a durable checkpoint.
    ///
    /// The actor must already be stopped. Checkpointing does not suspend
    /// implicitly: the caller chooses the quiescent point.
    #[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-substrate
    pub async fn checkpoint(
        &mut self,
        actor: &ActorHandle,
        name: &str,
    ) -> Result<CheckpointRef, Error> {
        let observed = self.inspect(actor).await?;
        if observed != SandboxState::Stopped {
            return Err(Error::NotQuiescent {
                actor: actor.name.clone(),
                state: observed,
            });
        }
        let created = self
            .client
            .create_tag(pb::CreateTagRequest {
                tag: Some(pb::Tag {
                    metadata: Some(pb::ResourceMetadata {
                        atespace: self.atespace.clone(),
                        name: name.into(),
                        ..Default::default()
                    }),
                    scope: pb::TagScope::Atespace as i32,
                    source_actor: self.reference(&actor.name),
                    ..Default::default()
                }),
            })
            .await;
        let tag = match created {
            Ok(response) => response.into_inner(),
            // A retried checkpoint adopts its earlier tag only if that tag
            // captured the same actor within this atespace.
            Err(status) if status.code() == Code::AlreadyExists => {
                let existing = self
                    .client
                    .get_tag(pb::GetTagRequest {
                        tag: self.reference(name),
                    })
                    .await?
                    .into_inner();
                if existing.source_actor != self.reference(&actor.name)
                    || existing.scope != pb::TagScope::Atespace as i32
                {
                    return Err(Error::Rpc(status));
                }
                existing
            }
            Err(status) => return Err(status.into()),
        };
        let metadata = tag.metadata.ok_or(Error::MissingField("tag.metadata"))?;
        // A tag names no source UID, so check that the actor was not
        // replaced while it was taken; if it was, the tag may hold the
        // newer actor's state and is deleted, fenced by its own UID.
        if let Err(error) = self.confirm(actor, "CreateTag", None).await {
            let _ = self
                .client
                .delete_tag(pb::DeleteTagRequest {
                    tag: self.reference(&metadata.name),
                    options: Some(pb::DeleteOptions {
                        uid: metadata.uid.clone(),
                        ..Default::default()
                    }),
                })
                .await;
            return Err(error);
        }
        Ok(CheckpointRef {
            atespace: metadata.atespace,
            name: metadata.name,
            uid: metadata.uid,
            snapshot_uri: tag
                .status
                .and_then(|status| status.snapshot)
                .map(|snapshot| snapshot.snapshot_uri),
        })
    }

    /// Create a new, stopped actor seeded from `checkpoint`.
    ///
    /// The child has its own name, UID and Substrate-issued identity. With a
    /// full-scope template it also inherits the source's memory and root
    /// filesystem, including anything secret that was present there when the
    /// source was suspended.
    pub async fn branch(
        &mut self,
        checkpoint: &CheckpointRef,
        name: &str,
        template: &str,
    ) -> Result<ActorHandle, Error> {
        let actor = self
            .client
            .create_actor(pb::CreateActorRequest {
                actor: Some(pb::Actor {
                    metadata: Some(pb::ResourceMetadata {
                        atespace: self.atespace.clone(),
                        name: name.into(),
                        ..Default::default()
                    }),
                    actor_template: self.reference(template),
                    source_tag: Some(pb::ObjectRef {
                        atespace: checkpoint.atespace.clone(),
                        name: checkpoint.name.clone(),
                    }),
                    ..Default::default()
                }),
            })
            .await?
            .into_inner();
        handle(&actor)
    }

    /// Return a running, paused or crashed actor to its latest suspend,
    /// discarding everything since. This is not restore to a chosen checkpoint.
    pub async fn revert(&mut self, actor: &ActorHandle) -> Result<(), Error> {
        self.inspect(actor).await?;
        let acted = self
            .client
            .revert_actor(pb::RevertActorRequest {
                actor: self.reference(&actor.name),
            })
            .await?
            .into_inner()
            .actor;
        self.confirm(actor, "RevertActor", acted).await
    }

    /// Delete the actor in any state. Deleting an absent actor succeeds.
    ///
    /// The delete is fenced by UID, so it cannot remove a newer actor that
    /// reused the name.
    pub async fn destroy(&mut self, actor: &ActorHandle) -> Result<(), Error> {
        let deleted = self
            .client
            .delete_actor(pb::DeleteActorRequest {
                actor: self.reference(&actor.name),
                any_state: true,
                options: Some(pb::DeleteOptions {
                    uid: actor.uid.clone(),
                    ..Default::default()
                }),
            })
            .await;
        match deleted {
            Ok(_) => Ok(()),
            Err(status) if status.code() == Code::NotFound => Ok(()),
            Err(status) => Err(status.into()),
        }
    }

    async fn get(&mut self, name: &str) -> Result<pb::Actor, Error> {
        Ok(self
            .client
            .get_actor(pb::GetActorRequest {
                actor: self.reference(name),
            })
            .await?
            .into_inner())
    }
}

fn handle(actor: &pb::Actor) -> Result<ActorHandle, Error> {
    let metadata = actor
        .metadata
        .as_ref()
        .ok_or(Error::MissingField("actor.metadata"))?;
    if metadata.uid.is_empty() {
        return Err(Error::MissingField("actor.metadata.uid"));
    }
    Ok(ActorHandle {
        atespace: metadata.atespace.clone(),
        name: metadata.name.clone(),
        uid: metadata.uid.clone(),
    })
}
