//! Where one turn's harness runs: a local process, or a sandbox from a
//! [`Provider`]. Everything provider-specific in the engine is here.
//!
//! In a sandbox, the branch's worktree is mounted read-write at
//! [`WORKSPACE`] and the harness runs there; its private home is mounted at
//! [`HOME`]; the repository's git directory is mounted read-only at its
//! host path, so git inside the sandbox can read the worktree's history but
//! not move refs. The harness gets `HOME` and the variables named in
//! [`SandboxOptions::pass_env`], and nothing else from this process. Each
//! turn gets a fresh sandbox, destroyed when the turn ends.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use branchyard_harness::{Driver, Open};
use branchyard_runtime::{RuntimeError, Session};
use branchyard_sandbox::{Mount, Resources, SandboxProvider, SandboxSpec};

use crate::state::{now_ms, Record};
#[cfg(test)]
use crate::SandboxOptions;
use crate::{git, harness, Error, Provider, Yard};

/// Where the worktree appears in a sandbox.
pub const WORKSPACE: &str = "/workspace";
/// Where the branch's private home appears in a sandbox.
pub const HOME: &str = "/branchyard/home";

/// Whether this provider runs the harness somewhere other than this host.
pub(crate) fn sandboxed(provider: Option<&Provider>) -> bool {
    matches!(provider, Some(Provider::Microsandbox(_)))
}

/// Refuse a provider this build or its options cannot run, before anything
/// is created.
pub(crate) fn check(provider: Option<&Provider>) -> Result<(), Error> {
    match provider {
        None | Some(Provider::Local) => Ok(()),
        Some(Provider::Microsandbox(options)) => {
            if !branchyard_microsandbox::ENABLED {
                return Err(Error::Unsupported(
                    "this build has no Microsandbox support; rebuild with \
                     --features microsandbox (Rust 1.94 or newer), see docs/providers.md"
                        .into(),
                ));
            }
            if options.image.trim().is_empty() {
                return Err(Error::Unsupported(
                    "the microsandbox provider needs an image".into(),
                ));
            }
            Ok(())
        }
    }
}

/// A turn's harness location. A sandbox is destroyed by
/// [`Placement::release`], or on drop.
pub(crate) struct Placement {
    cwd: String,
    kind: Kind,
}

enum Kind {
    Local(branchyard_runtime::Environment),
    Sandbox {
        provider: Box<dyn SandboxProvider>,
        name: String,
        env: BTreeMap<OsString, OsString>,
        released: bool,
    },
}

impl Placement {
    /// Prepare the record's provider: for a sandbox, create it now. A
    /// failure is the turn's failure reason.
    pub fn prepare(yard: &Yard, record: &Record) -> Result<Placement, String> {
        let options = match &record.provider {
            None | Some(Provider::Local) => {
                return Ok(Placement {
                    cwd: record.info.worktree.display().to_string(),
                    kind: Kind::Local(harness::environment(record.home.as_deref())),
                })
            }
            Some(Provider::Microsandbox(options)) => options,
        };
        let home = record
            .home
            .clone()
            .ok_or("a sandboxed branch has no private home")?;
        let mut env = BTreeMap::from([(OsString::from("HOME"), OsString::from(HOME))]);
        for name in &options.pass_env {
            let value = std::env::var_os(name)
                .ok_or_else(|| format!("{name} is to be passed to the sandbox but is not set"))?;
            env.insert(name.into(), value);
        }
        let git_dir = git::common_dir(&yard.root).map_err(|e| e.to_string())?;
        let spec = SandboxSpec {
            name: sandbox_name(&record.info.name, now_ms()),
            image: Some(options.image.clone()),
            resources: Resources {
                cpus: options.cpus,
                memory_mib: options.memory_mib,
            },
            mounts: vec![
                Mount::writable(&record.info.worktree, WORKSPACE),
                Mount::writable(home, HOME),
                Mount::read_only(&git_dir, &git_dir),
            ],
        };
        let provider = microsandbox()?;
        provider
            .ensure(&spec)
            .map_err(|e| format!("could not create sandbox {}: {e}", spec.name))?;
        Ok(Placement {
            cwd: WORKSPACE.into(),
            kind: Kind::Sandbox {
                provider,
                name: spec.name,
                env,
                released: false,
            },
        })
    }

    /// The harness's working directory, as the harness sees it.
    pub fn cwd(&self) -> String {
        self.cwd.clone()
    }

    pub fn start(&self, driver: Box<dyn Driver>, open: Open) -> Result<Session, RuntimeError> {
        match &self.kind {
            Kind::Local(env) => Session::start(driver, open, env, None),
            Kind::Sandbox {
                provider,
                name,
                env,
                ..
            } => Session::start_in(driver, open, provider.as_ref(), name, env.clone(), None),
        }
    }

    /// Destroy the sandbox, if any. A failure is returned as a warning.
    pub fn release(&mut self) -> Option<String> {
        let Kind::Sandbox {
            provider,
            name,
            released,
            ..
        } = &mut self.kind
        else {
            return None;
        };
        if std::mem::replace(released, true) {
            return None;
        }
        provider
            .destroy(name)
            .err()
            .map(|e| format!("could not destroy sandbox {name}: {e}"))
    }
}

impl Drop for Placement {
    fn drop(&mut self) {
        self.release();
    }
}

/// `by-<branch>-<ms>`, within the runtime's 128-byte limit. Branch names
/// are already `[a-z0-9._-]`.
fn sandbox_name(branch: &str, ms: u64) -> String {
    let suffix = format!("-{ms}");
    let room = 128 - "by-".len() - suffix.len();
    let branch: String = branch.chars().take(room).collect();
    format!("by-{branch}{suffix}")
}

#[cfg(feature = "microsandbox")]
fn microsandbox() -> Result<Box<dyn SandboxProvider>, String> {
    branchyard_microsandbox::MicrosandboxProvider::new()
        .map(|p| Box::new(p) as Box<dyn SandboxProvider>)
        .map_err(|e| format!("could not start the Microsandbox SDK: {e}"))
}

#[cfg(not(feature = "microsandbox"))]
fn microsandbox() -> Result<Box<dyn SandboxProvider>, String> {
    Err("this build has no Microsandbox support".into())
}

/// The private home a new branch needs because it runs in a sandbox.
pub(crate) fn private_home(
    provider: Option<&Provider>,
    store: &crate::state::Store,
    name: &str,
) -> Option<PathBuf> {
    sandboxed(provider).then(|| store.home(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_names_fit_the_runtime() {
        assert_eq!(sandbox_name("fix-parser", 17), "by-fix-parser-17");
        let long = sandbox_name(&"a".repeat(200), u64::MAX);
        assert_eq!(long.len(), 128);
        assert!(branchyard_microsandbox::plan::name(&long).is_ok());
    }

    #[test]
    fn providers_are_checked_before_anything_is_created() {
        assert!(check(None).is_ok());
        assert!(check(Some(&Provider::Local)).is_ok());
        let empty = Provider::Microsandbox(SandboxOptions::default());
        assert!(matches!(check(Some(&empty)), Err(Error::Unsupported(_))));
        let image = Provider::Microsandbox(SandboxOptions {
            image: "alpine:3.20".into(),
            ..SandboxOptions::default()
        });
        assert_eq!(
            check(Some(&image)).is_ok(),
            branchyard_microsandbox::ENABLED
        );
        assert!(sandboxed(Some(&image)));
        assert!(!sandboxed(Some(&Provider::Local)));
    }
}
