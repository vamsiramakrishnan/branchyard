//! [`MicrosandboxProvider`]: the lifecycle mapped onto the Microsandbox SDK.
//!
//! | Contract | SDK 0.7.3 call |
//! |---|---|
//! | `ensure` | `Sandbox::builder(name).image(..).cpus(..).memory(..).volume(guest, \|m\| m.bind(host))` then `create()` (attached: the microVM dies with this process) |
//! | `inspect` | `Sandbox::status()`, or `Sandbox::get(name)` and `status_snapshot()` |
//! | `exec` | `Sandbox::exec_stream_with(program, \|e\| e.args(..).cwd(..).envs(..).stdin_pipe())` |
//! | `Process::kill` | `ExecControl::kill()`: agentd signals the exec's process group |
//! | `Process::teardown` | `Sandbox::exec_with("sh", ..)` running [`GROUP_TEARDOWN`] |
//! | `stop` | `Sandbox::stop_with_timeout`, then `Sandbox::kill` if that fails |
//! | `destroy` | `Sandbox::destroy()` (stop and remove) |
//! | `checkpoint` | `Snapshot::builder(label).from_sandbox(name).create()` (disk), `.full()` for a full-scope one |
//! | `branch` | `Sandbox::restore(label).name(..).cpus(..).memory(..).volume(..).restore()`, `.forked()` from a full checkpoint |
//! | `pause`, `resume` | `Sandbox::pause()`/`resume()`, or through `Sandbox::get(name)` from another process |
//! | `branch_live` | `SandboxHandle::branch(name).volume(..).branch()`, or `branch_many(names)` when every child has the same mounts |
//! | `release_checkpoint` | `Snapshot::remove(label, true)` |
//!
//! Pause, resume, live branching and full-scope checkpoints are declared only
//! with [`MicrosandboxProvider::with_live_branch`] (the `live_branch = true`
//! opt-in), until they are qualified on a KVM host. A spec with
//! [`SandboxSpec::persist`] is created detached (`create_detached`), so it
//! outlives this process and a later one adopts it by name
//! (`Sandbox::get(name)` then `connect()`); every other sandbox this provider
//! holds is destroyed when it is dropped.
//!
//! The SDK is asynchronous; a private Tokio runtime drives it, and every
//! trait method blocks on it. Call them from ordinary threads, never from
//! inside a Tokio runtime. Drop every [`Process`] before its provider.

use branchyard_support::LockExt as _;
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use branchyard_sandbox::{
    Capabilities, Checkpoint, ExecSpec, Operation, Process, ProviderError, SandboxInfo,
    SandboxProvider, SandboxSpec, SandboxState, SnapshotGuarantee, SnapshotScope, Unsupported,
};
use microsandbox::protocol::exec::ExecFailureKind;
use microsandbox::sandbox::exec::ExecSink;
use microsandbox::sandbox::SandboxStatus;
use microsandbox::{ExecControl, ExecEvent, ExecHandle, MicrosandboxError, Sandbox, Snapshot};
use tokio::runtime::{Handle, Runtime};

use crate::bridge::{self, BridgedProcess, GuestControl, GuestEvent, GuestEvents, GROUP_TEARDOWN};
use crate::plan::{self, CreatePlan, PlannedMount};

/// How long a graceful stop may take before the microVM is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the in-guest teardown may take.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(20);

/// Sandboxes on the local Microsandbox runtime. See the crate documentation
/// for prerequisites and guarantees.
pub struct MicrosandboxProvider {
    runtime: Option<Runtime>,
    sandboxes: Mutex<HashMap<String, Arc<Sandbox>>>,
    /// Sandboxes that outlive this provider: created with
    /// [`SandboxSpec::persist`], or adopted from another process.
    kept: Mutex<HashSet<String>>,
    live_branch: bool,
}

impl std::fmt::Debug for MicrosandboxProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = self.lock().keys().cloned().collect();
        f.debug_struct("MicrosandboxProvider")
            .field("sandboxes", &names)
            .finish_non_exhaustive()
    }
}

