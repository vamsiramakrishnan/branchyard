//! [`LocalProvider`]: sandboxes that are only names, and processes that run
//! on this host as this user.
//!
//! What it guarantees:
//!
//! - Each exec runs in its own process group, so [`Process::teardown`],
//!   [`SandboxProvider::stop`] and dropping the process reach every
//!   descendant that stays in the group. `teardown` names them first.
//! - The process's environment is exactly [`ExecSpec::env`]: nothing is
//!   inherited from this process.
//!
//! - With [`LocalProvider::spawn_confined`] (and `exec_confined`), on Linux
//!   where unprivileged user and network namespaces are allowed, the
//!   process and its descendants have no network but one listener the
//!   caller serves ([`crate::egress`]).
//!
//! What it does not guarantee:
//!
//! - Isolation of any kind, beyond that confinement. A "sandbox" is a record; the process sees the
//!   host filesystem, network and every file this user can read. Mounts are
//!   identity only (host path equals sandbox path) and must be writable,
//!   since nothing could enforce read-only access. An image or resource
//!   limit is refused rather than ignored.
//! - Reaching a descendant that leaves its process group, such as a daemon
//!   calling `setsid`.
//! - Process-group reuse safety. Teardown signals the group by ID after the
//!   launched process has been reaped; if every member had already exited
//!   and the ID was reused, an unrelated group would be signalled. The same
//!   window exists for any tool that kills a group after reaping its leader.

use branchyard_support::LockExt as _;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use std::net::TcpListener;

use branchyard_sandbox::{
    Capabilities, ExecSpec, ExitStatus, Mount, Operation, Process, ProviderError, SandboxInfo,
    SandboxProvider, SandboxSpec, SandboxState,
};

/// Runs processes on this host. See the module documentation.
#[derive(Debug, Default)]
pub struct LocalProvider {
    sandboxes: Mutex<HashMap<String, Local>>,
}

#[derive(Debug)]
struct Local {
    running: bool,
    /// Process groups started here and not yet torn down.
    groups: Vec<Weak<Group>>,
}

/// A process group, marked once it has been torn down so nothing signals
/// its ID again.
#[derive(Debug)]
struct Group {
    pgid: u32,
    done: AtomicBool,
}

impl Group {
    fn kill(&self) {
        if !self.done.load(Ordering::Acquire) {
            signal_group(self.pgid);
        }
    }
}

impl LocalProvider {
    /// A provider with no sandbox: harnesses run as local processes.
    pub fn new() -> LocalProvider {
        LocalProvider::default()
    }

    /// Start `spec` directly, outside any named sandbox: in its own process
    /// group, without a shell, with exactly `spec.env`.
    pub fn spawn(spec: &ExecSpec) -> io::Result<LocalProcess> {
        let mut command = LocalProvider::command(spec)?;
        LocalProcess::new(command.spawn()?)
    }

    /// Like [`LocalProvider::spawn`], but confined to its own network
    /// namespace whose only way out is the returned listener, bound to
    /// `127.0.0.1:port` inside it: Linux only, where unprivileged user and
    /// network namespaces are allowed ([`LocalProvider::confinement`]).
    pub fn spawn_confined(spec: &ExecSpec, port: u16) -> io::Result<(LocalProcess, TcpListener)> {
        #[cfg(target_os = "linux")]
        {
            let mut command = LocalProvider::command(spec)?;
            let (child, listener) = crate::netns::spawn(&mut command, port)?;
            Ok((LocalProcess::new(child)?, listener))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (spec, port);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                LocalProvider::confinement().unwrap_err(),
            ))
        }
    }

    /// Whether [`LocalProvider::spawn_confined`] works on this host, and
    /// why not; detected once per process.
    pub fn confinement() -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            crate::netns::supported()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err("confining a local process's network needs Linux network namespaces".into())
        }
    }

    fn command(spec: &ExecSpec) -> io::Result<Command> {
        let Some((program, args)) = spec.argv.split_first() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty argument vector",
            ));
        };
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so teardown reaches every descendant.
            .process_group(0);
        Ok(command)
    }

    fn check(spec: &SandboxSpec) -> Result<(), ProviderError> {
        spec.validate()?;
        if let Some(image) = &spec.image {
            return Err(ProviderError::Invalid(format!(
                "the local provider runs host executables and cannot boot image {image}"
            )));
        }
        if !spec.resources.is_unlimited() {
            return Err(ProviderError::Invalid(
                "the local provider cannot enforce CPU or memory limits".into(),
            ));
        }
        for Mount {
            host,
            guest,
            writable,
        } in &spec.mounts
        {
            if host != guest {
                return Err(ProviderError::Invalid(format!(
                    "the local provider cannot remap {} to {}",
                    host.display(),
                    guest.display()
                )));
            }
            if !writable {
                return Err(ProviderError::Invalid(format!(
                    "the local provider cannot make {} read-only",
                    host.display()
                )));
            }
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Local>> {
        self.sandboxes.lock_recovering("sandboxes")
    }
}

impl SandboxProvider for LocalProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            exec: true,
            egress: LocalProvider::confinement().is_ok(),
            ..Capabilities::default()
        }
    }

    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
        LocalProvider::check(spec)?;
        let mut sandboxes = self.lock();
        let local = sandboxes.entry(spec.name.clone()).or_insert(Local {
            running: true,
            groups: Vec::new(),
        });
        local.running = true;
        Ok(SandboxInfo {
            name: spec.name.clone(),
            state: SandboxState::Running,
        })
    }

    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        Ok(self.lock().get(name).map(|local| SandboxInfo {
            name: name.to_owned(),
            state: match local.running {
                true => SandboxState::Running,
                false => SandboxState::Stopped,
            },
        }))
    }

    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        let mut sandboxes = self.lock();
        let local = sandboxes
            .get_mut(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        if !local.running {
            return Err(ProviderError::Runtime(format!("sandbox {name} is stopped")));
        }
        let process = LocalProvider::spawn(spec)?;
        local.groups.retain(|group| group.strong_count() > 0);
        local.groups.push(Arc::downgrade(&process.group));
        Ok(Box::new(process))
    }

    fn exec_confined(
        &self,
        name: &str,
        spec: &ExecSpec,
        port: u16,
    ) -> Result<(Box<dyn Process>, TcpListener), ProviderError> {
        if LocalProvider::confinement().is_err() {
            return Err(ProviderError::unsupported(Operation::Egress));
        }
        let mut sandboxes = self.lock();
        let local = sandboxes
            .get_mut(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        if !local.running {
            return Err(ProviderError::Runtime(format!("sandbox {name} is stopped")));
        }
        let (process, listener) = LocalProvider::spawn_confined(spec, port)?;
        local.groups.retain(|group| group.strong_count() > 0);
        local.groups.push(Arc::downgrade(&process.group));
        Ok((Box::new(process), listener))
    }

    fn stop(&self, name: &str) -> Result<(), ProviderError> {
        let mut sandboxes = self.lock();
        let local = sandboxes
            .get_mut(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        for group in local.groups.drain(..).filter_map(|g| g.upgrade()) {
            group.kill();
        }
        local.running = false;
        Ok(())
    }

    fn destroy(&self, name: &str) -> Result<(), ProviderError> {
        if let Some(local) = self.lock().remove(name) {
            for group in local.groups.iter().filter_map(|g| g.upgrade()) {
                group.kill();
            }
        }
        Ok(())
    }
}

/// A process started by [`LocalProvider`].
pub struct LocalProcess {
    child: Child,
    stdin: Option<Box<dyn Write + Send>>,
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
    group: Arc<Group>,
    reaped: bool,
}

impl std::fmt::Debug for LocalProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalProcess")
            .field("pid", &self.child.id())
            .field("reaped", &self.reaped)
            .finish_non_exhaustive()
    }
}

