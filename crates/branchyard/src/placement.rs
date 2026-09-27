//! Where one turn's harness runs: a local process, or a sandbox from a
//! [`Provider`]. Everything provider-specific in the engine is here.
//!
//! In a Microsandbox sandbox, the branch's worktree is mounted read-write at
//! [`WORKSPACE`] and the harness runs there; its private home is mounted at
//! [`HOME`]; the repository's git directory is mounted read-only at its
//! host path, so git inside the sandbox can read the worktree's history but
//! not move refs.
//!
//! A Substrate actor cannot mount anything. The worktree is copied into a
//! new repository at [`SubstrateOptions::workdir`] and the private home to
//! [`SubstrateOptions::home`] before the harness starts; when the turn ends
//! the actor's working files are applied to the worktree's files and its
//! home replaces the private home, then the actor is deleted. The actor is
//! journaled as the turn's `sandbox` step before it is created, so recovery
//! can delete it if this engine stops. See [`branchyard_substrate::transfer`].
//!
//! Either way the harness gets `HOME` and the variables named in the
//! provider's `pass_env`, and nothing else from this process. Each turn gets
//! a fresh sandbox, destroyed when the turn ends.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use branchyard_harness::{Driver, Open};
use branchyard_runtime::{RuntimeError, Session};
use branchyard_sandbox::{Mount, Resources, SandboxProvider, SandboxSpec};
use branchyard_substrate::transfer::{self, Pushed};
use branchyard_substrate::SubstrateProvider;
use serde_json::{json, Value};

use crate::state::{now_ms, Fence, Record};
#[cfg(test)]
use crate::SandboxOptions;
use crate::{git, harness, Error, Provider, SubstrateOptions, Yard};

/// Where the worktree appears in a sandbox.
pub const WORKSPACE: &str = "/workspace";
/// Where the branch's private home appears in a sandbox.
pub const HOME: &str = "/branchyard/home";
/// The journaled step naming a turn's sandbox, before it is created.
pub(crate) const STEP_SANDBOX: &str = "sandbox";

/// Whether this provider runs the harness somewhere other than this host.
pub(crate) fn sandboxed(provider: Option<&Provider>) -> bool {
    matches!(
        provider,
        Some(Provider::Microsandbox(_) | Provider::Substrate(_))
    )
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
        Some(Provider::Substrate(options)) => check_substrate(options),
    }
}

fn check_substrate(options: &SubstrateOptions) -> Result<(), Error> {
    let refuse = |why: String| Err(Error::Unsupported(format!("the substrate provider {why}")));
    if !options.endpoint.starts_with("http://") {
        return refuse(format!(
            "needs an http:// Control API endpoint, not {:?}",
            options.endpoint
        ));
    }
    if !options.router.starts_with("http://") || !options.router.contains("{actor}") {
        return refuse(format!(
            "needs an http:// router URL naming {{actor}}, not {:?}",
            options.router
        ));
    }
    if options.template.trim().is_empty() {
        return refuse("needs an actor template".into());
    }
    for (what, path) in [("workdir", options.workdir()), ("home", options.home())] {
        if !Path::new(path).is_absolute() {
            return refuse(format!("needs an absolute {what}, not {path:?}"));
        }
    }
    branchyard_bridge::Signer::read(&options.key)
        .map(|_| ())
        .or_else(|e| refuse(format!("cannot use its bridge key: {e}")))
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
    Substrate(Box<Actor>),
}

/// A turn's Substrate actor, and what must come back from it.
struct Actor {
    provider: SubstrateProvider,
    name: String,
    env: BTreeMap<OsString, OsString>,
    worktree: PathBuf,
    pushed: Option<Pushed>,
    /// The private home on this host, and where it is in the actor.
    home: Option<(PathBuf, PathBuf)>,
    released: bool,
}

impl Actor {
    /// Bring the worktree and home back, then delete the actor. Returns
    /// what went wrong, if anything; the actor is deleted regardless.
    fn release(&mut self) -> Vec<String> {
        if std::mem::replace(&mut self.released, true) {
            return Vec::new();
        }
        let mut warnings = Vec::new();
        match self.provider.endpoint(&self.name) {
            Ok(endpoint) => {
                if let Some(pushed) = &self.pushed {
                    if let Err(error) = transfer::pull(&endpoint, pushed, &self.worktree) {
                        warnings.push(format!(
                            "could not bring the worktree back from actor {}: {error}",
                            self.name
                        ));
                    }
                }
                if let Some((host, guest)) = &self.home {
                    if let Err(error) = transfer::pull_tree(&endpoint, guest, host) {
                        warnings.push(format!(
                            "could not bring the home directory back from actor {}: {error}",
                            self.name
                        ));
                    }
                }
            }
            Err(error) if self.pushed.is_some() => warnings.push(format!(
                "could not bring anything back from actor {}: {error}",
                self.name
            )),
            Err(_) => {}
        }
        self.pushed = None;
        if let Err(error) = self.provider.destroy(&self.name) {
            warnings.push(format!("could not delete actor {}: {error}", self.name));
        }
        warnings
    }
}