impl MicrosandboxProvider {
    /// Start the private runtime. Nothing contacts the Microsandbox runtime
    /// until the first `ensure`.
    pub fn new() -> io::Result<MicrosandboxProvider> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("microsandbox")
            .enable_all()
            .build()?;
        Ok(MicrosandboxProvider {
            runtime: Some(runtime),
            sandboxes: Mutex::new(HashMap::new()),
            kept: Mutex::new(HashSet::new()),
            live_branch: false,
        })
    }

    /// Declare and allow pause, resume, live branching and full-scope
    /// checkpoints ([`plan::capabilities_with`]). Unqualified: see
    /// `docs/sandbox-snapshots.md`.
    pub fn with_live_branch(mut self, on: bool) -> MicrosandboxProvider {
        self.live_branch = on;
        self
    }

    fn keep(&self, name: &str) {
        self.kept.lock_recovering("kept").insert(name.to_owned());
    }

    fn live(&self, operation: Operation) -> Result<(), ProviderError> {
        match self.live_branch {
            true => Ok(()),
            false => Err(ProviderError::unsupported(operation)),
        }
    }

    /// The sandbox named `name`: held, or adopted from the runtime (a
    /// detached sandbox another process created) and connected.
    fn adopt(&self, name: &str) -> Result<Arc<Sandbox>, ProviderError> {
        if let Ok(held) = self.held(name) {
            return Ok(held);
        }
        let connect = async {
            let handle = Sandbox::get(name).await?;
            handle.connect().await
        };
        let sandbox = Arc::new(self.handle().block_on(connect).map_err(error)?);
        self.lock().insert(name.to_owned(), sandbox.clone());
        self.keep(name);
        Ok(sandbox)
    }

    #[allow(clippy::expect_used)] // ratchet: branchyard-microsandbox
    fn handle(&self) -> &Handle {
        self.runtime
            .as_ref()
            .expect("the runtime lives until drop")
            .handle()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<Sandbox>>> {
        self.sandboxes.lock_recovering("sandboxes")
    }

    fn held(&self, name: &str) -> Result<Arc<Sandbox>, ProviderError> {
        self.lock()
            .get(name)
            .cloned()
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))
    }
}

impl Drop for MicrosandboxProvider {
    #[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-microsandbox
    fn drop(&mut self) {
        let kept = std::mem::take(&mut *self.kept.lock_recovering("kept"));
        let held: Vec<Arc<Sandbox>> = self
            .lock()
            .drain()
            .filter(|(name, _)| !kept.contains(name))
            .map(|(_, s)| s)
            .collect();
        if let Some(runtime) = self.runtime.take() {
            for sandbox in held {
                let _ = runtime.block_on(async {
                    tokio::time::timeout(STOP_TIMEOUT, sandbox.destroy()).await
                });
            }
            runtime.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

/// The contract's view of a runtime status.
pub fn state(status: SandboxStatus) -> SandboxState {
    match status {
        // Not running, state retained, can be started.
        SandboxStatus::Created | SandboxStatus::Stopped => SandboxState::Stopped,
        SandboxStatus::Starting => SandboxState::Starting,
        SandboxStatus::Running => SandboxState::Running,
        SandboxStatus::Draining => SandboxState::Stopping,
        SandboxStatus::Crashed => SandboxState::Crashed,
        // Frozen but resident.
        SandboxStatus::Paused => SandboxState::Paused,
    }
}

/// The contract's view of an SDK error.
pub fn error(error: MicrosandboxError) -> ProviderError {
    match error {
        MicrosandboxError::SandboxNotFound(name) => ProviderError::NotFound(name),
        MicrosandboxError::InvalidConfig(reason) => ProviderError::Invalid(reason),
        MicrosandboxError::Io(error) => ProviderError::Io(error),
        MicrosandboxError::ExecFailed(failed) => {
            ProviderError::Io(io::Error::new(failure_kind(failed.kind), failed.message))
        }
        other => ProviderError::Runtime(other.to_string()),
    }
}

/// The I/O error kind for an exec that could not start.
pub fn failure_kind(kind: ExecFailureKind) -> io::ErrorKind {
    match kind {
        ExecFailureKind::NotFound => io::ErrorKind::NotFound,
        ExecFailureKind::PermissionDenied => io::ErrorKind::PermissionDenied,
        ExecFailureKind::BadCwd => io::ErrorKind::NotADirectory,
        ExecFailureKind::BadArgs => io::ErrorKind::InvalidInput,
        ExecFailureKind::ResourceLimit => io::ErrorKind::ResourceBusy,
        _ => io::ErrorKind::Other,
    }
}

/// The contract's view of one exec event.
pub fn event(event: ExecEvent) -> GuestEvent {
    match event {
        ExecEvent::Started { pid } => GuestEvent::Started { pid },
        ExecEvent::Stdout(bytes) => GuestEvent::Stdout(bytes.to_vec()),
        ExecEvent::Stderr(bytes) => GuestEvent::Stderr(bytes.to_vec()),
        ExecEvent::Exited { code } => GuestEvent::Exited { code },
        ExecEvent::Failed(failed) => GuestEvent::Failed {
            kind: failure_kind(failed.kind),
            message: failed.message,
        },
        ExecEvent::StdinError(error) => GuestEvent::StdinError(format!("{error:?}")),
    }
}

fn unsupported(
    operation: Operation,
    required: &SnapshotGuarantee,
    offered: Vec<SnapshotGuarantee>,
) -> ProviderError {
    ProviderError::Unsupported(Unsupported {
        operation,
        required: Some(*required),
        offered,
    })
}

/// Apply planned mounts to a builder that takes `volume(guest, |m| ..)`.
fn bind<B>(mut builder: B, mounts: Vec<PlannedMount>, volume: impl Fn(B, PlannedMount) -> B) -> B {
    for mount in mounts {
        builder = volume(builder, mount);
    }
    builder
}

fn mount_with(
    mount: PlannedMount,
) -> impl FnOnce(microsandbox::sandbox::MountBuilder) -> microsandbox::sandbox::MountBuilder {
    move |m| {
        let m = m.bind(mount.host);
        if mount.readonly {
            m.readonly()
        } else {
            m
        }
    }
}

struct Events {
    runtime: Handle,
    exec: ExecHandle,
}

impl GuestEvents for Events {
    fn next(&mut self) -> Option<GuestEvent> {
        self.runtime.block_on(self.exec.recv()).map(event)
    }
}

struct Control {
    runtime: Handle,
    exec: ExecControl,
    stdin: Mutex<Option<ExecSink>>,
    sandbox: Arc<Sandbox>,
}

impl GuestControl for Control {
    fn write_stdin(&self, data: &[u8]) -> io::Result<()> {
        let stdin = self.stdin.lock_recovering("stdin");
        let Some(sink) = stdin.as_ref() else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin is closed"));
        };
        self.runtime
            .block_on(sink.write(data))
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))
    }

