//! Sandbox providers, independent of any runtime SDK.
//!
//! A provider declares the guarantees it can keep; a task declares the ones
//! it requires. [`admit`] rejects a request the provider cannot meet instead of
//! substituting a weaker operation. That is the capability half of the
//! `SandboxProvider` contract in `docs/design.md` §10. The lifecycle half is
//! the [`SandboxProvider`] trait: create, exec, stop and destroy, with
//! optional checkpoint, restore, branch and share, and the optional
//! sandbox-level branching operations: pause and resume of a live sandbox,
//! live branching of a running or paused sandbox into children, and releasing
//! a checkpoint. [`Feature`] names the ones a caller chooses by
//! ([`Capabilities::has`]). [`conformance`] holds the checks every provider
//! must pass, and [`fake`] an in-process provider that models the optional
//! operations for tests. See `docs/providers.md` and
//! `docs/sandbox-snapshots.md`.
//!
//! Declarations describe what an adapter claims. They are not qualification
//! evidence: a provider profile ships only after it passes the runtime gates in
//! `docs/implementation-plan.md`.

use std::fmt;

pub mod conformance;
pub mod fake;
mod provider;

pub use provider::{
    Checkpoint, ExecSpec, ExitStatus, Mount, Process, ProviderError, Resources, SandboxInfo,
    SandboxProvider, SandboxSpec,
};

/// What a snapshot captures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SnapshotScope {
    /// Filesystem and durable data only. Restoring starts a fresh guest.
    Disk,
    /// Process memory and filesystem. Restoring continues running processes.
    Full,
}

/// What the workload observes about the moment a snapshot was taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Consistency {
    /// Taken without the workload's cooperation, as if the guest stopped.
    Crash,
    /// Taken at a point the workload reached deliberately.
    Application,
}

/// Where a snapshot can be restored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Locality {
    /// Only on the host that holds it.
    SameHost,
    /// On any eligible host, from shared storage.
    Portable,
}

/// The guarantees one snapshot path provides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SnapshotGuarantee {
    pub scope: SnapshotScope,
    pub consistency: Consistency,
    pub locality: Locality,
}

impl SnapshotGuarantee {
    /// Whether `self`, as offered, meets `required`.
    ///
    /// Scope must match exactly: a disk snapshot is not a fallback for a
    /// requested live restore, and a full restore is not a fresh guest.
    /// Consistency and locality may be stronger than required.
    pub fn satisfies(&self, required: &SnapshotGuarantee) -> bool {
        self.scope == required.scope
            && self.consistency >= required.consistency
            && self.locality >= required.locality
    }
}

/// An operation a task can require from its sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    /// Start a process with its own stdin, stdout and stderr.
    Exec,
    /// Route a network request to a listening process, activating it if idle.
    Ingress,
    /// Record the sandbox state under a durable reference.
    Checkpoint,
    /// Return a sandbox to one of its own checkpoints.
    Restore,
    /// Create a new sandbox, with a new identity, from a checkpoint.
    Branch,
    /// Attach state that another sandbox can also observe.
    Share,
    /// Freeze a running sandbox in place, keeping its memory and processes.
    Pause,
    /// Continue a paused sandbox, or one a checkpoint left stopped.
    Resume,
    /// Create new sandboxes from a running or paused one, keeping its
    /// processes and memory in each child, without a durable snapshot.
    LiveBranch,
    /// Release a checkpoint the provider holds.
    Release,
    /// Run a process whose only network is a loopback listener handed to
    /// the caller, such as an egress proxy's.
    Egress,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Operation::Exec => "exec",
            Operation::Ingress => "ingress",
            Operation::Checkpoint => "checkpoint",
            Operation::Restore => "restore",
            Operation::Branch => "branch",
            Operation::Share => "share",
            Operation::Pause => "pause",
            Operation::Resume => "resume",
            Operation::LiveBranch => "live branch",
            Operation::Release => "release a checkpoint",
            Operation::Egress => "confine egress",
        })
    }
}

