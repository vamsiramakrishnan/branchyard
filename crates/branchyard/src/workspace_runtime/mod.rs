//! Operator-controlled interaction with an idle branch's actual execution site.
//!
//! The branch lease excludes native turns while a terminal or app request uses
//! its files. Remote interaction requires a retained allocation; it never falls
//! back to this host. Existing placement transfer and sandbox retention own
//! copying, parking and cleanup. Server callers must authorize every action.

mod lifecycle;
mod preview;
mod terminal;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use branchyard_sandbox::{ExitStatus, Process};
use branchyard_support::{best_effort, time::now_ms};
use serde::{Deserialize, Serialize};

use crate::placement::{Placement, SandboxPlan};
use crate::state::{Lease, Record, SandboxKind, Taken};
use crate::{Error, RecordedEvent, Yard};

pub use lifecycle::{RuntimeAction, RuntimeState};
pub use preview::{PreviewPort, PreviewResponse};
pub use terminal::{Terminal, TerminalOptions, TerminalOutput};

/// The provider allocation reported for a branch, without filesystem paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    /// Provider label used by the branch.
    pub provider: String,
    /// Retained provider allocation, absent for local execution.
    pub allocation: Option<String>,
    /// Whether this provider runs outside the server's local process space.
    pub remote: bool,
    /// Host/boot that last ran this branch's native turn.
    pub owner_host: Option<String>,
    /// Branch-reserved development port, never a caller-selected host port.
    pub port: Option<u16>,
}

/// A leased workspace at its original provider, ready for one helper process.
///
/// Acquiring this value does not run a model or change its native conversation.
/// Dropping it returns files and parks the sandbox through existing retention.
pub struct WorkspaceRuntime {
    yard: Yard,
    record: Record,
    placement: Option<Placement>,
    lease: Option<Lease>,
    info: RuntimeInfo,
}

impl Yard {
    /// Lease an idle branch and prepare its original workspace execution site.
    ///
    /// Remote branches must already own a retained sandbox. Running or stale
    /// leases are refused, not recovered by replaying an uncertain model turn.
    pub fn workspace_runtime(&self, branch: &str) -> Result<WorkspaceRuntime, Error> {
        let store = self.store();
        let record = store.read(branch)?;
        let lease = match store.acquire(&record)? {
            Taken::Granted(lease) => lease,
            Taken::Stale => return Err(Error::Unsupported(
                "the branch has a stale execution lease; recover it before opening its workspace"
                    .into(),
            )),
        };
        let kind = crate::providers::of(record.provider.as_ref());
        let owner_host = store.backend().execution_host(branch)?;
        match owner_host.as_deref() {
            Some(host) if host == crate::proc::host() => {}
            Some(_) => return Err(Error::Unsupported("this workspace belongs to another worker host; connect to its owning worker for terminal or app access".into())),
            None => return Err(Error::Unsupported("this workspace has no verified execution host; run a native turn on its owning worker first".into())),
        }
        kind.check(self)?;
        let remote = kind.sandboxed();
        let allocation = if remote {
            let row = store
                .sandboxes()
                .sandboxes(branch)?
                .into_iter()
                .find(|row| {
                    row.kind == SandboxKind::Kept
                        && row.incarnation == lease.fence().incarnation
                        && row.provider == kind.key()
                });
            Some(row.ok_or_else(|| Error::Unsupported(
                "this branch has no retained remote allocation; enable pause retention before its next turn".into(),
            ))?.name)
        } else {
            None
        };
        let placement = Placement::prepare(self, &record, lease.fence(), &SandboxPlan::Retained)
            .map_err(Error::State)?;
        let info = RuntimeInfo {
            provider: kind.label(),
            allocation,
            remote,
            owner_host,
            port: store.ports().port(branch)?,
        };
        Ok(WorkspaceRuntime {
            yard: self.clone(),
            record,
            placement: Some(placement),
            lease: Some(lease),
            info,
        })
    }
}

impl WorkspaceRuntime {
    /// The execution site's identity and branch-reserved port.
    pub fn info(&self) -> &RuntimeInfo {
        &self.info
    }

