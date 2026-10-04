//! [`FakeProvider`]: an in-process provider that models the optional
//! sandbox-level operations (pause, resume, live branch, checkpoints and
//! their release) for hermetic tests of code that chooses between them.
//!
//! A fake sandbox is a record plus a private directory standing in for its
//! root filesystem. Processes run on this host through an inner provider
//! (such as the local provider), with the sandbox's mounts applied by path:
//! an exec's working directory and every variable whose value is a sandbox
//! path under a mount are mapped to the host path, and each exec gets
//! [`ROOTFS_ENV`] naming the sandbox's root-filesystem directory and
//! [`SANDBOX_ENV`] naming the sandbox. What a process writes there is
//! private to the sandbox, carried into every child a live branch or a
//! checkpoint makes, and deleted with it; that is how a test sees state
//! "inside" a sandbox survive a branch.
//!
//! What it models, by the capabilities it is given:
//!
//! - `pause` freezes a sandbox's record: execs are refused until `resume`.
//!   Host processes are not stopped.
//! - `branch_live` copies the source's root filesystem into each child,
//!   which gets its own spec (name and mounts) and starts running; the
//!   source keeps its state, a paused one stays paused.
//! - `checkpoint` copies the root filesystem under a new reference;
//!   `branch` creates a running sandbox from one; `release_checkpoint`
//!   deletes it.
//! - [`FakeProvider::vanish`] removes a sandbox behind its user's back, as a
//!   host reboot or an operator would.
//!
//! Every call is recorded ([`FakeProvider::ops`]) so tests can count them.
//! Nothing here is evidence about a real provider: memory, processes and
//! isolation are not modelled.

use branchyard_support::LockExt as _;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::provider::{
    Checkpoint, ExecSpec, Process, ProviderError, SandboxInfo, SandboxProvider, SandboxSpec,
};
use crate::{
    Capabilities, Consistency, Locality, Operation, SandboxState, SnapshotGuarantee, SnapshotScope,
    Unsupported,
};

/// The variable naming a fake sandbox's root-filesystem directory.
pub const ROOTFS_ENV: &str = "BY_FAKE_ROOTFS";
/// The variable naming the fake sandbox an exec runs in.
pub const SANDBOX_ENV: &str = "BY_FAKE_SANDBOX";

/// The snapshot guarantee [`FakeProvider::live`] declares: full scope,
/// crash consistency, same host, like a local microVM's.
pub const FULL_SAME_HOST: SnapshotGuarantee = SnapshotGuarantee {
    scope: SnapshotScope::Full,
    consistency: Consistency::Crash,
    locality: Locality::SameHost,
};

/// One call a [`FakeProvider`] received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FakeOp {
    Ensure {
        name: String,
    },
    Exec {
        sandbox: String,
        argv: Vec<String>,
        /// As the caller gave it: a sandbox path.
        cwd: PathBuf,
    },
    Pause {
        name: String,
    },
    Resume {
        name: String,
    },
    Stop {
        name: String,
    },
    Destroy {
        name: String,
    },
    Checkpoint {
        name: String,
        reference: String,
    },
    Branch {
        reference: String,
        child: String,
    },
    BranchLive {
        source: String,
        children: Vec<String>,
    },
    Release {
        reference: String,
    },
}

struct Sandbox {
    spec: SandboxSpec,
    state: SandboxState,
    rootfs: PathBuf,
}

#[derive(Default)]
struct State {
    sandboxes: BTreeMap<String, Sandbox>,
    checkpoints: BTreeMap<String, PathBuf>,
    ops: Vec<FakeOp>,
    next: u64,
    failing: Vec<Operation>,
    destroy_fails: bool,
}

/// See the module documentation.
pub struct FakeProvider {
    inner: Box<dyn SandboxProvider>,
    root: PathBuf,
    capabilities: Capabilities,
    state: Mutex<State>,
}

impl std::fmt::Debug for FakeProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeProvider")
            .field("root", &self.root)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

impl FakeProvider {
    /// A fake whose processes run through `inner`, whose root filesystems
    /// live under `root`, and which declares `capabilities`. Operations it
    /// does not declare are refused as unsupported.
    pub fn new(
        inner: Box<dyn SandboxProvider>,
        root: impl Into<PathBuf>,
        capabilities: Capabilities,
    ) -> FakeProvider {
        FakeProvider {
            inner,
            root: root.into(),
            capabilities,
            state: Mutex::new(State::default()),
        }
    }