/// What a provider profile declares it can do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub exec: bool,
    pub ingress: bool,
    pub checkpoint: Vec<SnapshotGuarantee>,
    pub restore: Vec<SnapshotGuarantee>,
    pub branch: Vec<SnapshotGuarantee>,
    pub share: bool,
    /// [`SandboxProvider::pause`] and [`SandboxProvider::resume`]: a
    /// sandbox can be frozen in place and continued later, by this process
    /// or another.
    pub pause: bool,
    /// [`SandboxProvider::branch_live`]: a running or paused sandbox can be
    /// branched into new sandboxes that keep its memory and processes, with
    /// their own mounts, while the source keeps its state.
    pub live_branch: bool,
    /// [`SandboxProvider::exec_confined`]: a process can be started with no
    /// network but one loopback listener, whose connections the caller
    /// accepts and forwards (Branchyard's egress proxy). Without it, a
    /// branch's egress policy is not enforced in this provider's sandboxes;
    /// see `docs/egress.md`.
    pub egress: bool,
}

/// An optional sandbox-level operation, as the engine chooses by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Feature {
    /// [`Capabilities::live_branch`].
    LiveBranch,
    /// [`Capabilities::pause`].
    Pause,
    /// A checkpoint with [`SnapshotScope::Full`]: memory and processes, not
    /// only the disk.
    FullSnapshot,
}

/// [`Feature::LiveBranch`].
pub const LIVE_BRANCH: Feature = Feature::LiveBranch;
/// [`Feature::Pause`].
pub const PAUSE: Feature = Feature::Pause;
/// [`Feature::FullSnapshot`].
pub const FULL_SNAPSHOT: Feature = Feature::FullSnapshot;

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Feature::LiveBranch => "live branch",
            Feature::Pause => "pause",
            Feature::FullSnapshot => "full snapshot",
        })
    }
}

impl Capabilities {
    /// Whether the provider declares `feature`.
    pub fn has(&self, feature: Feature) -> bool {
        match feature {
            Feature::LiveBranch => self.live_branch,
            Feature::Pause => self.pause,
            Feature::FullSnapshot => self.full_snapshot().is_some(),
        }
    }

    /// The first full-scope checkpoint guarantee declared, if any.
    pub fn full_snapshot(&self) -> Option<SnapshotGuarantee> {
        self.checkpoint
            .iter()
            .find(|g| g.scope == SnapshotScope::Full)
            .copied()
    }
}

/// What a task needs from its sandbox. Unset fields are not required.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requirements {
    pub exec: bool,
    pub ingress: bool,
    pub checkpoint: Option<SnapshotGuarantee>,
    pub restore: Option<SnapshotGuarantee>,
    pub branch: Option<SnapshotGuarantee>,
    pub share: bool,
    pub pause: bool,
    pub live_branch: bool,
    pub egress: bool,
}

/// One requirement a provider cannot meet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsupported {
    pub operation: Operation,
    /// The snapshot guarantee requested, for snapshot operations.
    pub required: Option<SnapshotGuarantee>,
    /// Every guarantee the provider offers for this operation.
    pub offered: Vec<SnapshotGuarantee>,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.required {
            None => write!(f, "provider does not support {}", self.operation),
            Some(required) => write!(
                f,
                "provider cannot {} with {:?}; offers {:?}",
                self.operation, required, self.offered
            ),
        }
    }
}

impl std::error::Error for Unsupported {}