impl LocalProcess {
    fn new(mut child: Child) -> io::Result<LocalProcess> {
        let group = Arc::new(Group {
            pgid: child.id(),
            done: AtomicBool::new(false),
        });
        Ok(LocalProcess {
            stdin: child
                .stdin
                .take()
                .map(|p| Box::new(p) as Box<dyn Write + Send>),
            stdout: child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
            stderr: child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
            child,
            group,
            reaped: false,
        })
    }

    /// The operating-system process ID of the harness.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

fn status(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus {
        code: status.code(),
        signal: status.signal(),
    }
}

impl Process for LocalProcess {
    fn id(&self) -> String {
        self.child.id().to_string()
    }

    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        self.stdin.take()
    }

    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stderr.take()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let exited = self.child.try_wait()?;
        self.reaped |= exited.is_some();
        Ok(exited.map(status))
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        let exited = self.child.wait()?;
        self.reaped = true;
        Ok(status(exited))
    }

    /// SIGKILL to the launched process only; [`Process::teardown`] reaches
    /// the rest of its group.
    fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    fn teardown(&mut self) -> Vec<String> {
        let survivors = group_members(self.group.pgid);
        signal_group(self.group.pgid);
        self.group.done.store(true, Ordering::Release);
        survivors
    }
}

impl Drop for LocalProcess {
    fn drop(&mut self) {
        if !self.group.done.load(Ordering::Acquire) {
            signal_group(self.group.pgid);
            self.group.done.store(true, Ordering::Release);
        }
        if !self.reaped {
            branchyard_support::best_effort("kill child", self.child.kill());
            branchyard_support::best_effort("reap child", self.child.wait());
        }
    }
}

/// Command names of the live (non-zombie) members of process group `pgid`:
/// from `/proc` on Linux, else from `ps`. Empty when neither is available.
/// A member forked but not yet exec'd still carries its parent's name,
/// such as `sh` for a shell's background job.
fn group_members(pgid: u32) -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(|pid| {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                // `pid (comm) state ppid pgrp ...`; comm may hold spaces
                // and parentheses, so split around its last `)`.
                let open = stat.find('(')?;
                let close = stat.rfind(')')?;
                let name = stat.get(open + 1..close)?.to_owned();
                let mut fields = stat.get(close + 1..)?.split_whitespace();
                let state = fields.next()?;
                let group = fields.nth(1)?.parse::<u32>().ok()?;
                (group == pgid && state != "Z" && !name.is_empty()).then_some(name)
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let Ok(listing) = Command::new("ps")
            .args(["-A", "-o", "pgid=", "-o", "stat=", "-o", "comm="])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            return Vec::new();
        };
        let pgid = pgid.to_string();
        String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let (group, stat) = (fields.next()?, fields.next()?);
                let name = fields.collect::<Vec<_>>().join(" ");
                (group == pgid && !stat.starts_with('Z') && !name.is_empty()).then_some(name)
            })
            .collect()
    }
}

/// SIGKILL the whole process group, through killpg(2).
fn signal_group(pgid: u32) {
    branchyard_support::kill_group(pgid);
}