    /// Start an operator-controlled argument vector at this workspace.
    ///
    /// Ownership moves into the process, so its branch lease cannot be dropped
    /// while the process still uses its files. Do not accept this argv from an
    /// untrusted HTTP caller; terminal and preview helpers choose it themselves.
    pub fn exec(self, argv: Vec<String>) -> Result<WorkspaceProcess, Error> {
        if argv.is_empty() || argv.iter().any(|value| value.contains('\0')) {
            return Err(Error::Unsupported(
                "a workspace helper needs a valid executable".into(),
            ));
        }
        let placement = self
            .placement
            .as_ref()
            .ok_or_else(|| Error::State("workspace is closed".into()))?;
        self.check_lease()?;
        let (_, home) = crate::placement::guest_paths(&self.record);
        let mut env = BTreeMap::from([
            (
                OsString::from("PATH"),
                OsString::from("/usr/local/bin:/usr/bin:/bin"),
            ),
            (OsString::from("HOME"), OsString::from(home)),
            (OsString::from("TERM"), OsString::from("xterm-256color")),
            (OsString::from("LANG"), OsString::from("C.UTF-8")),
        ]);
        if let Some(port) = self.info.port {
            env.insert("BRANCHYARD_PORT".into(), port.to_string().into());
        }
        let mut process = placement.exec_workspace(argv, env).map_err(Error::State)?;
        let service = if !self.info.remote {
            let pid = process
                .id()
                .parse::<u32>()
                .map_err(|_| Error::State("local helper has no process identity".into()))?;
            match crate::services::Reclaim::process(pid, true) {
                Some(reclaim) => Some(
                    self.yard.register_service(
                        crate::services::Service::new(
                            "workspace_process",
                            crate::services::ServiceOwner::this_process()
                                .for_branch(&self.record.info.name),
                        )
                        .with_reclaim(reclaim),
                        crate::services::DEFAULT_TTL,
                    )?,
                ),
                None if process.try_wait()?.is_some() => None,
                None => {
                    return Err(Error::State(
                        "cannot identify live workspace helper for recovery".into(),
                    ))
                }
            }
        } else {
            None
        };
        Ok(WorkspaceProcess {
            process,
            runtime: Some(self),
            _service: service,
        })
    }

    /// Copy files back and park the allocation using the existing retention owner.
    pub fn close(mut self) -> Result<(), Error> {
        self.release()
    }

    fn release(&mut self) -> Result<(), Error> {
        let Some(lease) = self.lease.take() else {
            return Ok(());
        };
        if lease.lost() {
            if let Some(placement) = self.placement.as_mut() {
                placement.detach_workspace();
            }
            return Err(Error::State(
                "interactive workspace lost its branch lease; recovery owns its allocation".into(),
            ));
        }
        let activities = self.placement.as_mut().map_or_else(Vec::new, |placement| {
            placement.release(&self.yard, &self.record, lease.fence())
        });
        let transfer_error = self
            .placement
            .as_ref()
            .and_then(Placement::transfer_error)
            .map(str::to_owned);
        for activity in activities {
            self.yard.store().append(
                &self.record.info.name,
                &RecordedEvent {
                    at_ms: now_ms(),
                    activity,
                },
                Some(lease.fence()),
            )?;
        }
        lease.finish(None, None)?;
        if let Some(error) = transfer_error {
            return Err(Error::State(error));
        }
        Ok(())
    }

    fn check_lease(&self) -> Result<(), Error> {
        if self.lease.as_ref().is_none_or(Lease::lost) {
            return Err(Error::State(
                "interactive workspace lost its branch lease".into(),
            ));
        }
        Ok(())
    }
}

impl Drop for WorkspaceRuntime {
    fn drop(&mut self) {
        best_effort("release interactive workspace", self.release());
    }
}

/// A helper process that retains its workspace lease and provider placement.
pub struct WorkspaceProcess {
    process: Box<dyn Process>,
    runtime: Option<WorkspaceRuntime>,
    _service: Option<crate::services::Registration>,
}

