//! The lifecycle half of the provider contract: `SandboxProvider` from
//! `docs/design.md` §10.
//!
//! A provider keeps named sandboxes. [`SandboxProvider::ensure`] creates one
//! from a [`SandboxSpec`] (an image, resource limits and host directories
//! mounted at sandbox paths), [`SandboxProvider::exec`] starts an argument
//! vector inside it and returns a [`Process`] with independent stdin, stdout
//! and stderr pipes, and [`SandboxProvider::stop`] and
//! [`SandboxProvider::destroy`] end it. Checkpoint, restore, branch and share
//! are optional; a provider that does not offer one returns
//! [`ProviderError::Unsupported`] and does not declare it in its
//! [`Capabilities`].
//!
//! The contract is synchronous and uses `std::io` traits. A provider whose
//! runtime is asynchronous bridges it privately.
//!
//! What every provider guarantees:
//!
//! - `ensure` never weakens a spec. A mount, image or limit it cannot honor is
//!   an error ([`ProviderError::Invalid`] or [`ProviderError::Unsupported`]),
//!   not ignored.
//! - `exec` runs the argument vector without a shell, in `cwd` (a sandbox
//!   path), with `env` added to the provider's base environment and nothing
//!   else from the caller's. A program that cannot be started is an
//!   [`ProviderError::Io`] from `exec`, not a process that exits at once.
//! - [`Process::teardown`] reaches every process the exec started that is
//!   still in the process group (or equivalent) the provider created for it,
//!   including descendants of a launched process that already exited, and
//!   names what it found. Dropping a [`Process`] that was never waited for
//!   kills and tears it down.
//! - `stop` and `destroy` end every process in the sandbox.
//!
//! What it does not guarantee:
//!
//! - Isolation. That is a property of each provider and is documented on it;
//!   the local provider offers none beyond the operating-system user.
//! - Bounded output. Pipes apply the provider's own buffering.
//! - Path safety. [`SandboxSpec::guest_path`] and [`SandboxSpec::host_path`]
//!   map paths lexically; they do not resolve symlinks. Callers acting on a
//!   harness-supplied path must still validate it inside the sandbox.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::{Capabilities, Operation, SandboxState, SnapshotGuarantee, Unsupported};

/// A host directory made visible inside a sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// Absolute host path.
    pub host: PathBuf,
    /// Absolute sandbox path where the directory appears.
    pub guest: PathBuf,
    /// Whether the sandbox may write through the mount. A provider that
    /// cannot enforce read-only access rejects a read-only mount.
    pub writable: bool,
}

impl Mount {
    /// A writable mount of `host` at `guest`.
    pub fn writable(host: impl Into<PathBuf>, guest: impl Into<PathBuf>) -> Mount {
        Mount {
            host: host.into(),
            guest: guest.into(),
            writable: true,
        }
    }

    /// A read-only mount of `host` at `guest`.
    pub fn read_only(host: impl Into<PathBuf>, guest: impl Into<PathBuf>) -> Mount {
        Mount {
            writable: false,
            ..Mount::writable(host, guest)
        }
    }
}

/// Resource limits. Unset limits take the provider's default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resources {
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
}

impl Resources {
    pub fn is_unlimited(&self) -> bool {
        self.cpus.is_none() && self.memory_mib.is_none()
    }
}

/// What a sandbox is created from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxSpec {
    /// Provider-unique name. [`SandboxProvider::ensure`] is idempotent on it.
    pub name: String,
    /// OCI image reference, for providers that boot one.
    pub image: Option<String>,
    pub resources: Resources,
    pub mounts: Vec<Mount>,
}

