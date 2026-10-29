//! Idempotent resource controls using the branch's lease and retained row.

use branchyard_sandbox::{SandboxProvider, SandboxState};
use branchyard_support::time::now_ms;
use serde::{Deserialize, Serialize};

use super::RuntimeInfo;
use crate::state::{Begun, SandboxKind, SandboxRow, Taken};
use crate::{Error, Yard};

/// An action on an existing idle provider allocation, never on a model turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAction {
    /// Freeze on the provider's node while retaining its memory.
    Pause,
    /// Stop with provider-retained state, portable where that provider promises it.
    Suspend,
    /// Resume the retained allocation without starting a model turn.
    Resume,
    /// Release only the allocation, preserving task files and immutable outputs.
    Destroy,
}

/// Observed allocation state and the provider's declared resource controls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeState {
    /// Allocation identity and owning provider.
    pub runtime: RuntimeInfo,
    /// Provider-observed state, or `absent` after release.
    pub state: String,
    /// Why interactive access cannot run on this server, if unavailable.
    pub reason: Option<String>,
    /// The provider declares node-local pause.
    pub can_pause: bool,
    /// The provider declares retained-state resume.
    pub can_resume: bool,
    /// A retained remote allocation can be stopped through this provider.
    pub can_suspend: bool,
    /// A retained remote allocation can be released.
    pub can_destroy: bool,
}

#[derive(Serialize, Deserialize)]
struct Intent {
    action: RuntimeAction,
    allocation: Option<SandboxRow>,
}

fn observed(provider: &dyn SandboxProvider, name: &str) -> Result<String, Error> {
    Ok(
        match provider
            .inspect(name)
            .map_err(|error| Error::State(error.to_string()))?
        {
            None => "absent".into(),
            Some(info) => match info.state {
                SandboxState::Starting => "starting".into(),
                SandboxState::Running => "running".into(),
                SandboxState::Paused => "paused".into(),
                SandboxState::Stopping => "stopping".into(),
                SandboxState::Stopped => "stopped".into(),
                SandboxState::Crashed => "crashed".into(),
                SandboxState::Destroying => "destroying".into(),
                SandboxState::Unknown(value) => format!("unknown: {value}"),
            },
        },
    )
}

impl Yard {
    /// Inspect a retained runtime without resuming it or taking a branch lease.
    /// The returned state is an observation, not permission to mutate it.
    pub fn workspace_runtime_state(&self, branch: &str) -> Result<RuntimeState, Error> {
        let store = self.store();
        let record = store.read(branch)?;
        let kind = crate::providers::of(record.provider.as_ref());
        let mut info = RuntimeInfo {
            provider: kind.label(),
            allocation: None,
            remote: kind.sandboxed(),
            owner_host: store.backend().execution_host(branch)?,
            port: store.ports().port(branch)?,
        };
        let owner = info.owner_host.as_deref();
        if owner != Some(crate::proc::host()) {
            let reason = if owner.is_some() {
                "connect to the worker that owns this workspace"
            } else {
                "run a native turn to establish the execution host"
            };
            return Ok(RuntimeState {
                runtime: info,
                state: "unavailable".into(),
                reason: Some(reason.into()),
                can_pause: false,
                can_resume: false,
                can_suspend: false,
                can_destroy: false,
            });
        }
        if !info.remote {
            return Ok(RuntimeState {
                runtime: info,
                state: "local".into(),
                reason: None,
                can_pause: false,
                can_resume: false,
                can_suspend: false,
                can_destroy: false,
            });
        }
        kind.check(self)?;
        let provider = kind.open(self).map_err(Error::State)?;
        let caps = provider.capabilities();
        let incarnation = store
            .storage()
            .identities()?
            .into_iter()
            .find(|identity| identity.name == branch)
            .ok_or_else(|| Error::UnknownBranch(branch.into()))?
            .incarnation;
        let row = store
            .sandboxes()
            .sandboxes(branch)?
            .into_iter()
            .rev()
            .find(|row| {
                row.kind == SandboxKind::Kept
                    && row.provider == kind.key()
                    && row.incarnation == incarnation
            });
        let state = if let Some(row) = row {
            let state = observed(provider.as_ref(), &row.name)?;
            info.allocation = Some(row.name);
            state
        } else {
            "absent".into()
        };
        let retained = info.allocation.is_some() && state != "absent";
        Ok(RuntimeState {
            runtime: info,
            state,
            reason: None,
            can_pause: caps.pause && retained,
            can_resume: caps.pause && retained,
            can_suspend: retained,
            can_destroy: retained,
        })
    }