/// Admit `required` against `offered`, reporting every unmet requirement.
///
/// Admission never weakens a request: the caller must either change its
/// requirements or select another provider.
pub fn admit(required: &Requirements, offered: &Capabilities) -> Result<(), Vec<Unsupported>> {
    let mut missing = Vec::new();
    for (operation, needed, available) in [
        (Operation::Exec, required.exec, offered.exec),
        (Operation::Ingress, required.ingress, offered.ingress),
        (Operation::Share, required.share, offered.share),
        (Operation::Pause, required.pause, offered.pause),
        (
            Operation::LiveBranch,
            required.live_branch,
            offered.live_branch,
        ),
        (Operation::Egress, required.egress, offered.egress),
    ] {
        if needed && !available {
            missing.push(Unsupported {
                operation,
                required: None,
                offered: Vec::new(),
            });
        }
    }
    for (operation, needed, available) in [
        (
            Operation::Checkpoint,
            &required.checkpoint,
            &offered.checkpoint,
        ),
        (Operation::Restore, &required.restore, &offered.restore),
        (Operation::Branch, &required.branch, &offered.branch),
    ] {
        if let Some(needed) = needed {
            if !available.iter().any(|g| g.satisfies(needed)) {
                missing.push(Unsupported {
                    operation,
                    required: Some(*needed),
                    offered: available.clone(),
                });
            }
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

/// A provider's observed sandbox lifecycle state.
///
/// Adapters map runtime states here. A state the adapter does not recognize
/// stays [`SandboxState::Unknown`]; it is never read as stopped or finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxState {
    Starting,
    Running,
    /// Frozen in place by [`SandboxProvider::pause`]: memory and processes
    /// are kept, nothing runs, and [`SandboxProvider::resume`] continues it.
    Paused,
    Stopping,
    /// Not running; its state is retained and it can be started again.
    Stopped,
    /// Stopped without a clean checkpoint; recovery is required.
    Crashed,
    Destroying,
    /// A runtime state this adapter does not recognize, kept verbatim.
    Unknown(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guarantee(
        scope: SnapshotScope,
        consistency: Consistency,
        locality: Locality,
    ) -> SnapshotGuarantee {
        SnapshotGuarantee {
            scope,
            consistency,
            locality,
        }
    }

    #[test]
    fn stronger_consistency_and_locality_satisfy_weaker_requirements() {
        let offered = guarantee(
            SnapshotScope::Full,
            Consistency::Application,
            Locality::Portable,
        );
        let required = guarantee(SnapshotScope::Full, Consistency::Crash, Locality::SameHost);
        assert!(offered.satisfies(&required));
        assert!(!required.satisfies(&offered));
    }

    #[test]
    fn scope_never_substitutes() {
        let full = guarantee(SnapshotScope::Full, Consistency::Crash, Locality::Portable);
        let disk = guarantee(SnapshotScope::Disk, Consistency::Crash, Locality::Portable);
        assert!(!full.satisfies(&disk));
        assert!(!disk.satisfies(&full));
    }

    #[test]
    fn admission_reports_every_unmet_requirement() {
        let disk = guarantee(SnapshotScope::Disk, Consistency::Crash, Locality::Portable);
        let offered = Capabilities {
            ingress: true,
            branch: vec![disk],
            ..Capabilities::default()
        };
        let full = guarantee(SnapshotScope::Full, Consistency::Crash, Locality::Portable);
        let required = Requirements {
            exec: true,
            ingress: true,
            branch: Some(full),
            ..Requirements::default()
        };
        let missing = admit(&required, &offered).unwrap_err();
        assert_eq!(
            missing,
            vec![
                Unsupported {
                    operation: Operation::Exec,
                    required: None,
                    offered: vec![],
                },
                Unsupported {
                    operation: Operation::Branch,
                    required: Some(full),
                    offered: vec![disk],
                },
            ]
        );
        assert_eq!(missing[0].to_string(), "provider does not support exec");
    }

    #[test]
    fn features_are_named_by_capability_not_by_provider() {
        let full = guarantee(SnapshotScope::Full, Consistency::Crash, Locality::SameHost);
        let disk = guarantee(SnapshotScope::Disk, Consistency::Crash, Locality::SameHost);
        let plain = Capabilities {
            exec: true,
            checkpoint: vec![disk],
            ..Capabilities::default()
        };
        for feature in [LIVE_BRANCH, PAUSE, FULL_SNAPSHOT] {
            assert!(!plain.has(feature), "{feature}");
        }
        let live = Capabilities {
            pause: true,
            live_branch: true,
            checkpoint: vec![disk, full],
            ..plain.clone()
        };
        for feature in [LIVE_BRANCH, PAUSE, FULL_SNAPSHOT] {
            assert!(live.has(feature), "{feature}");
        }
        assert_eq!(live.full_snapshot(), Some(full));
        let required = Requirements {
            pause: true,
            live_branch: true,
            ..Requirements::default()
        };
        assert_eq!(admit(&required, &live), Ok(()));
        let missing = admit(&required, &plain).unwrap_err();
        assert_eq!(missing.len(), 2);
        assert_eq!(missing[0].to_string(), "provider does not support pause");
        assert_eq!(
            missing[1].to_string(),
            "provider does not support live branch"
        );
    }

    #[test]
    fn empty_requirements_are_always_admitted() {
        assert_eq!(
            admit(&Requirements::default(), &Capabilities::default()),
            Ok(())
        );
    }
}
