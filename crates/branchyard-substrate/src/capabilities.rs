use std::fmt;

use branchyard_sandbox::{
    Capabilities, Consistency, Locality, SandboxState, SnapshotGuarantee, SnapshotScope,
};

use crate::pb;

/// A template whose snapshot behavior cannot be determined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TemplateError {
    /// The template carries no snapshot configuration.
    MissingSnapshotConfig,
    /// A snapshot scope value this adapter does not recognize.
    UnknownScope(i32),
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::MissingSnapshotConfig => {
                f.write_str("actor template has no snapshot configuration")
            }
            TemplateError::UnknownScope(value) => {
                write!(f, "unrecognized snapshot content scope {value}")
            }
        }
    }
}

impl std::error::Error for TemplateError {}

/// Declare what actors created from `template` can guarantee.
///
/// Checkpoints are suspends captured by an atespace-scoped tag: portable
/// through object storage, with the scope the template commits on suspend.
/// Branching creates a new actor from such a tag. Substrate captures without
/// the workload's cooperation, so consistency is declared as crash-level.
///
/// Exec is declared only when the template runs the Branchyard bridge (a
/// container sets [`branchyard_bridge::KEY_ENV`], see [`runs_bridge`]): the
/// `Control` API itself has no exec or attach call.
///
/// Restore to an arbitrary checkpoint is not declared: `RevertActor` only
/// returns an actor to its latest suspend, which [`Actors::revert`]
/// exposes under its own name. Pause snapshots are node-local and unnamed, so
/// they are not offered as checkpoints; pausing itself (`PauseActor`, and
/// `ResumeActor` to continue) is declared. Live branching is not: Substrate
/// has no fork of a running actor, only suspend, tag and create from the tag.
///
/// [`Actors::revert`]: crate::Actors::revert
pub fn capabilities(template: &pb::ActorTemplate) -> Result<Capabilities, TemplateError> {
    let config = template
        .snapshot_config
        .as_ref()
        .ok_or(TemplateError::MissingSnapshotConfig)?;
    let committed = SnapshotGuarantee {
        scope: scope(config.on_commit)?,
        consistency: Consistency::Crash,
        locality: Locality::Portable,
    };
    Ok(Capabilities {
        exec: runs_bridge(template),
        ingress: true,
        checkpoint: vec![committed],
        restore: Vec::new(),
        branch: vec![committed],
        share: false,
        pause: true,
        live_branch: false,
        // Substrate's egress policy is not vendored: an actor reaches what
        // its cluster allows (docs/egress.md).
        egress: false,
    })
}

/// Whether a container of `template` runs the Branchyard bridge, judged by
/// the verifying key it is given. [`crate::template::bridge_template`] builds
/// such a template.
pub fn runs_bridge(template: &pb::ActorTemplate) -> bool {
    template.containers.iter().any(|container| {
        container
            .env
            .iter()
            .any(|var| var.name == branchyard_bridge::KEY_ENV && !var.value.is_empty())
    })
}

fn scope(value: i32) -> Result<SnapshotScope, TemplateError> {
    match pb::SnapshotContentScope::try_from(value) {
        // The API documents FULL as the default when unset.
        Ok(pb::SnapshotContentScope::Full | pb::SnapshotContentScope::Unspecified) => {
            Ok(SnapshotScope::Full)
        }
        // DATA resumes by cold boot or from the template's golden snapshot:
        // no process state from the source actor survives.
        Ok(pb::SnapshotContentScope::Data) => Ok(SnapshotScope::Disk),
        Err(_) => Err(TemplateError::UnknownScope(value)),
    }
}