impl SandboxSpec {
    pub fn new(name: impl Into<String>) -> SandboxSpec {
        SandboxSpec {
            name: name.into(),
            ..SandboxSpec::default()
        }
    }

    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.image = Some(image.into());
        self
    }

    pub fn resources(mut self, resources: Resources) -> Self {
        self.resources = resources;
        self
    }

    pub fn mount(mut self, mount: Mount) -> Self {
        self.mounts.push(mount);
        self
    }

    /// Check what every provider requires: a non-empty name, absolute
    /// normalized host and sandbox paths, and no two mounts at one sandbox
    /// path. Providers add their own checks.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.name.is_empty() {
            return Err(ProviderError::Invalid("the sandbox name is empty".into()));
        }
        for mount in &self.mounts {
            for (what, path) in [("host", &mount.host), ("sandbox", &mount.guest)] {
                if !normalized_absolute(path) {
                    return Err(ProviderError::Invalid(format!(
                        "mount {what} path {} is not absolute and normalized",
                        path.display()
                    )));
                }
            }
        }
        for (index, mount) in self.mounts.iter().enumerate() {
            if self.mounts[..index].iter().any(|m| m.guest == mount.guest) {
                return Err(ProviderError::Invalid(format!(
                    "two mounts at {}",
                    mount.guest.display()
                )));
            }
        }
        Ok(())
    }

    /// The sandbox path of a host path inside a mount, by the longest
    /// matching mount. Lexical: `..` and symlinks are not resolved.
    pub fn guest_path(&self, host: &Path) -> Option<PathBuf> {
        map_path(
            self.mounts
                .iter()
                .map(|m| (m.host.as_path(), m.guest.as_path())),
            host,
        )
    }

    /// The host path of a sandbox path inside a mount, by the longest
    /// matching mount. Lexical: `..` and symlinks are not resolved.
    pub fn host_path(&self, guest: &Path) -> Option<PathBuf> {
        map_path(
            self.mounts
                .iter()
                .map(|m| (m.guest.as_path(), m.host.as_path())),
            guest,
        )
    }
}

fn normalized_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

fn map_path<'a>(pairs: impl Iterator<Item = (&'a Path, &'a Path)>, path: &Path) -> Option<PathBuf> {
    if !normalized_absolute(path) {
        return None;
    }
    pairs
        .filter_map(|(from, to)| {
            let rest = path.strip_prefix(from).ok()?;
            Some((from.components().count(), to.join(rest)))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, mapped)| mapped)
}

/// A process to start inside a sandbox.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecSpec {
    /// Program and arguments, executed without a shell.
    pub argv: Vec<String>,
    /// Working directory, a sandbox path.
    pub cwd: PathBuf,
    /// Variables set for the process on top of the provider's base
    /// environment. The local provider's base is empty; an image-based
    /// provider's is the image's configured environment.
    pub env: BTreeMap<OsString, OsString>,
}

/// How a process ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExitStatus {
    /// Exit code, when the process exited and the provider reports one.
    pub code: Option<i32>,
    /// Terminating signal, when the provider reports one.
    pub signal: Option<i32>,
}

impl ExitStatus {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.code, self.signal) {
            (Some(code), _) => write!(f, "exit code {code}"),
            (None, Some(signal)) => write!(f, "signal {signal}"),
            (None, None) => f.write_str("unknown exit status"),
        }
    }
}

/// A process started by [`SandboxProvider::exec`].
///
/// Each pipe can be taken once. A provider keeps its own reader or writer
/// state; closing stdin (dropping its writer) delivers end of file.
pub trait Process: Send {
    /// A provider-specific identifier for diagnostics, such as a PID.
    fn id(&self) -> String;
    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>>;
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>>;
    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>>;
    /// The exit status if the launched process has exited.
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    /// Block until the launched process exits.
    fn wait(&mut self) -> io::Result<ExitStatus>;
    /// Send the launched process SIGKILL or the provider's equivalent. A
    /// provider may kill its whole group too, and says so.
    fn kill(&mut self) -> io::Result<()>;
    /// Name the processes of this exec still running, then kill them all,
    /// descendants that outlived the launched process included. Returns
    /// their command names; empty when none were found or the provider
    /// cannot list them.
    fn teardown(&mut self) -> Vec<String>;
}