    fn close_stdin(&self) -> io::Result<()> {
        let sink = self.stdin.lock_recovering("stdin").take();
        match sink {
            Some(sink) => self
                .runtime
                .block_on(sink.close())
                .map_err(|e| io::Error::other(e.to_string())),
            None => Ok(()),
        }
    }

    fn kill(&self) -> io::Result<()> {
        self.runtime
            .block_on(self.exec.kill())
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn teardown(&self, pgid: u32) -> io::Result<Vec<String>> {
        let pgid = pgid.to_string();
        let run = self.sandbox.exec_with("sh", |e| {
            e.args(["-c", GROUP_TEARDOWN, "sh", pgid.as_str()])
                .timeout(TEARDOWN_TIMEOUT)
        });
        let output = self
            .runtime
            .block_on(run)
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(bridge::survivors(&String::from_utf8_lossy(
            output.stdout_bytes(),
        )))
    }
}

impl SandboxProvider for MicrosandboxProvider {
    fn capabilities(&self) -> Capabilities {
        plan::capabilities_with(self.live_branch)
    }

    #[allow(clippy::expect_used)] // ratchet: branchyard-microsandbox
    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
        let CreatePlan {
            name,
            image,
            cpus,
            memory_mib,
            mounts,
        } = plan::create(spec)?;
        if self.lock().contains_key(&name) {
            return self
                .inspect(&name)?
                .ok_or_else(|| ProviderError::NotFound(name.clone()));
        }
        let image = image.expect("a create plan has an image");
        let mut builder = Sandbox::builder(name.as_str()).image(image);
        if let Some(cpus) = cpus {
            builder = builder.cpus(cpus);
        }
        if let Some(memory) = memory_mib {
            builder = builder.memory(memory);
        }
        builder = bind(builder, mounts, |b, mount| {
            b.volume(mount.guest.clone(), mount_with(mount))
        });
        let sandbox = match spec.persist {
            true => self.handle().block_on(builder.create_detached()),
            false => self.handle().block_on(builder.create()),
        }
        .map_err(error)?;
        self.lock().insert(name.clone(), Arc::new(sandbox));
        if spec.persist {
            self.keep(&name);
        }
        Ok(SandboxInfo {
            name,
            state: SandboxState::Running,
        })
    }

    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        let held = self.lock().get(name).cloned();
        let status = match held {
            Some(sandbox) => self.handle().block_on(sandbox.status()),
            None => self
                .handle()
                .block_on(Sandbox::get(name))
                .map(|handle| handle.status_snapshot()),
        };
        match status {
            Ok(status) => Ok(Some(SandboxInfo {
                name: name.to_owned(),
                state: state(status),
            })),
            Err(MicrosandboxError::SandboxNotFound(_)) => Ok(None),
            Err(other) => Err(error(other)),
        }
    }

    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        let sandbox = self.adopt(name)?;
        let plan = plan::exec(spec)?;
        let start = sandbox.exec_stream_with(plan.program, |e| {
            e.args(plan.args).cwd(plan.cwd).envs(plan.env).stdin_pipe()
        });
        let mut exec = self.handle().block_on(start).map_err(error)?;
        let control = Arc::new(Control {
            runtime: self.handle().clone(),
            exec: exec.control(),
            stdin: Mutex::new(exec.take_stdin()),
            sandbox,
        });
        let events = Events {
            runtime: self.handle().clone(),
            exec,
        };
        let process = BridgedProcess::start(Box::new(events), control)?;
        Ok(Box::new(process))
    }

    fn stop(&self, name: &str) -> Result<(), ProviderError> {
        let sandbox = self.adopt(name)?;
        let graceful = self
            .handle()
            .block_on(sandbox.stop_with_timeout(STOP_TIMEOUT));
        if graceful.is_err() {
            self.handle().block_on(sandbox.kill()).map_err(error)?;
        }
        Ok(())
    }

    fn destroy(&self, name: &str) -> Result<(), ProviderError> {
        let held = self.lock().remove(name);
        self.kept.lock_recovering("kept").remove(name);
        let destroyed = match held {
            Some(sandbox) => self.handle().block_on(sandbox.destroy()),
            None => match self.handle().block_on(Sandbox::get(name)) {
                Ok(handle) => self.handle().block_on(handle.destroy()),
                Err(error) => Err(error),
            },
        };
        match destroyed {
            Ok(()) | Err(MicrosandboxError::SandboxNotFound(_)) => Ok(()),
            Err(other) => Err(error(other)),
        }
    }

    /// A live disk snapshot of the guest's root filesystem. Bind-mounted
    /// host directories, the workspace included, are not captured.
    fn checkpoint(
        &self,
        name: &str,
        required: &SnapshotGuarantee,
    ) -> Result<Checkpoint, ProviderError> {
        let offered = self.capabilities().checkpoint;
        let Some(guarantee) = offered.iter().find(|g| g.satisfies(required)).copied() else {
            return Err(unsupported(Operation::Checkpoint, required, offered));
        };
        let label = format!("{name}-checkpoint-{}", branchyard_support::time::now_ms());
        plan::name(&label)?;
        let builder = Snapshot::builder(label.as_str()).from_sandbox(name);
        let builder = match guarantee.scope {
            SnapshotScope::Full => builder.full(),
            SnapshotScope::Disk => builder,
        };
        self.handle().block_on(builder.create()).map_err(error)?;
        Ok(Checkpoint {
            sandbox: name.to_owned(),
            reference: label,
            guarantee,
        })
    }

    /// Boot a new sandbox from a disk checkpoint, or restore a full one
    /// with private copy-on-write memory (`forked`). The checkpoint fixes
    /// the root filesystem, so `spec.image` is not used; its limits (which
    /// must match a full checkpoint's) and mounts are.
    fn branch(
        &self,
        checkpoint: &Checkpoint,
        spec: &SandboxSpec,
    ) -> Result<SandboxInfo, ProviderError> {
        let offered = self.capabilities().branch;
        if !offered.contains(&checkpoint.guarantee) {
            return Err(unsupported(
                Operation::Branch,
                &checkpoint.guarantee,
                offered,
            ));
        }
        let plan = plan::branch(spec)?;
        if self.lock().contains_key(&plan.name) {
            return Err(ProviderError::Invalid(format!(
                "a sandbox named {} already exists",
                plan.name
            )));
        }
        let mut restore = Sandbox::restore(checkpoint.reference.as_str()).name(plan.name.as_str());
        if let Some(cpus) = plan.cpus {
            restore = restore.cpus(cpus);
        }
        if let Some(memory) = plan.memory_mib {
            restore = restore.memory(memory);
        }
        restore = bind(restore, plan.mounts, |r, mount| {
            r.volume(mount.guest.clone(), mount_with(mount))
        });
        if checkpoint.guarantee.scope == SnapshotScope::Full {
            restore = restore.forked();
        } else {
            restore = restore.disk_only();
        }
        // A restore is always detached.
        let sandbox = self.handle().block_on(restore.restore()).map_err(error)?;
        self.lock().insert(plan.name.clone(), Arc::new(sandbox));
        if spec.persist {
            self.keep(&plan.name);
        }
        Ok(SandboxInfo {
            name: plan.name,
            state: SandboxState::Running,
        })
    }

    fn pause(&self, name: &str) -> Result<(), ProviderError> {
        self.live(Operation::Pause)?;
        let held = self.lock().get(name).cloned();
        let paused = match held {
            Some(sandbox) => self.handle().block_on(sandbox.pause()),
            None => self.handle().block_on(async {
                let handle = Sandbox::get(name).await?;
                handle.pause().await
            }),
        };
        paused.map_err(error)
    }

    /// Resume a paused sandbox, or start a stopped one detached, and
    /// connect to it for exec.
    fn resume(&self, name: &str) -> Result<SandboxInfo, ProviderError> {
        self.live(Operation::Resume)?;
        let resumed = self.handle().block_on(async {
            let handle = Sandbox::get(name).await?;
            match handle.status_snapshot() {
                SandboxStatus::Paused => {
                    handle.resume().await?;
                    handle.connect().await
                }
                SandboxStatus::Running => handle.connect().await,
                _ => handle.start_detached().await,
            }
        });
        let sandbox = resumed.map_err(error)?;
        self.lock().insert(name.to_owned(), Arc::new(sandbox));
        self.keep(name);
        Ok(SandboxInfo {
            name: name.to_owned(),
            state: SandboxState::Running,
        })
    }

    /// `Sandbox::branch` per child, each with its own mounts, or one
    /// `branch_many` when every child has the same mounts. Children are
    /// detached; the source keeps its running or paused state. Local only.
    fn branch_live(
        &self,
        source: &str,
        children: &[SandboxSpec],
    ) -> Vec<Result<SandboxInfo, ProviderError>> {
        if let Err(error) = self.live(Operation::LiveBranch) {
            let why = error.to_string();
            return children
                .iter()
                .map(|_| Err(ProviderError::Runtime(why.clone())))
                .collect();
        }
        let plans: Vec<Result<CreatePlan, ProviderError>> =
            children.iter().map(plan::live_child).collect();
        let valid: Vec<CreatePlan> = plans
            .iter()
            .filter_map(|p| p.as_ref().ok().cloned())
            .collect();
        let source_handle = self.handle().block_on(Sandbox::get(source));
        let handle = match source_handle {
            Ok(handle) => handle,
            Err(e) => {
                let why = error(e).to_string();
                return children
                    .iter()
                    .map(|_| Err(ProviderError::Runtime(why.clone())))
                    .collect();
            }
        };
        let mut made: HashMap<String, Result<Sandbox, ProviderError>> = HashMap::new();
        if valid.len() == plans.len() && plan::one_batch(&valid) {
            let names: Vec<String> = valid.iter().map(|p| p.name.clone()).collect();
            let builder = bind(
                handle.branch_many(names),
                valid[0].mounts.clone(),
                |b, mount| b.volume(mount.guest.clone(), mount_with(mount)),
            );
            match self.handle().block_on(builder.branch()) {
                Ok(outcomes) => {
                    for outcome in outcomes {
                        made.insert(outcome.name, outcome.result.map_err(error));
                    }
                }
                Err(e) => {
                    let why = error(e).to_string();
                    for name in valid.iter().map(|p| p.name.clone()) {
                        made.insert(name, Err(ProviderError::Runtime(why.clone())));
                    }
                }
            }
        } else {
            for child in &valid {
                let builder = bind(
                    handle.branch(child.name.as_str()),
                    child.mounts.clone(),
                    |b, mount| b.volume(mount.guest.clone(), mount_with(mount)),
                );
                made.insert(
                    child.name.clone(),
                    self.handle().block_on(builder.branch()).map_err(error),
                );
            }
        }
        plans
            .into_iter()
            .zip(children)
            .map(|(plan, spec)| {
                let plan = plan?;
                let sandbox = made
                    .remove(&plan.name)
                    .unwrap_or_else(|| Err(ProviderError::Runtime("no outcome".into())))?;
                self.lock().insert(plan.name.clone(), Arc::new(sandbox));
                if spec.persist {
                    self.keep(&plan.name);
                }
                Ok(SandboxInfo {
                    name: plan.name,
                    state: SandboxState::Running,
                })
            })
            .collect()
    }

    fn release_checkpoint(&self, checkpoint: &Checkpoint) -> Result<(), ProviderError> {
        match self
            .handle()
            .block_on(Snapshot::remove(checkpoint.reference.as_str(), true))
        {
            Ok(()) => Ok(()),
            Err(e) => match error(e) {
                ProviderError::NotFound(_) => Ok(()),
                other => Err(other),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::DISK_SNAPSHOT;

    #[test]
    fn every_runtime_status_maps() {
        assert_eq!(state(SandboxStatus::Running), SandboxState::Running);
        assert_eq!(state(SandboxStatus::Starting), SandboxState::Starting);
        assert_eq!(state(SandboxStatus::Draining), SandboxState::Stopping);
        assert_eq!(state(SandboxStatus::Stopped), SandboxState::Stopped);
        assert_eq!(state(SandboxStatus::Created), SandboxState::Stopped);
        assert_eq!(state(SandboxStatus::Crashed), SandboxState::Crashed);
        assert_eq!(state(SandboxStatus::Paused), SandboxState::Paused);
    }

    #[test]
    fn errors_and_failures_map_to_contract_errors() {
        assert!(matches!(
            error(MicrosandboxError::SandboxNotFound("x".into())),
            ProviderError::NotFound(name) if name == "x"
        ));
        assert!(matches!(
            error(MicrosandboxError::InvalidConfig("bad".into())),
            ProviderError::Invalid(_)
        ));
        assert!(matches!(
            error(MicrosandboxError::Runtime("boom".into())),
            ProviderError::Runtime(_)
        ));
        assert_eq!(
            failure_kind(ExecFailureKind::NotFound),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            failure_kind(ExecFailureKind::BadCwd),
            io::ErrorKind::NotADirectory
        );
    }

    #[test]
    fn exec_events_map_one_to_one() {
        assert_eq!(
            event(ExecEvent::Started { pid: 5 }),
            GuestEvent::Started { pid: 5 }
        );
        assert_eq!(
            event(ExecEvent::Stdout(b"x".as_slice().into())),
            GuestEvent::Stdout(b"x".to_vec())
        );
        assert_eq!(
            event(ExecEvent::Exited { code: 2 }),
            GuestEvent::Exited { code: 2 }
        );
    }

    #[test]
    fn the_provider_starts_without_a_runtime_and_declares_its_plan() {
        let provider = MicrosandboxProvider::new().unwrap();
        assert_eq!(provider.capabilities(), plan::capabilities());
        assert!(matches!(
            provider.pause("absent"),
            Err(ProviderError::Unsupported(_))
        ));
        assert!(provider.branch_live("absent", &[SandboxSpec::new("c")])[0].is_err());
        let live = MicrosandboxProvider::new().unwrap().with_live_branch(true);
        assert_eq!(live.capabilities(), plan::capabilities_with(true));
        assert!(matches!(
            provider.exec("absent", &ExecSpec::default()),
            Err(ProviderError::NotFound(_))
        ));
        let full = SnapshotGuarantee {
            scope: SnapshotScope::Full,
            ..DISK_SNAPSHOT
        };
        assert!(matches!(
            provider.checkpoint("absent", &full),
            Err(ProviderError::Unsupported(_))
        ));
        assert!(matches!(
            provider.restore(
                "absent",
                &Checkpoint {
                    sandbox: "absent".into(),
                    reference: "r".into(),
                    guarantee: DISK_SNAPSHOT,
                }
            ),
            Err(ProviderError::Unsupported(_))
        ));
    }
}