impl Placement {
    /// Prepare the record's provider: for a sandbox, create it now. A
    /// failure is the turn's failure reason.
    pub fn prepare(yard: &Yard, record: &Record, fence: &Fence) -> Result<Placement, String> {
        let options = match &record.provider {
            None | Some(Provider::Local) => {
                return Ok(Placement {
                    cwd: record.info.worktree.display().to_string(),
                    kind: Kind::Local(harness::environment(record.home.as_deref())),
                })
            }
            Some(Provider::Substrate(options)) => {
                return Placement::substrate(yard, record, fence, options)
            }
            Some(Provider::Microsandbox(options)) => options,
        };
        let home = record
            .home
            .clone()
            .ok_or("a sandboxed branch has no private home")?;
        let env = sandbox_env(HOME, &options.pass_env)?;
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

    fn substrate(
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        options: &SubstrateOptions,
    ) -> Result<Placement, String> {
        let home = record
            .home
            .clone()
            .ok_or("a sandboxed branch has no private home")?;
        let env = sandbox_env(options.home(), &options.pass_env)?;
        let provider = substrate(options)?;
        let name = actor_name(&record.info.name, now_ms());
        // Journaled before the actor exists, so recovery can delete it.
        let store = yard.store();
        let intent = json!({
            "provider": "substrate",
            "actor": name,
            "atespace": options.atespace(),
        });
        store
            .backend()
            .begin_step(fence, fence.turn, STEP_SANDBOX, &intent)
            .map_err(|e| format!("could not record actor {name}: {e}"))?;
        let mut actor = Box::new(Actor {
            provider,
            name: name.clone(),
            env,
            worktree: record.info.worktree.clone(),
            pushed: None,
            home: None,
            released: false,
        });
        let created = actor.provider.ensure(&SandboxSpec::new(&name));
        let uid = actor
            .provider
            .handle(&name)
            .ok()
            .flatten()
            .map(|h| h.uid)
            .unwrap_or_default();
        let _ =
            store
                .backend()
                .finish_step(fence, fence.turn, STEP_SANDBOX, &json!({ "uid": uid }));
        let fail = |actor: &mut Actor, why: String| -> Result<Placement, String> {
            let warnings = actor.release();
            match warnings.is_empty() {
                true => Err(why),
                false => Err(format!("{why}; {}", warnings.join("; "))),
            }
        };
        if let Err(error) = created {
            return fail(
                &mut actor,
                format!("could not create actor {name}: {error}"),
            );
        }
        let endpoint = match actor.provider.endpoint(&name) {
            Ok(endpoint) => endpoint,
            Err(error) => return fail(&mut actor, error.to_string()),
        };
        let workdir = PathBuf::from(options.workdir());
        match transfer::push(&endpoint, &record.info.worktree, &workdir) {
            Ok(pushed) => actor.pushed = Some(pushed),
            Err(error) => {
                return fail(
                    &mut actor,
                    format!("could not copy the worktree into actor {name}: {error}"),
                )
            }
        }
        let guest_home = PathBuf::from(options.home());
        if let Err(error) = transfer::push_tree(&endpoint, &home, &guest_home) {
            return fail(
                &mut actor,
                format!("could not copy the home directory into actor {name}: {error}"),
            );
        }
        actor.home = Some((home, guest_home));
        Ok(Placement {
            cwd: options.workdir().to_owned(),
            kind: Kind::Substrate(actor),
        })
    }

    /// Whether the harness runs in a sandbox rather than on this host.
    pub fn is_sandbox(&self) -> bool {
        !matches!(self.kind, Kind::Local(_))
    }

    /// Add a variable to the harness's environment.
    pub fn set_env(&mut self, name: &str, value: &str) {
        match &mut self.kind {
            Kind::Local(env) => *env = env.clone().set(name, value),
            Kind::Sandbox { env, .. } => {
                env.insert(name.into(), value.into());
            }
            Kind::Substrate(actor) => {
                actor.env.insert(name.into(), value.into());
            }
        }
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
            Kind::Substrate(actor) => Session::start_in(
                driver,
                open,
                &actor.provider,
                &actor.name,
                actor.env.clone(),
                None,
            ),
        }
    }

    /// Destroy the sandbox, if any; for a Substrate actor, bring the
    /// worktree and home back first. A failure is returned as a warning.
    pub fn release(&mut self) -> Option<String> {
        match &mut self.kind {
            Kind::Local(_) => None,
            Kind::Sandbox {
                provider,
                name,
                released,
                ..
            } => {
                if std::mem::replace(released, true) {
                    return None;
                }
                provider
                    .destroy(name)
                    .err()
                    .map(|e| format!("could not destroy sandbox {name}: {e}"))
            }
            Kind::Substrate(actor) => {
                let warnings = actor.release();
                (!warnings.is_empty()).then(|| warnings.join("; "))
            }
        }
    }
}