    /// Apply a provider action to an idle branch's existing retained allocation.
    ///
    /// `operation_id` must be the authenticated server operation's stable ID.
    /// Repeating it returns the recorded outcome; conflicting reuse is refused.
    /// Intent precedes provider effects. A resumed retry uses the same allocation,
    /// and no model call is replayed. Server authorization remains the caller's job.
    pub fn workspace_runtime_action(
        &self,
        branch: &str,
        action: RuntimeAction,
        operation_id: &str,
    ) -> Result<RuntimeState, Error> {
        if operation_id.is_empty()
            || operation_id.len() > 128
            || !operation_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return Err(Error::Unsupported(
                "runtime control needs a stable operation identifier".into(),
            ));
        }
        let store = self.store();
        let record = store.read(branch)?;
        let lease = match store.acquire(&record)? {
            Taken::Granted(lease) => lease,
            Taken::Stale => {
                return Err(Error::Unsupported(
                    "recover the stale execution lease before controlling its runtime".into(),
                ))
            }
        };
        let kind = crate::providers::of(record.provider.as_ref());
        if !kind.sandboxed() {
            return Err(Error::Unsupported(
                "a local workspace has no remote allocation to control".into(),
            ));
        }
        kind.check(self)?;
        let provider = kind.open(self).map_err(Error::State)?;
        let capabilities = provider.capabilities();
        if matches!(action, RuntimeAction::Pause | RuntimeAction::Resume) && !capabilities.pause {
            return Err(Error::Unsupported(
                "this provider does not support pause and resume".into(),
            ));
        }
        let row = store
            .sandboxes()
            .sandboxes(branch)?
            .into_iter()
            .find(|row| {
                row.kind == SandboxKind::Kept
                    && row.incarnation == lease.fence().incarnation
                    && row.provider == kind.key()
            });
        let step = format!("workspace-runtime:{operation_id}");
        let requested = Intent {
            action,
            allocation: row,
        };
        let value =
            serde_json::to_value(&requested).map_err(|error| Error::State(error.to_string()))?;
        let intent = match store
            .backend()
            .begin_step(lease.fence(), 0, &step, &value)?
        {
            Begun::Done(outcome) => {
                let original = store
                    .backend()
                    .steps(branch, 0)?
                    .into_iter()
                    .find(|s| s.step == step);
                if let Some(original) = original {
                    let original: Intent = serde_json::from_value(original.intent)
                        .map_err(|error| Error::State(error.to_string()))?;
                    if original.action != action {
                        return Err(Error::Denied(
                            "runtime operation ID was used for another action".into(),
                        ));
                    }
                }
                return serde_json::from_value(outcome)
                    .map_err(|error| Error::State(error.to_string()));
            }
            Begun::Pending(value) => {
                serde_json::from_value(value).map_err(|error| Error::State(error.to_string()))?
            }
            Begun::Fresh => requested,
        };
        if intent.action != action {
            return Err(Error::Denied(
                "runtime operation ID was used for another action".into(),
            ));
        }
        let row = intent.allocation.ok_or_else(|| {
            Error::Unsupported("this branch has no retained remote allocation".into())
        })?;
        if row.incarnation != lease.fence().incarnation || row.provider != kind.key() {
            return Err(Error::Denied(
                "runtime operation belongs to an earlier branch or another provider".into(),
            ));
        }
        let claimed = store
            .sandboxes()
            .take_sandbox(branch, row.kind, &row.name)?;
        if claimed.is_none() {
            return Err(Error::Unsupported(
                "the retained allocation has no confirmed owner; reconcile it before retrying"
                    .into(),
            ));
        }
        store
            .backend()
            .renew(lease.fence(), crate::state::LEASE_TTL)?;
        let acted = match action {
            RuntimeAction::Pause => provider.pause(&row.name),
            RuntimeAction::Suspend => provider.stop(&row.name),
            RuntimeAction::Resume => provider.resume(&row.name).map(|_| ()),
            RuntimeAction::Destroy => provider.destroy(&row.name),
        };
        if action != RuntimeAction::Destroy || acted.is_err() {
            let mut retained = row.clone();
            retained.used_ms = now_ms();
            store
                .sandboxes()
                .put_sandbox_fenced(&retained, lease.fence())?;
        }
        acted.map_err(|error| Error::State(error.to_string()))?;
        let state = observed(provider.as_ref(), &row.name)?;
        let retained = action != RuntimeAction::Destroy && state != "absent";
        let result = RuntimeState {
            runtime: RuntimeInfo {
                provider: kind.label(),
                allocation: Some(row.name),
                remote: true,
                owner_host: store.backend().execution_host(branch)?,
                port: store.ports().port(branch)?,
            },
            state,
            reason: None,
            can_pause: capabilities.pause && retained,
            can_resume: capabilities.pause && retained,
            can_suspend: retained,
            can_destroy: retained,
        };
        store.backend().finish_step(
            lease.fence(),
            0,
            &step,
            &serde_json::to_value(&result).map_err(|error| Error::State(error.to_string()))?,
        )?;
        lease.finish(None, None)?;
        Ok(result)
    }
}