/// A sandbox as the provider reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxInfo {
    pub name: String,
    pub state: SandboxState,
}

/// A durable reference to recorded sandbox state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// The sandbox it was taken from.
    pub sandbox: String,
    /// Provider-specific reference.
    pub reference: String,
    /// What the checkpoint actually captured.
    pub guarantee: SnapshotGuarantee,
}

/// Why a provider operation failed.
#[derive(Debug)]
pub enum ProviderError {
    /// The provider does not offer this operation or guarantee.
    Unsupported(Unsupported),
    /// The spec cannot be honored by this provider as written.
    Invalid(String),
    /// No sandbox has this name.
    NotFound(String),
    /// An I/O failure, including a program that could not be started.
    Io(io::Error),
    /// The provider's runtime reported a failure.
    Runtime(String),
}

impl ProviderError {
    /// An operation this provider does not offer.
    pub fn unsupported(operation: Operation) -> ProviderError {
        ProviderError::Unsupported(Unsupported {
            operation,
            required: None,
            offered: Vec::new(),
        })
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::Unsupported(unsupported) => write!(f, "{unsupported}"),
            ProviderError::Invalid(reason) => write!(f, "invalid sandbox spec: {reason}"),
            ProviderError::NotFound(name) => write!(f, "no sandbox named {name}"),
            ProviderError::Io(error) => write!(f, "{error}"),
            ProviderError::Runtime(reason) => write!(f, "sandbox runtime: {reason}"),
        }
    }
}

impl std::error::Error for ProviderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProviderError::Unsupported(unsupported) => Some(unsupported),
            ProviderError::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ProviderError {
    fn from(error: io::Error) -> Self {
        ProviderError::Io(error)
    }
}

impl From<ProviderError> for io::Error {
    /// An I/O error keeps its kind; anything else becomes
    /// [`io::ErrorKind::Other`] with the provider's message.
    fn from(error: ProviderError) -> Self {
        match error {
            ProviderError::Io(error) => error,
            ProviderError::NotFound(_) => io::Error::new(io::ErrorKind::NotFound, error),
            ProviderError::Unsupported(_) => io::Error::new(io::ErrorKind::Unsupported, error),
            ProviderError::Invalid(_) => io::Error::new(io::ErrorKind::InvalidInput, error),
            other => io::Error::other(other),
        }
    }
}

/// A sandbox provider's lifecycle, independent of any vendor SDK.
pub trait SandboxProvider: Send + Sync {
    /// What this provider claims to support. Unqualified until the runtime
    /// gates in `docs/implementation-plan.md` pass.
    fn capabilities(&self) -> Capabilities;

    /// Create the sandbox `spec.name` if this provider holds none by that
    /// name, and start it. Calling it again with the same name returns the
    /// existing sandbox; it does not reconcile a different spec.
    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError>;

    /// The sandbox's state, or `None` if it does not exist.
    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError>;

    /// Start `spec.argv` inside the sandbox with piped stdio.
    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError>;

    /// End every process in the sandbox. Its state is kept where the
    /// provider keeps any.
    fn stop(&self, name: &str) -> Result<(), ProviderError>;

    /// Stop the sandbox and release everything the provider holds for it.
    /// Mounted host directories are not deleted. Destroying a sandbox that
    /// does not exist is not an error.
    fn destroy(&self, name: &str) -> Result<(), ProviderError>;

    /// Record the sandbox's state with at least the `required` guarantee.
    fn checkpoint(
        &self,
        name: &str,
        required: &SnapshotGuarantee,
    ) -> Result<Checkpoint, ProviderError> {
        let _ = (name, required);
        Err(ProviderError::unsupported(Operation::Checkpoint))
    }

    /// Return the sandbox to one of its own checkpoints.
    fn restore(&self, name: &str, checkpoint: &Checkpoint) -> Result<(), ProviderError> {
        let _ = (name, checkpoint);
        Err(ProviderError::unsupported(Operation::Restore))
    }