/// Map an actor's reported lifecycle state.
pub fn state(actor: &pb::Actor) -> SandboxState {
    let raw = actor.status.as_ref().map_or(0, |status| status.state);
    match pb::ActorState::try_from(raw) {
        Ok(pb::ActorState::Resuming) => SandboxState::Starting,
        Ok(pb::ActorState::Running) => SandboxState::Running,
        Ok(pb::ActorState::Suspending | pb::ActorState::Pausing | pb::ActorState::Reverting) => {
            SandboxState::Stopping
        }
        Ok(pb::ActorState::Suspended) => SandboxState::Stopped,
        Ok(pb::ActorState::Paused) => SandboxState::Paused,
        Ok(pb::ActorState::Crashed) => SandboxState::Crashed,
        Ok(pb::ActorState::Deleting) => SandboxState::Destroying,
        Ok(pb::ActorState::Unspecified) => SandboxState::Unknown("ACTOR_STATE_UNSPECIFIED".into()),
        Err(_) => SandboxState::Unknown(format!("ActorState({raw})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_sandbox::{admit, Operation, Requirements};

    fn template(on_commit: pb::SnapshotContentScope) -> pb::ActorTemplate {
        pb::ActorTemplate {
            snapshot_config: Some(pb::SnapshotConfig {
                on_commit: on_commit as i32,
                storage_location: "gs://bucket/snapshots".into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn full_commit_declares_portable_full_branch_without_exec() {
        let caps = capabilities(&template(pb::SnapshotContentScope::Full)).unwrap();
        let full = SnapshotGuarantee {
            scope: SnapshotScope::Full,
            consistency: Consistency::Crash,
            locality: Locality::Portable,
        };
        assert_eq!(caps.branch, vec![full]);
        assert_eq!(caps.checkpoint, vec![full]);
        assert!(caps.ingress && !caps.exec && !caps.share && caps.restore.is_empty());
    }

    #[test]
    fn data_commit_declares_disk_scope_only() {
        let caps = capabilities(&template(pb::SnapshotContentScope::Data)).unwrap();
        let wants_full = Requirements {
            branch: Some(SnapshotGuarantee {
                scope: SnapshotScope::Full,
                consistency: Consistency::Crash,
                locality: Locality::SameHost,
            }),
            ..Requirements::default()
        };
        let missing = admit(&wants_full, &caps).unwrap_err();
        assert_eq!(missing[0].operation, Operation::Branch);
        assert_eq!(caps.branch[0].scope, SnapshotScope::Disk);
    }

    #[test]
    fn exec_and_application_consistency_are_rejected() {
        let caps = capabilities(&template(pb::SnapshotContentScope::Full)).unwrap();
        let required = Requirements {
            exec: true,
            checkpoint: Some(SnapshotGuarantee {
                scope: SnapshotScope::Full,
                consistency: Consistency::Application,
                locality: Locality::Portable,
            }),
            ..Requirements::default()
        };
        let operations: Vec<_> = admit(&required, &caps)
            .unwrap_err()
            .into_iter()
            .map(|m| m.operation)
            .collect();
        assert_eq!(operations, vec![Operation::Exec, Operation::Checkpoint]);
    }

    #[test]
    fn a_template_that_runs_the_bridge_declares_exec() {
        let mut bridged = template(pb::SnapshotContentScope::Full);
        bridged.containers.push(pb::Container {
            env: vec![pb::EnvVar {
                name: branchyard_bridge::KEY_ENV.into(),
                value: "00".repeat(32),
            }],
            ..Default::default()
        });
        let caps = capabilities(&bridged).unwrap();
        assert!(caps.exec && caps.ingress);
        assert!(admit(
            &Requirements {
                exec: true,
                ..Requirements::default()
            },
            &caps
        )
        .is_ok());
    }

    #[test]
    fn undeterminable_templates_are_errors() {
        assert_eq!(
            capabilities(&pb::ActorTemplate::default()),
            Err(TemplateError::MissingSnapshotConfig)
        );
        let mut unknown = template(pb::SnapshotContentScope::Full);
        unknown.snapshot_config.as_mut().unwrap().on_commit = 42;
        assert_eq!(capabilities(&unknown), Err(TemplateError::UnknownScope(42)));
    }

    #[test]
    fn unrecognized_states_stay_unknown() {
        let actor = |state| pb::Actor {
            status: Some(pb::ActorStatus {
                state,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            state(&actor(pb::ActorState::Paused as i32)),
            SandboxState::Paused
        );
        assert_eq!(
            state(&actor(pb::ActorState::Suspended as i32)),
            SandboxState::Stopped
        );
        assert_eq!(
            state(&actor(pb::ActorState::Reverting as i32)),
            SandboxState::Stopping
        );
        assert_eq!(
            state(&actor(99)),
            SandboxState::Unknown("ActorState(99)".into())
        );
        assert_eq!(
            state(&pb::Actor::default()),
            SandboxState::Unknown("ACTOR_STATE_UNSPECIFIED".into())
        );
    }
}