    /// Everything: exec, pause, live branch, and full same-host
    /// checkpoints that branch.
    pub fn live(inner: Box<dyn SandboxProvider>, root: impl Into<PathBuf>) -> FakeProvider {
        FakeProvider::new(
            inner,
            root,
            Capabilities {
                exec: true,
                checkpoint: vec![FULL_SAME_HOST],
                branch: vec![FULL_SAME_HOST],
                pause: true,
                live_branch: true,
                ..Capabilities::default()
            },
        )
    }

    /// Exec only, like a provider that can neither pause nor branch.
    pub fn plain(inner: Box<dyn SandboxProvider>, root: impl Into<PathBuf>) -> FakeProvider {
        FakeProvider::new(
            inner,
            root,
            Capabilities {
                exec: true,
                ..Capabilities::default()
            },
        )
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock_recovering("state")
    }

    /// Every call so far, in order.
    pub fn ops(&self) -> Vec<FakeOp> {
        self.lock().ops.clone()
    }

    /// The sandboxes that exist, with their states.
    pub fn sandboxes(&self) -> Vec<(String, SandboxState)> {
        self.lock()
            .sandboxes
            .iter()
            .map(|(name, s)| (name.clone(), s.state.clone()))
            .collect()
    }

    /// The spec a sandbox was created or branched with.
    pub fn spec(&self, name: &str) -> Option<SandboxSpec> {
        self.lock().sandboxes.get(name).map(|s| s.spec.clone())
    }

    /// A sandbox's root-filesystem directory.
    pub fn rootfs(&self, name: &str) -> Option<PathBuf> {
        self.lock().sandboxes.get(name).map(|s| s.rootfs.clone())
    }

    /// The checkpoint references held.
    pub fn checkpoints(&self) -> Vec<String> {
        self.lock().checkpoints.keys().cloned().collect()
    }

    /// Make every later call of `operation` fail with a runtime error
    /// (`on`), or succeed again.
    pub fn set_failing(&self, operation: Operation, on: bool) {
        let mut state = self.lock();
        state.failing.retain(|o| *o != operation);
        if on {
            state.failing.push(operation);
        }
    }

    /// Make every later `destroy` fail with a runtime error (`on`), or
    /// succeed again.
    pub fn set_destroy_failing(&self, on: bool) {
        self.lock().destroy_fails = on;
    }

    /// Remove `name` without recording a call, as if it vanished.
    pub fn vanish(&self, name: &str) {
        if let Some(sandbox) = self.lock().sandboxes.remove(name) {
            branchyard_support::cleanup_dir(sandbox.rootfs);
        }
        branchyard_support::best_effort("self.inner.destroy", self.inner.destroy(name));
    }

    fn refuse(&self, operation: Operation) -> Result<(), ProviderError> {
        let declared = match operation {
            Operation::Pause | Operation::Resume => self.capabilities.pause,
            Operation::LiveBranch => self.capabilities.live_branch,
            Operation::Checkpoint | Operation::Release => !self.capabilities.checkpoint.is_empty(),
            Operation::Branch => !self.capabilities.branch.is_empty(),
            _ => true,
        };
        if !declared {
            return Err(ProviderError::unsupported(operation));
        }
        if self.lock().failing.contains(&operation) {
            return Err(ProviderError::Runtime(format!(
                "the fake was told to fail {operation}"
            )));
        }
        Ok(())
    }

    fn new_rootfs(&self, state: &mut State, name: &str) -> PathBuf {
        state.next += 1;
        self.root
            .join("rootfs")
            .join(format!("{name}-{}", state.next))
    }

    /// Record a new running sandbox from `spec`, its root filesystem copied
    /// from `seed` (empty when `None`).
    fn create(
        &self,
        spec: &SandboxSpec,
        seed: Option<&Path>,
    ) -> Result<SandboxInfo, ProviderError> {
        spec.validate()?;
        let rootfs = {
            let mut state = self.lock();
            if state.sandboxes.contains_key(&spec.name) {
                return Err(ProviderError::Invalid(format!(
                    "a sandbox named {} already exists",
                    spec.name
                )));
            }
            self.new_rootfs(&mut state, &spec.name)
        };
        match seed {
            Some(seed) => copy_dir(seed, &rootfs)?,
            None => fs::create_dir_all(&rootfs)?,
        }
        self.inner.ensure(&SandboxSpec::new(&spec.name))?;
        self.lock().sandboxes.insert(
            spec.name.clone(),
            Sandbox {
                spec: spec.clone(),
                state: SandboxState::Running,
                rootfs,
            },
        );
        Ok(SandboxInfo {
            name: spec.name.clone(),
            state: SandboxState::Running,
        })
    }
}