impl WorkspaceProcess {
    fn register_pty(&self, pid: u32) -> Result<Option<crate::services::Registration>, Error> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| Error::State("workspace helper is closed".into()))?;
        if runtime.info.remote {
            return Ok(None);
        }
        let reclaim = crate::services::Reclaim::process(pid, true)
            .ok_or_else(|| Error::State("cannot identify terminal PTY for recovery".into()))?;
        Ok(Some(
            runtime.yard.register_service(
                crate::services::Service::new(
                    "workspace_terminal_pty",
                    crate::services::ServiceOwner::this_process()
                        .for_branch(&runtime.record.info.name),
                )
                .with_reclaim(reclaim),
                crate::services::DEFAULT_TTL,
            )?,
        ))
    }

    fn check_lease(&self) -> Result<(), Error> {
        self.runtime
            .as_ref()
            .ok_or_else(|| Error::State("workspace helper is closed".into()))?
            .check_lease()
    }
    /// Capture bounded stdout, with a wall deadline and provider-group teardown.
    pub fn output(mut self, max_bytes: usize, timeout: Duration) -> Result<Vec<u8>, Error> {
        let output = self
            .process
            .take_stdout()
            .ok_or_else(|| Error::State("workspace helper has no stdout".into()))?;
        let errors = self.process.take_stderr();
        let (send, receive) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut bytes = Vec::new();
            let result = output
                .take(max_bytes.saturating_add(1) as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            best_effort("deliver workspace helper output", send.send(result));
        });
        let error_reader = errors.map(|stream| {
            std::thread::spawn(move || {
                use std::io::Read as _;
                let mut sink = io::sink();
                best_effort(
                    "drain workspace helper stderr",
                    io::copy(&mut stream.take(64 * 1024), &mut sink),
                );
            })
        });
        let started = Instant::now();
        let result = (|| {
            let bytes = loop {
                self.check_lease()?;
                let left = timeout.saturating_sub(started.elapsed());
                if left.is_zero() {
                    return Err(Error::State("workspace helper timed out".into()));
                }
                match receive.recv_timeout(left.min(Duration::from_millis(250))) {
                    Ok(bytes) => break bytes?,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(Error::State(
                            "workspace helper output reader stopped".into(),
                        ))
                    }
                }
            };
            if bytes.len() > max_bytes {
                return Err(Error::Unsupported(
                    "workspace helper output exceeds its byte limit".into(),
                ));
            }
            loop {
                self.check_lease()?;
                if let Some(status) = self.process.try_wait()? {
                    return if status.success() {
                        Ok(bytes)
                    } else {
                        Err(Error::State(format!("workspace helper failed: {status}")))
                    };
                }
                let left = timeout.saturating_sub(started.elapsed());
                if left.is_zero() {
                    return Err(Error::State("workspace helper timed out".into()));
                }
                std::thread::park_timeout(left.min(Duration::from_millis(10)));
            }
        })();
        if result.is_err() && !matches!(self.process.try_wait(), Ok(Some(_))) {
            best_effort("stop failed workspace helper", self.process.kill());
        }
        self.process.teardown();
        reader
            .join()
            .map_err(|_| Error::State("workspace helper output reader stopped".into()))?;
        if let Some(reader) = error_reader {
            reader
                .join()
                .map_err(|_| Error::State("workspace helper error reader stopped".into()))?;
        }
        let bytes = result?;
        if let Some(runtime) = self.runtime.take() {
            runtime.close()?;
        }
        Ok(bytes)
    }
}

impl Process for WorkspaceProcess {
    fn id(&self) -> String {
        self.process.id()
    }
    fn take_stdin(&mut self) -> Option<Box<dyn io::Write + Send>> {
        self.process.take_stdin()
    }
    fn take_stdout(&mut self) -> Option<Box<dyn io::Read + Send>> {
        self.process.take_stdout()
    }
    fn take_stderr(&mut self) -> Option<Box<dyn io::Read + Send>> {
        self.process.take_stderr()
    }
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.process.try_wait()
    }
    fn wait(&mut self) -> io::Result<ExitStatus> {
        self.process.wait()
    }
    fn kill(&mut self) -> io::Result<()> {
        self.process.kill()
    }
    fn teardown(&mut self) -> Vec<String> {
        self.process.teardown()
    }
}

impl Drop for WorkspaceProcess {
    fn drop(&mut self) {
        self.process.teardown();
    }
}