impl Drop for Placement {
    fn drop(&mut self) {
        self.release();
    }
}

/// `HOME` and the passed variables, each of which must be set here.
fn sandbox_env(home: &str, pass_env: &[String]) -> Result<BTreeMap<OsString, OsString>, String> {
    let mut env = BTreeMap::from([(OsString::from("HOME"), OsString::from(home))]);
    for name in pass_env {
        let value = std::env::var_os(name)
            .ok_or_else(|| format!("{name} is to be passed to the sandbox but is not set"))?;
        env.insert(name.into(), value);
    }
    Ok(env)
}

/// `by-<branch>-<ms>`, within the runtime's 128-byte limit. Branch names
/// are already `[a-z0-9._-]`.
fn sandbox_name(branch: &str, ms: u64) -> String {
    let suffix = format!("-{ms}");
    let room = 128 - "by-".len() - suffix.len();
    let branch: String = branch.chars().take(room).collect();
    format!("by-{branch}{suffix}")
}

/// `by-<branch>-<ms>` as a Substrate resource name: a DNS label of at most
/// 63 characters, so `.` and `_` become `-` and the branch is shortened.
fn actor_name(branch: &str, ms: u64) -> String {
    let suffix = format!("-{ms}");
    let room = 63 - "by-".len() - suffix.len();
    let branch: String = branch
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' => c,
            _ => '-',
        })
        .take(room)
        .collect();
    format!("by-{}{suffix}", branch.trim_end_matches('-'))
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

/// A provider for `options`; `signed` includes the bridge key.
fn substrate_provider(
    options: &SubstrateOptions,
    signed: bool,
) -> Result<SubstrateProvider, String> {
    let mut config = branchyard_substrate::Config::new(
        &options.endpoint,
        options.atespace(),
        &options.template,
        &options.router,
    );
    if signed {
        let signer = branchyard_bridge::Signer::read(&options.key).map_err(|e| e.to_string())?;
        config = config.signer(signer);
    }
    config.ready_timeout = Duration::from_secs(300);
    SubstrateProvider::connect(config).map_err(|e| format!("could not reach Substrate: {e}"))
}

fn substrate(options: &SubstrateOptions) -> Result<SubstrateProvider, String> {
    substrate_provider(options, true)
}

/// Delete the actor a stopped engine's turn journaled, if it still exists.
/// Returns what recovery should report, if anything.
pub(crate) fn recover(record: &Record, intent: &Value) -> Option<String> {
    let Some(Provider::Substrate(options)) = &record.provider else {
        return None;
    };
    let actor = intent.get("actor")?.as_str()?;
    // Deleting needs only the Control API, not the bridge key.
    let deleted = substrate_provider(options, false).and_then(|provider| {
        let existed = provider.handle(actor).map_err(|e| e.to_string())?.is_some();
        provider.destroy(actor).map_err(|e| e.to_string())?;
        Ok(existed)
    });
    Some(match deleted {
        Ok(true) => format!("deleted its Substrate actor {actor}"),
        Ok(false) => format!("its Substrate actor {actor} was already gone"),
        Err(error) => format!("could not delete its Substrate actor {actor}: {error}"),
    })
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
    fn actor_names_are_dns_labels() {
        assert_eq!(actor_name("fix.parser_2", 17), "by-fix-parser-2-17");
        let long = actor_name(&"a".repeat(200), 1_790_000_000_000);
        assert_eq!(long.len(), 63);
        assert!(long.ends_with("-1790000000000"));
        let dashed = actor_name(&format!("{}.x", "a".repeat(45)), 1_790_000_000_000);
        assert!(!dashed.contains("--"), "{dashed}");
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

    #[test]
    fn substrate_options_are_checked_before_anything_is_created() {
        let dir = std::env::temp_dir().join(format!("by-placement-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key");
        let _ = std::fs::remove_file(&key);
        branchyard_bridge::Signer::write(&key).unwrap();
        let good = SubstrateOptions {
            endpoint: "http://127.0.0.1:9".into(),
            router: "http://127.0.0.1:9/{atespace}/{actor}/".into(),
            template: "by".into(),
            key: key.clone(),
            ..SubstrateOptions::default()
        };
        assert!(check(Some(&Provider::Substrate(good.clone()))).is_ok());
        assert!(sandboxed(Some(&Provider::Substrate(good.clone()))));
        let bad = [
            SubstrateOptions {
                endpoint: "https://secure".into(),
                ..good.clone()
            },
            SubstrateOptions {
                router: "http://router/".into(),
                ..good.clone()
            },
            SubstrateOptions {
                template: " ".into(),
                ..good.clone()
            },
            SubstrateOptions {
                workdir: "relative".into(),
                ..good.clone()
            },
            SubstrateOptions {
                key: dir.join("missing"),
                ..good.clone()
            },
        ];
        for options in bad {
            assert!(
                matches!(
                    check(Some(&Provider::Substrate(options.clone()))),
                    Err(Error::Unsupported(_))
                ),
                "{options:?}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
