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
//! | `checkpoint` | `Snapshot::builder(label).from_sandbox(name).create()` (disk) |
//! | `branch` | `Sandbox::restore(label).name(..).cpus(..).memory(..).volume(..).restore()` |
//!
//! The SDK is asynchronous; a private Tokio runtime drives it, and every
//! trait method blocks on it. Call them from ordinary threads, never from
//! inside a Tokio runtime. Drop every [`Process`] before its provider.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
use crate::plan::{self, CreatePlan, DISK_SNAPSHOT};

/// How long a graceful stop may take before the microVM is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the in-guest teardown may take.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(20);

/// Sandboxes on the local Microsandbox runtime. See the crate documentation
/// for prerequisites and guarantees.
pub struct MicrosandboxProvider {
    runtime: Option<Runtime>,
    sandboxes: Mutex<HashMap<String, Arc<Sandbox>>>,
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
        })
    }

    fn handle(&self) -> &Handle {
        self.runtime
            .as_ref()
            .expect("the runtime lives until drop")
            .handle()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<Sandbox>>> {
        self.sandboxes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn held(&self, name: &str) -> Result<Arc<Sandbox>, ProviderError> {
        self.lock()
            .get(name)
            .cloned()
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))
    }
}

impl Drop for MicrosandboxProvider {
    fn drop(&mut self) {
        let held: Vec<Arc<Sandbox>> = self.lock().drain().map(|(_, s)| s).collect();
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
        // Frozen but resident: neither running nor stopped in the contract.
        SandboxStatus::Paused => SandboxState::Unknown("paused".into()),
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

fn unsupported(operation: Operation, required: &SnapshotGuarantee) -> ProviderError {
    ProviderError::Unsupported(Unsupported {
        operation,
        required: Some(*required),
        offered: vec![DISK_SNAPSHOT],
    })
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
        let stdin = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sink) = stdin.as_ref() else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin is closed"));
        };
        self.runtime
            .block_on(sink.write(data))
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))
    }

    fn close_stdin(&self) -> io::Result<()> {
        let sink = self.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
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

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

impl SandboxProvider for MicrosandboxProvider {
    fn capabilities(&self) -> Capabilities {
        plan::capabilities()
    }

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
        for mount in mounts {
            builder = builder.volume(mount.guest, move |m| {
                let m = m.bind(mount.host);
                if mount.readonly {
                    m.readonly()
                } else {
                    m
                }
            });
        }
        let sandbox = self.handle().block_on(builder.create()).map_err(error)?;
        self.lock().insert(name.clone(), Arc::new(sandbox));
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
        let sandbox = self.held(name)?;
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
        let sandbox = self.held(name)?;
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
        if !DISK_SNAPSHOT.satisfies(required) {
            return Err(unsupported(Operation::Checkpoint, required));
        }
        self.held(name)?;
        let label = format!("{name}-checkpoint-{}", now_ms());
        plan::name(&label)?;
        let create = Snapshot::builder(label.as_str())
            .from_sandbox(name)
            .create();
        self.handle().block_on(create).map_err(error)?;
        Ok(Checkpoint {
            sandbox: name.to_owned(),
            reference: label,
            guarantee: DISK_SNAPSHOT,
        })
    }

    /// Boot a new sandbox from a disk checkpoint. The checkpoint fixes the
    /// root filesystem, so `spec.image` is not used; its limits and mounts
    /// are.
    fn branch(
        &self,
        checkpoint: &Checkpoint,
        spec: &SandboxSpec,
    ) -> Result<SandboxInfo, ProviderError> {
        if checkpoint.guarantee.scope != SnapshotScope::Disk {
            return Err(unsupported(Operation::Branch, &checkpoint.guarantee));
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
        for mount in plan.mounts {
            restore = restore.volume(mount.guest, move |m| {
                let m = m.bind(mount.host);
                if mount.readonly {
                    m.readonly()
                } else {
                    m
                }
            });
        }
        let sandbox = self.handle().block_on(restore.restore()).map_err(error)?;
        self.lock().insert(plan.name.clone(), Arc::new(sandbox));
        Ok(SandboxInfo {
            name: plan.name,
            state: SandboxState::Running,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_runtime_status_maps() {
        assert_eq!(state(SandboxStatus::Running), SandboxState::Running);
        assert_eq!(state(SandboxStatus::Starting), SandboxState::Starting);
        assert_eq!(state(SandboxStatus::Draining), SandboxState::Stopping);
        assert_eq!(state(SandboxStatus::Stopped), SandboxState::Stopped);
        assert_eq!(state(SandboxStatus::Created), SandboxState::Stopped);
        assert_eq!(state(SandboxStatus::Crashed), SandboxState::Crashed);
        assert_eq!(
            state(SandboxStatus::Paused),
            SandboxState::Unknown("paused".into())
        );
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