impl SandboxProvider for FakeProvider {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
        self.lock().ops.push(FakeOp::Ensure {
            name: spec.name.clone(),
        });
        if let Some(sandbox) = self.lock().sandboxes.get(&spec.name) {
            return Ok(SandboxInfo {
                name: spec.name.clone(),
                state: sandbox.state.clone(),
            });
        }
        self.create(spec, None)
    }

    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        Ok(self.lock().sandboxes.get(name).map(|s| SandboxInfo {
            name: name.to_owned(),
            state: s.state.clone(),
        }))
    }

    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        let (mapped, rootfs) = {
            let mut state = self.lock();
            state.ops.push(FakeOp::Exec {
                sandbox: name.to_owned(),
                argv: spec.argv.clone(),
                cwd: spec.cwd.clone(),
            });
            let sandbox = state
                .sandboxes
                .get(name)
                .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
            if sandbox.state != SandboxState::Running {
                return Err(ProviderError::Runtime(format!(
                    "sandbox {name} is {:?}, not running",
                    sandbox.state
                )));
            }
            (sandbox.spec.clone(), sandbox.rootfs.clone())
        };
        let cwd = match mapped.host_path(&spec.cwd) {
            Some(host) => host,
            None if spec.cwd == Path::new("/") => rootfs.clone(),
            None => {
                return Err(ProviderError::Invalid(format!(
                    "the fake can only run in a mounted directory, not {}",
                    spec.cwd.display()
                )))
            }
        };
        let mut env: BTreeMap<OsString, OsString> = spec
            .env
            .iter()
            .map(|(name, value)| {
                let value = mapped
                    .host_path(Path::new(value))
                    .map(OsString::from)
                    .unwrap_or_else(|| value.clone());
                (name.clone(), value)
            })
            .collect();
        if !env.contains_key(&OsString::from("PATH")) {
            if let Some(path) = std::env::var_os("PATH") {
                env.insert("PATH".into(), path);
            }
        }
        env.insert(ROOTFS_ENV.into(), rootfs.into());
        env.insert(SANDBOX_ENV.into(), name.into());
        self.inner.exec(
            name,
            &ExecSpec {
                argv: spec.argv.clone(),
                cwd,
                env,
            },
        )
    }

    fn stop(&self, name: &str) -> Result<(), ProviderError> {
        self.lock().ops.push(FakeOp::Stop {
            name: name.to_owned(),
        });
        {
            let mut state = self.lock();
            let sandbox = state
                .sandboxes
                .get_mut(name)
                .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
            sandbox.state = SandboxState::Stopped;
        }
        self.inner.stop(name)
    }

    fn destroy(&self, name: &str) -> Result<(), ProviderError> {
        self.lock().ops.push(FakeOp::Destroy {
            name: name.to_owned(),
        });
        if self.lock().destroy_fails {
            return Err(ProviderError::Runtime("the fake would not destroy".into()));
        }
        let removed = self.lock().sandboxes.remove(name);
        if let Some(sandbox) = removed {
            branchyard_support::cleanup_dir(sandbox.rootfs);
        }
        self.inner.destroy(name)
    }

    fn checkpoint(
        &self,
        name: &str,
        required: &SnapshotGuarantee,
    ) -> Result<Checkpoint, ProviderError> {
        self.refuse(Operation::Checkpoint)?;
        let offered = self.capabilities.checkpoint.clone();
        let Some(guarantee) = offered.iter().find(|g| g.satisfies(required)).copied() else {
            return Err(ProviderError::Unsupported(Unsupported {
                operation: Operation::Checkpoint,
                required: Some(*required),
                offered,
            }));
        };
        let (rootfs, reference, target) = {
            let mut state = self.lock();
            let rootfs = state
                .sandboxes
                .get(name)
                .map(|s| s.rootfs.clone())
                .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
            state.next += 1;
            let reference = format!("{name}-checkpoint-{}", state.next);
            let target = self.root.join("checkpoints").join(&reference);
            (rootfs, reference, target)
        };
        copy_dir(&rootfs, &target)?;
        let mut state = self.lock();
        state.checkpoints.insert(reference.clone(), target);
        state.ops.push(FakeOp::Checkpoint {
            name: name.to_owned(),
            reference: reference.clone(),
        });
        Ok(Checkpoint {
            sandbox: name.to_owned(),
            reference,
            guarantee,
        })
    }

    fn branch(
        &self,
        checkpoint: &Checkpoint,
        spec: &SandboxSpec,
    ) -> Result<SandboxInfo, ProviderError> {
        self.refuse(Operation::Branch)?;
        self.lock().ops.push(FakeOp::Branch {
            reference: checkpoint.reference.clone(),
            child: spec.name.clone(),
        });
        let seed = self
            .lock()
            .checkpoints
            .get(&checkpoint.reference)
            .cloned()
            .ok_or_else(|| ProviderError::NotFound(checkpoint.reference.clone()))?;
        self.create(spec, Some(&seed))
    }

    fn pause(&self, name: &str) -> Result<(), ProviderError> {
        self.refuse(Operation::Pause)?;
        let mut state = self.lock();
        state.ops.push(FakeOp::Pause {
            name: name.to_owned(),
        });
        let sandbox = state
            .sandboxes
            .get_mut(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        match sandbox.state {
            SandboxState::Running | SandboxState::Paused => {
                sandbox.state = SandboxState::Paused;
                Ok(())
            }
            ref other => Err(ProviderError::Runtime(format!(
                "cannot pause {name}: it is {other:?}"
            ))),
        }
    }

    fn resume(&self, name: &str) -> Result<SandboxInfo, ProviderError> {
        self.refuse(Operation::Resume)?;
        let stopped = {
            let mut state = self.lock();
            state.ops.push(FakeOp::Resume {
                name: name.to_owned(),
            });
            let sandbox = state
                .sandboxes
                .get_mut(name)
                .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
            let stopped = sandbox.state == SandboxState::Stopped;
            sandbox.state = SandboxState::Running;
            stopped
        };
        if stopped {
            self.inner.ensure(&SandboxSpec::new(name))?;
        }
        Ok(SandboxInfo {
            name: name.to_owned(),
            state: SandboxState::Running,
        })
    }

    fn branch_live(
        &self,
        source: &str,
        children: &[SandboxSpec],
    ) -> Vec<Result<SandboxInfo, ProviderError>> {
        if let Err(error) = self.refuse(Operation::LiveBranch) {
            let why = error.to_string();
            return children
                .iter()
                .map(|_| match &error {
                    ProviderError::Unsupported(u) => Err(ProviderError::Unsupported(u.clone())),
                    _ => Err(ProviderError::Runtime(why.clone())),
                })
                .collect();
        }
        let seed = {
            let mut state = self.lock();
            state.ops.push(FakeOp::BranchLive {
                source: source.to_owned(),
                children: children.iter().map(|c| c.name.clone()).collect(),
            });
            match state.sandboxes.get(source) {
                Some(s) if matches!(s.state, SandboxState::Running | SandboxState::Paused) => {
                    Ok(s.rootfs.clone())
                }
                Some(s) => Err(format!("source {source} is {:?}", s.state)),
                None => Err(format!("no sandbox named {source}")),
            }
        };
        children
            .iter()
            .map(|child| match &seed {
                Ok(seed) => self.create(child, Some(seed)),
                Err(why) => Err(ProviderError::Invalid(why.clone())),
            })
            .collect()
    }

    fn release_checkpoint(&self, checkpoint: &Checkpoint) -> Result<(), ProviderError> {
        self.refuse(Operation::Release)?;
        let mut state = self.lock();
        state.ops.push(FakeOp::Release {
            reference: checkpoint.reference.clone(),
        });
        if let Some(dir) = state.checkpoints.remove(&checkpoint.reference) {
            branchyard_support::cleanup_dir(dir);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Mount;
    use crate::ExitStatus;
    use std::io::Read;

    /// Runs nothing: every exec is refused, so these tests stay in-process.
    struct Nothing;

    impl SandboxProvider for Nothing {
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
            Ok(SandboxInfo {
                name: spec.name.clone(),
                state: SandboxState::Running,
            })
        }
        fn inspect(&self, _: &str) -> Result<Option<SandboxInfo>, ProviderError> {
            Ok(None)
        }
        fn exec(&self, _: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
            Err(ProviderError::Runtime(format!(
                "ran {:?} in {} with {:?}",
                spec.argv,
                spec.cwd.display(),
                spec.env
            )))
        }
        fn stop(&self, _: &str) -> Result<(), ProviderError> {
            Ok(())
        }
        fn destroy(&self, _: &str) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    fn dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "by-fake-provider-{}-{}",
            std::process::id(),
            branchyard_support::time::now_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn exec_error(fake: &FakeProvider, name: &str, spec: &ExecSpec) -> String {
        match fake.exec(name, spec) {
            Err(error) => error.to_string(),
            Ok(mut process) => {
                let mut out = String::new();
                let _ = process.take_stdout().unwrap().read_to_string(&mut out);
                let _: ExitStatus = process.wait().unwrap();
                out
            }
        }
    }

    #[test]
    fn live_branches_copy_the_root_filesystem_and_keep_a_paused_source_paused() {
        let root = dir();
        let fake = FakeProvider::live(Box::new(Nothing), &root);
        fake.ensure(&SandboxSpec::new("src").mount(Mount::writable("/host/a", "/workspace")))
            .unwrap();
        fs::write(fake.rootfs("src").unwrap().join("deps"), "installed").unwrap();
        fake.pause("src").unwrap();
        let said = exec_error(
            &fake,
            "src",
            &ExecSpec {
                argv: vec!["true".into()],
                cwd: "/workspace".into(),
                env: BTreeMap::new(),
            },
        );
        assert!(said.contains("Paused"), "{said}");
        let children = [
            SandboxSpec::new("c1").mount(Mount::writable("/host/c1", "/workspace")),
            SandboxSpec::new("c2").mount(Mount::writable("/host/c2", "/workspace")),
        ];
        let made = fake.branch_live("src", &children);
        assert!(made.iter().all(Result::is_ok), "{made:?}");
        assert_eq!(
            fake.sandboxes(),
            [
                ("c1".to_owned(), SandboxState::Running),
                ("c2".to_owned(), SandboxState::Running),
                ("src".to_owned(), SandboxState::Paused),
            ]
        );
        for child in ["c1", "c2"] {
            let deps = fake.rootfs(child).unwrap().join("deps");
            assert_eq!(fs::read_to_string(&deps).unwrap(), "installed");
        }
        // Writes stay private to each child.
        fs::write(fake.rootfs("c1").unwrap().join("deps"), "changed").unwrap();
        assert_eq!(
            fs::read_to_string(fake.rootfs("c2").unwrap().join("deps")).unwrap(),
            "installed"
        );
        // Each child has its own mounts, and execs are mapped through them.
        let said = exec_error(
            &fake,
            "c2",
            &ExecSpec {
                argv: vec!["x".into()],
                cwd: "/workspace/src".into(),
                env: BTreeMap::from([("HOME".into(), "/workspace/home".into())]),
            },
        );
        assert!(said.contains("/host/c2/src"), "{said}");
        assert!(said.contains("\"/host/c2/home\""), "{said}");
        assert!(said.contains(ROOTFS_ENV), "{said}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn undeclared_operations_are_unsupported_and_checkpoints_release() {
        let root = dir();
        let plain = FakeProvider::plain(Box::new(Nothing), &root);
        plain.ensure(&SandboxSpec::new("s")).unwrap();
        assert!(matches!(
            plain.pause("s"),
            Err(ProviderError::Unsupported(_))
        ));
        assert!(matches!(
            plain.branch_live("s", &[SandboxSpec::new("c")])[0],
            Err(ProviderError::Unsupported(_))
        ));
        assert!(matches!(
            plain.checkpoint("s", &FULL_SAME_HOST),
            Err(ProviderError::Unsupported(_))
        ));

        let live = FakeProvider::live(Box::new(Nothing), &root);
        live.ensure(&SandboxSpec::new("s")).unwrap();
        fs::write(live.rootfs("s").unwrap().join("f"), "1").unwrap();
        let checkpoint = live.checkpoint("s", &FULL_SAME_HOST).unwrap();
        live.branch(&checkpoint, &SandboxSpec::new("b")).unwrap();
        assert_eq!(
            fs::read_to_string(live.rootfs("b").unwrap().join("f")).unwrap(),
            "1"
        );
        live.release_checkpoint(&checkpoint).unwrap();
        assert!(live.checkpoints().is_empty());
        live.set_failing(Operation::LiveBranch, true);
        assert!(matches!(
            live.branch_live("s", &[SandboxSpec::new("c")])[0],
            Err(ProviderError::Runtime(_))
        ));
        live.vanish("b");
        assert_eq!(live.inspect("b").unwrap(), None);
        let _ = fs::remove_dir_all(root);
    }
}