    /// Create a new sandbox, with a new identity, from a checkpoint.
    fn branch(
        &self,
        checkpoint: &Checkpoint,
        spec: &SandboxSpec,
    ) -> Result<SandboxInfo, ProviderError> {
        let _ = (checkpoint, spec);
        Err(ProviderError::unsupported(Operation::Branch))
    }

    /// Attach a host directory that other sandboxes may also mount to a
    /// running sandbox.
    fn share(&self, name: &str, mount: &Mount) -> Result<(), ProviderError> {
        let _ = (name, mount);
        Err(ProviderError::unsupported(Operation::Share))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SandboxSpec {
        SandboxSpec::new("s")
            .mount(Mount::writable("/repo/.by/wt/a", "/workspace"))
            .mount(Mount::read_only("/repo/.git", "/repo/.git"))
            .mount(Mount::writable("/repo/.by/wt/a/target", "/cache"))
    }

    #[test]
    fn paths_map_through_the_longest_mount() {
        let spec = spec();
        assert_eq!(
            spec.guest_path(Path::new("/repo/.by/wt/a/src/lib.rs")),
            Some(PathBuf::from("/workspace/src/lib.rs"))
        );
        assert_eq!(
            spec.guest_path(Path::new("/repo/.by/wt/a/target/debug")),
            Some(PathBuf::from("/cache/debug"))
        );
        assert_eq!(
            spec.guest_path(Path::new("/repo/.by/wt/a")),
            Some(PathBuf::from("/workspace"))
        );
        assert_eq!(
            spec.host_path(Path::new("/workspace/src")),
            Some(PathBuf::from("/repo/.by/wt/a/src"))
        );
        assert_eq!(
            spec.host_path(Path::new("/repo/.git/HEAD")),
            Some(PathBuf::from("/repo/.git/HEAD"))
        );
    }

    #[test]
    fn unmapped_and_unnormalized_paths_do_not_map() {
        let spec = spec();
        assert_eq!(spec.guest_path(Path::new("/repo/.by/wt/ab")), None);
        assert_eq!(spec.host_path(Path::new("/etc/passwd")), None);
        assert_eq!(spec.host_path(Path::new("/workspace/../etc")), None);
        assert_eq!(spec.host_path(Path::new("workspace/src")), None);
    }

    #[test]
    fn specs_are_validated() {
        assert!(spec().validate().is_ok());
        assert!(SandboxSpec::new("").validate().is_err());
        let relative = SandboxSpec::new("s").mount(Mount::writable("repo", "/w"));
        assert!(relative.validate().is_err());
        let dotted = SandboxSpec::new("s").mount(Mount::writable("/repo", "/w/../x"));
        assert!(dotted.validate().is_err());
        let twice = SandboxSpec::new("s")
            .mount(Mount::writable("/a", "/w"))
            .mount(Mount::writable("/b", "/w"));
        assert!(
            matches!(twice.validate(), Err(ProviderError::Invalid(why)) if why.contains("two mounts"))
        );
    }

    #[test]
    fn errors_convert_to_io_errors_by_kind() {
        let missing = io::Error::new(io::ErrorKind::NotFound, "no such program");
        assert_eq!(
            io::Error::from(ProviderError::Io(missing)).kind(),
            io::ErrorKind::NotFound
        );
        let unsupported = ProviderError::unsupported(Operation::Branch);
        assert_eq!(unsupported.to_string(), "provider does not support branch");
        assert_eq!(
            io::Error::from(unsupported).kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            io::Error::from(ProviderError::Runtime("x".into())).kind(),
            io::ErrorKind::Other
        );
    }

    #[test]
    fn exit_statuses_describe_themselves() {
        let ok = ExitStatus {
            code: Some(0),
            signal: None,
        };
        assert!(ok.success());
        let killed = ExitStatus {
            code: None,
            signal: Some(9),
        };
        assert!(!killed.success());
        assert_eq!(killed.to_string(), "signal 9");
        assert_eq!(ExitStatus::default().to_string(), "unknown exit status");
    }
}
