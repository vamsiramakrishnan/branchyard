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
//! home replaces the private home, then the actor is deleted. See
//! [`branchyard_substrate::transfer`].
//!
//! An environment recipe's machine ([`Provider::Recipe`]) is reached over
//! ssh or the recipe's exec command, and cannot mount anything either: the
//! same transfer copies the worktree and home in and back, its files
//! crossing as `cat` and `tar` streams over execs
//! ([`transfer::Exec`]), and the machine is destroyed (or suspended)
//! through the recipe's scripts. Its record is kept in the store's
//! `recipes` directory, so recovery and removal reach it from another
//! process.
//!
//! Either sandbox is journaled as the turn's `sandbox` step before it is
//! created. If this engine stops, recovery destroys a Microsandbox sandbox,
//! and brings a Substrate actor's work back as the turn's end would have
//! before deleting it, unless the turn had already parked it (below).
//!
//! Either way the harness gets `HOME` and the variables named in the
//! provider's `pass_env`, and nothing else from this process. By default
//! each turn gets a fresh sandbox, destroyed when the turn ends. With
//! `keep = "pause"` and a provider that can pause, the sandbox is paused
//! and recorded when the turn ends, and the next turn resumes it; a branch
//! seeded from another's checkpoint gets a sandbox branched from that
//! checkpoint's provider snapshot. See [`crate::snapshots`] and
//! `docs/sandbox-snapshots.md`. The turn's sandbox runs the branch's
//! `[workspace]` setup too ([`Placement::sandbox`]), so what it installs
//! outside the worktree lives in the sandbox and its snapshots.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use std::sync::Arc;

use branchyard_harness::{Driver, Open};
use branchyard_recipe::{Recipe, RecipeProvider};
use branchyard_runtime::{RuntimeError, Session};
use branchyard_sandbox::{Mount, Resources, SandboxProvider, SandboxSpec};
use branchyard_substrate::transfer::{self, Guest, Pushed};
use branchyard_substrate::SubstrateProvider;
use serde_json::{json, Value};

use crate::snapshots::{self, SandboxEvent, SandboxOrigin};
use crate::state::{now_ms, Begun, Fence, Record, SandboxKind};
use crate::{
    git, harness, Activity, Error, Provider, RecipeOptions, SandboxOptions, SubstrateOptions, Yard,
};

/// Where the worktree appears in a sandbox.
pub const WORKSPACE: &str = "/workspace";
/// Where the branch's private home appears in a sandbox.
pub const HOME: &str = "/branchyard/home";
/// Where a Microsandbox turn's authorized scratch areas are mounted, one
/// subdirectory per area named for it.
const SCRATCH_MOUNT_BASE: &str = "/branchyard/scratch";
/// The journaled step naming a turn's sandbox, before it is created.
pub(crate) const STEP_SANDBOX: &str = "sandbox";

/// Whether this provider runs the harness somewhere other than this host.
pub(crate) fn sandboxed(provider: Option<&Provider>) -> bool {
    matches!(
        provider,
        Some(Provider::Microsandbox(_) | Provider::Substrate(_) | Provider::Recipe(_))
    )
}

/// Refuse a provider this build or its options cannot run, before anything
/// is created. A yard given its own sandbox provider
/// ([`Yard::use_sandbox_provider`]) runs Microsandbox branches without the
/// SDK.
pub(crate) fn check(yard: &Yard, provider: Option<&Provider>) -> Result<(), Error> {
    match provider {
        None | Some(Provider::Local) => Ok(()),
        Some(Provider::Microsandbox(options)) => {
            let own = crate::projection::lock(&yard.hub.sandbox_provider).is_some();
            if !own && !branchyard_microsandbox::ENABLED {
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
        Some(Provider::Recipe(options)) => check_recipe(options),
    }
}

fn check_recipe(options: &RecipeOptions) -> Result<(), Error> {
    let refuse = |why: String| {
        Err(Error::Unsupported(format!(
            "the recipe provider (recipe {}) {why}",
            options.name
        )))
    };
    if options.name.trim().is_empty() {
        return Err(Error::Unsupported(
            "the recipe provider needs a recipe's name".into(),
        ));
    }
    if options.create.trim().is_empty() {
        return refuse("has no create command".into());
    }
    for (what, path) in [("workdir", &options.workdir), ("home", &options.home)] {
        if !path.is_empty() && !Path::new(path).is_absolute() {
            return refuse(format!("needs an absolute {what}, not {path:?}"));
        }
    }
    if options.keep == crate::SandboxKeep::Pause
        && (options.suspend.is_none() || options.resume.is_none())
    {
        return refuse(
            "cannot keep its machine paused between turns: it needs both suspend and resume".into(),
        );
    }
    Ok(())
}

fn check_substrate(options: &SubstrateOptions) -> Result<(), Error> {
    let refuse = |why: String| Err(Error::Unsupported(format!("the substrate provider {why}")));
    // Schemes, loopback-only plain HTTP, and the TLS files.
    if let Err(error) = substrate_config(options).check() {
        return refuse(format!("cannot use its options: {error}"));
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

/// The harness's working directory and `HOME` as the harness will see
/// them, before anything is created: a local harness's `HOME` is its
/// private home, or this process's own.
pub(crate) fn guest_paths(record: &Record) -> (String, String) {
    match &record.provider {
        None | Some(Provider::Local) => (
            record.info.worktree.display().to_string(),
            record.home.as_ref().map_or_else(
                || std::env::var("HOME").unwrap_or_default(),
                |home| home.display().to_string(),
            ),
        ),
        Some(Provider::Microsandbox(_)) => (WORKSPACE.into(), HOME.into()),
        Some(Provider::Substrate(options)) => {
            (options.workdir().to_owned(), options.home().to_owned())
        }
        Some(Provider::Recipe(options)) => recipe_paths(record, options),
    }
}

/// A recipe branch's worktree and home on its machine.
fn recipe_paths(record: &Record, options: &RecipeOptions) -> (String, String) {
    let (branch, worktree) = (&record.info.name, &record.info.worktree);
    (
        options.workdir(branch, worktree),
        options.home(branch, worktree),
    )
}

/// The spec of a Microsandbox branch's sandbox, and its harness's
/// variables: the worktree at [`WORKSPACE`], the private home at [`HOME`],
/// the git directory read-only, and every scratch area it may reach.
fn microsandbox_spec(
    yard: &Yard,
    record: &Record,
    options: &SandboxOptions,
) -> Result<(SandboxSpec, BTreeMap<OsString, OsString>), String> {
    let home = record
        .home
        .clone()
        .ok_or("a sandboxed branch has no private home")?;
    let mut env = sandbox_env(HOME, &options.pass_env)?;
    let git_dir = git::common_dir(&yard.root).map_err(|e| e.to_string())?;
    let mut mounts = vec![
        Mount::writable(&record.info.worktree, WORKSPACE),
        Mount::writable(home, HOME),
        Mount::read_only(&git_dir, &git_dir),
    ];
    // Every scratch area this branch may reach is mounted read-write at a
    // fixed guest path, named for the harness the same way the local
    // provider's environment variable does; see `docs/storage.md`. One
    // writer at a time is still enforced by `by scratch lock`, not by this
    // mount.
    if let Ok(areas) = crate::storage::authorized_scratch(yard, &record.info.name) {
        for area in &areas {
            let host = crate::storage::scratch_dir(&yard.store(), &area.name);
            let _ = std::fs::create_dir_all(&host);
            let guest = format!("{SCRATCH_MOUNT_BASE}/{}", area.name);
            mounts.push(Mount::writable(&host, &guest));
            env.insert(
                crate::storage::scratch_env_var(&area.name).into(),
                guest.into(),
            );
        }
    }
    let keep = snapshots::lifecycle(record.provider.as_ref()).is_some_and(|l| l.keep);
    let spec = SandboxSpec {
        name: sandbox_name(&record.info.name, now_ms()),
        image: Some(options.image.clone()),
        resources: Resources {
            cpus: options.cpus,
            memory_mib: options.memory_mib,
        },
        mounts,
        // A kept sandbox outlives this process.
        persist: keep,
    };
    Ok((spec, env))
}

/// A sandbox's spec and the provider it is created through.
pub(crate) type Planned = (SandboxSpec, Arc<dyn SandboxProvider>);

/// For a fan whose setup runs once: the spec of `record`'s sandbox, when
/// its placement mounts the worktree (so a sandbox branched from another
/// can be rebound to it), and its provider. `None` otherwise.
pub(crate) fn fan_spec(yard: &Yard, record: &Record) -> Result<Option<Planned>, String> {
    let Some(Provider::Microsandbox(options)) = &record.provider else {
        return Ok(None);
    };
    let (spec, _) = microsandbox_spec(yard, record, options)?;
    Ok(Some((spec, microsandbox(yard, options)?)))
}

/// Journal `record`'s turn's `sandbox` step for a sandbox made for it
/// before its turn (a fan's), so recovery destroys it if this engine stops
/// first.
pub(crate) fn journal_handed(
    yard: &Yard,
    record: &Record,
    fence: &Fence,
    name: &str,
) -> Result<(), String> {
    let provider = record
        .provider
        .as_ref()
        .map(snapshots::provider_name)
        .unwrap_or("");
    journal_sandbox(yard, fence, name, json!({ "provider": provider }))
}

/// Where a turn's sandbox comes from, when the caller already knows.
#[derive(Clone, Debug, Default)]
pub(crate) enum SandboxPlan {
    /// Kept, branched from the branch's seed, or fresh: see
    /// [`snapshots::acquire`].
    #[default]
    Default,
    /// A sandbox made for this turn already (a fan's branch of its prepared
    /// sandbox), whose `sandbox` step is journaled.
    Handed { name: String, origin: SandboxOrigin },
    /// A fresh one, for this reason.
    Fresh(String),
}

/// The prepared environment a branch whose setup has not run starts its
/// sandbox from, on provider `key`.
fn environment(
    yard: &Yard,
    record: &Record,
    key: &str,
) -> Option<crate::environments::SandboxEnvironment> {
    let workspace = record.workspace.as_ref().filter(|w| !w.ready)?;
    crate::environments::for_sandbox(&yard.root, &workspace.spec, &record.info.worktree, key)
}

/// A turn's harness location. A sandbox is destroyed, or parked, by
/// [`Placement::release`]; destroyed on drop otherwise.
pub(crate) struct Placement {
    cwd: String,
    kind: Kind,
    started: Option<SandboxEvent>,
    /// The turn's egress proxy, kept until the harness is gone; a
    /// confined local harness is started in its own network namespace.
    egress: Option<crate::egress::Egress>,
    /// The sandbox's record in the repository's service registry while the
    /// turn holds it: what a reaper recovers and destroys if this process
    /// stops. Deregistered once the sandbox is released or discarded.
    service: Option<crate::services::Registration>,
}

/// Register the turn's sandbox `name` in the repository's service
/// registry, owned by this process for `record`'s branch, with what
/// reclaims it: the branch's recovery, then its provider's destroy.
fn register_sandbox(
    yard: &Yard,
    record: &Record,
    name: &str,
) -> Option<crate::services::Registration> {
    use crate::services::{Reclaim, Service, ServiceOwner, KIND_RECIPE_MACHINE, KIND_SANDBOX};
    let provider = record.provider.clone()?;
    let kind = match &provider {
        Provider::Local => return None,
        Provider::Recipe(_) => KIND_RECIPE_MACHINE,
        _ => KIND_SANDBOX,
    };
    let branch = &record.info.name;
    let service = Service::new(kind, ServiceOwner::this_process().for_branch(branch))
        .with("provider", snapshots::provider_name(&provider))
        .with("branch", branch.as_str())
        .with("sandbox", name)
        .with_reclaim(Reclaim::Sandbox {
            root: yard.root.clone(),
            branch: branch.clone(),
            provider: Box::new(provider),
            sandbox: name.to_owned(),
        });
    yard.register_service(service, crate::services::DEFAULT_TTL)
        .ok()
}

enum Kind {
    Local(branchyard_runtime::Environment),
    Sandbox {
        provider: Arc<dyn SandboxProvider>,
        name: String,
        env: BTreeMap<OsString, OsString>,
        released: bool,
    },
    Substrate(Box<Actor>),
}

/// A sandbox the worktree is copied into and back from: a Substrate actor
/// (through its bridge) or a recipe's machine (through execs).
enum Remote {
    Substrate(Box<SubstrateProvider>),
    Recipe(Arc<RecipeProvider>),
}

impl Remote {
    fn provider(&self) -> &dyn SandboxProvider {
        match self {
            Remote::Substrate(provider) => provider.as_ref(),
            Remote::Recipe(provider) => provider.as_ref(),
        }
    }

    /// `actor` or `machine`, for messages.
    fn noun(&self) -> &'static str {
        match self {
            Remote::Substrate(_) => "actor",
            Remote::Recipe(_) => "machine",
        }
    }

    /// How the transfer reaches sandbox `name`.
    fn guest<'a>(&'a self, name: &'a str) -> Result<Box<dyn Guest + 'a>, String> {
        match self {
            Remote::Substrate(provider) => provider
                .endpoint(name)
                .map(|endpoint| Box::new(endpoint) as Box<dyn Guest>)
                .map_err(|e| e.to_string()),
            Remote::Recipe(provider) => Ok(Box::new(transfer::Exec::new(provider.as_ref(), name))),
        }
    }
}

/// How [`Placement::copied`] makes and reaches its sandbox.
struct Copied<'a> {
    remote: Remote,
    /// The provider's name in events: `substrate` or `recipe`.
    kind: &'static str,
    /// A fresh sandbox's spec.
    spec: SandboxSpec,
    /// The `sandbox` step's intent for a sandbox's name.
    intent: Box<dyn Fn(&str) -> Value + 'a>,
    workdir: String,
    home: String,
    pass_env: &'a [String],
}

/// A turn's Substrate actor or recipe machine, and what must come back
/// from it.
struct Actor {
    provider: Remote,
    name: String,
    env: BTreeMap<OsString, OsString>,
    worktree: PathBuf,
    pushed: Option<Pushed>,
    /// The private home on this host, and where it is in the actor.
    home: Option<(PathBuf, PathBuf)>,
    released: bool,
}

impl Actor {
    /// Bring the worktree and home back. Returns what went wrong, if
    /// anything.
    fn bring_back(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        let noun = self.provider.noun();
        match self.provider.guest(&self.name) {
            Ok(endpoint) => {
                if let Some(pushed) = &self.pushed {
                    if let Err(error) = transfer::pull(endpoint.as_ref(), pushed, &self.worktree) {
                        warnings.push(format!(
                            "could not bring the worktree back from {noun} {}: {error}",
                            self.name
                        ));
                    }
                }
                if let Some((host, guest)) = &self.home {
                    if let Err(error) = transfer::pull_tree(endpoint.as_ref(), guest, host) {
                        warnings.push(format!(
                            "could not bring the home directory back from {noun} {}: {error}",
                            self.name
                        ));
                    }
                }
            }
            Err(error) if self.pushed.is_some() => warnings.push(format!(
                "could not bring anything back from {noun} {}: {error}",
                self.name
            )),
            Err(_) => {}
        }
        self.pushed = None;
        warnings
    }

    /// Bring the worktree and home back, then delete the actor. Returns
    /// what went wrong, if anything; the actor is deleted regardless.
    fn release(&mut self) -> Vec<String> {
        if std::mem::replace(&mut self.released, true) {
            return Vec::new();
        }
        let mut warnings = self.bring_back();
        if let Err(error) = self.provider.provider().destroy(&self.name) {
            warnings.push(match &self.provider {
                Remote::Substrate(_) => format!("could not delete actor {}: {error}", self.name),
                Remote::Recipe(_) => format!("could not destroy machine {}: {error}", self.name),
            });
        }
        warnings
    }
}

/// Journal the turn's `sandbox` step naming `name` (and `extra`) before it
/// is created or resumed. A step an earlier choice of this turn began, for a
/// sandbox that was not used, is replaced.
fn journal_sandbox(yard: &Yard, fence: &Fence, name: &str, extra: Value) -> Result<(), String> {
    let store = yard.store();
    let mut intent = extra;
    intent["sandbox"] = json!(name);
    let backend = store.backend();
    let begun = backend
        .begin_step(fence, fence.turn, STEP_SANDBOX, &intent)
        .map_err(|e| format!("could not record sandbox {name}: {e}"))?;
    if let Begun::Pending(earlier) = begun {
        if earlier != intent {
            backend
                .abandon_step(fence, fence.turn, STEP_SANDBOX)
                .and_then(|()| backend.begin_step(fence, fence.turn, STEP_SANDBOX, &intent))
                .map_err(|e| format!("could not record sandbox {name}: {e}"))?;
        }
    }
    Ok(())
}

impl Placement {
    /// Prepare the record's provider: for a sandbox, get it now (kept,
    /// branched, handed over or fresh, as `plan` and the store say). A
    /// failure is the turn's failure reason.
    pub fn prepare(
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        plan: &SandboxPlan,
    ) -> Result<Placement, String> {
        let options = match &record.provider {
            None | Some(Provider::Local) => {
                return Ok(Placement {
                    cwd: record.info.worktree.display().to_string(),
                    kind: Kind::Local(harness::environment(record.home.as_deref())),
                    started: None,
                    egress: None,
                    service: None,
                })
            }
            Some(Provider::Substrate(options)) => {
                return Placement::substrate(yard, record, fence, options, plan)
            }
            Some(Provider::Recipe(options)) => {
                return Placement::recipe(yard, record, fence, options, plan)
            }
            Some(Provider::Microsandbox(options)) => options,
        };
        let (spec, env) = microsandbox_spec(yard, record, options)?;
        let provider = microsandbox(yard, options)?;
        let key = snapshots::provider_key(record.provider.as_ref().expect("matched above"));
        let journal =
            |name: &str| journal_sandbox(yard, fence, name, json!({ "provider": "microsandbox" }));
        let acquired = match plan {
            SandboxPlan::Handed { name, origin } => {
                journal(name)?;
                snapshots::Acquired {
                    name: name.clone(),
                    origin: origin.clone(),
                }
            }
            SandboxPlan::Fresh(reason) => {
                journal(&spec.name)?;
                provider.ensure(&spec).map_err(|e| {
                    let _ = provider.destroy(&spec.name);
                    format!("could not create sandbox {}: {e}", spec.name)
                })?;
                snapshots::Acquired {
                    name: spec.name.clone(),
                    origin: SandboxOrigin::Fresh {
                        reason: Some(reason.clone()),
                    },
                }
            }
            SandboxPlan::Default => {
                let store = yard.store();
                let environment = environment(yard, record, &key);
                snapshots::acquire(
                    &store,
                    record,
                    fence,
                    provider.as_ref(),
                    &key,
                    &spec,
                    environment.as_ref(),
                    &journal,
                )
                .inspect_err(|_| {
                    let _ = provider.destroy(&spec.name);
                })?
            }
        };
        let store = yard.store();
        let _ = store.backend().finish_step(
            fence,
            fence.turn,
            STEP_SANDBOX,
            &json!({ "created": true, "sandbox": acquired.name }),
        );
        let service = register_sandbox(yard, record, &acquired.name);
        Ok(Placement {
            cwd: WORKSPACE.into(),
            started: Some(SandboxEvent::Started {
                provider: "microsandbox".into(),
                sandbox: acquired.name.clone(),
                origin: acquired.origin,
            }),
            egress: None,
            service,
            kind: Kind::Sandbox {
                provider,
                name: acquired.name,
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
        plan: &SandboxPlan,
    ) -> Result<Placement, String> {
        let provider = substrate(options)?;
        let atespace = options.atespace().to_owned();
        Placement::copied(
            yard,
            record,
            fence,
            plan,
            Copied {
                remote: Remote::Substrate(Box::new(provider)),
                kind: "substrate",
                spec: SandboxSpec::new(actor_name(&record.info.name, now_ms())),
                // Journaled before the actor exists, so recovery can delete it.
                intent: Box::new(
                    move |name| json!({ "provider": "substrate", "actor": name, "atespace": atespace }),
                ),
                workdir: options.workdir().to_owned(),
                home: options.home().to_owned(),
                pass_env: &options.pass_env,
            },
        )
    }

    fn recipe(
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        options: &RecipeOptions,
        plan: &SandboxPlan,
    ) -> Result<Placement, String> {
        let (workdir, home) = recipe_paths(record, options);
        let recipe = options.name.clone();
        Placement::copied(
            yard,
            record,
            fence,
            plan,
            Copied {
                remote: Remote::Recipe(recipe_provider(yard, options)),
                kind: "recipe",
                spec: SandboxSpec::new(sandbox_name(&record.info.name, now_ms())),
                // Journaled before `create` runs, so recovery can destroy it.
                intent: Box::new(
                    move |name| json!({ "provider": "recipe", "sandbox": name, "recipe": recipe }),
                ),
                workdir,
                home,
                pass_env: &options.pass_env,
            },
        )
    }

    /// A sandbox the worktree and home are copied into, and back from when
    /// the turn ends.
    fn copied(
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        plan: &SandboxPlan,
        how: Copied<'_>,
    ) -> Result<Placement, String> {
        let home = record
            .home
            .clone()
            .ok_or("a sandboxed branch has no private home")?;
        let env = sandbox_env(&how.home, how.pass_env)?;
        let (remote, spec) = (how.remote, how.spec);
        let noun = remote.noun();
        let key = snapshots::provider_key(record.provider.as_ref().expect("a sandbox provider"));
        let journal = |name: &str| journal_sandbox(yard, fence, name, (how.intent)(name));
        let store = yard.store();
        let acquired = match plan {
            SandboxPlan::Handed { name, origin } => journal(name).map(|()| snapshots::Acquired {
                name: name.clone(),
                origin: origin.clone(),
            }),
            SandboxPlan::Fresh(reason) => journal(&spec.name).and_then(|()| {
                remote
                    .provider()
                    .ensure(&spec)
                    .map(|_| snapshots::Acquired {
                        name: spec.name.clone(),
                        origin: SandboxOrigin::Fresh {
                            reason: Some(reason.clone()),
                        },
                    })
                    .map_err(|e| match &remote {
                        Remote::Substrate(_) => {
                            format!("could not create actor {}: {e}", spec.name)
                        }
                        Remote::Recipe(_) => {
                            format!("could not create machine {}: {e}", spec.name)
                        }
                    })
            }),
            SandboxPlan::Default => {
                let environment = environment(yard, record, &key);
                snapshots::acquire(
                    &store,
                    record,
                    fence,
                    remote.provider(),
                    &key,
                    &spec,
                    environment.as_ref(),
                    &journal,
                )
            }
        };
        let name = acquired
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_else(|_| spec.name.clone());
        let finished = match &remote {
            Remote::Substrate(provider) => json!({
                "uid": provider
                    .handle(&name)
                    .ok()
                    .flatten()
                    .map(|h| h.uid)
                    .unwrap_or_default()
            }),
            Remote::Recipe(_) => json!({ "created": acquired.is_ok(), "sandbox": name }),
        };
        let _ = store
            .backend()
            .finish_step(fence, fence.turn, STEP_SANDBOX, &finished);
        let mut actor = Box::new(Actor {
            provider: remote,
            name: name.clone(),
            env,
            worktree: record.info.worktree.clone(),
            pushed: None,
            home: None,
            released: false,
        });
        let fail = |actor: &mut Actor, why: String| -> Result<Placement, String> {
            let warnings = actor.release();
            match warnings.is_empty() {
                true => Err(why),
                false => Err(format!("{why}; {}", warnings.join("; "))),
            }
        };
        let acquired = match acquired {
            Ok(acquired) => acquired,
            Err(error) => return fail(&mut actor, error),
        };
        let service = register_sandbox(yard, record, &name);
        let workdir = PathBuf::from(&how.workdir);
        let guest_home = PathBuf::from(&how.home);
        let pushed = (|| -> Result<Pushed, String> {
            let endpoint = actor.provider.guest(&name)?;
            // A resumed or branched sandbox still holds a worktree and a
            // home from before: the worktree's files go (what git ignores,
            // such as what setup installed, stays), and so does the home,
            // before this host's are sent.
            let cleared = match (&acquired.origin, &actor.provider) {
                (SandboxOrigin::Fresh { .. }, Remote::Substrate(_)) => Ok(()),
                // A recipe may hand out a machine that outlives its
                // sandboxes (a lab box): an earlier push there goes too.
                (SandboxOrigin::Fresh { .. }, Remote::Recipe(_)) => {
                    transfer::clear_previous_push(endpoint.as_ref(), &workdir)
                }
                _ => transfer::clear_for_push(endpoint.as_ref(), &workdir, &guest_home),
            };
            cleared.map_err(|e| format!("could not clear {noun} {name} for this turn: {e}"))?;
            let stage = staging(yard, &name);
            let pushed = transfer::push_staged(
                endpoint.as_ref(),
                &record.info.worktree,
                &workdir,
                Some(&stage),
            )
            .map_err(|e| format!("could not copy the worktree into {noun} {name}: {e}"))?;
            Ok(pushed)
        })();
        match pushed {
            Ok(pushed) => actor.pushed = Some(pushed),
            Err(error) => return fail(&mut actor, error),
        }
        let home_sent = actor
            .provider
            .guest(&name)
            .and_then(|endpoint| {
                transfer::push_tree(endpoint.as_ref(), &home, &guest_home)
                    .map_err(|e| e.to_string())
            })
            .map_err(|e| format!("could not copy the home directory into {noun} {name}: {e}"));
        if let Err(error) = home_sent {
            return fail(&mut actor, error);
        }
        actor.home = Some((home, guest_home));
        Ok(Placement {
            cwd: how.workdir,
            started: Some(SandboxEvent::Started {
                provider: how.kind.into(),
                sandbox: name,
                origin: acquired.origin,
            }),
            egress: None,
            service,
            kind: Kind::Substrate(actor),
        })
    }

    /// Where the turn's sandbox came from, to record; `None` for a local
    /// harness.
    pub fn started(&self) -> Option<SandboxEvent> {
        self.started.clone()
    }

    /// The turn's sandbox and its provider, for running the workspace's
    /// scripts in it; `None` for a local harness.
    pub fn sandbox(&self) -> Option<(&dyn SandboxProvider, &str)> {
        match &self.kind {
            Kind::Local(_) => None,
            Kind::Sandbox { provider, name, .. } => Some((provider.as_ref(), name.as_str())),
            Kind::Substrate(actor) => Some((actor.provider.provider(), actor.name.as_str())),
        }
    }

    /// Whether the worktree is mounted into the sandbox (so what setup
    /// leaves in it is on this host), rather than copied in and out.
    pub fn mounts_worktree(&self) -> bool {
        matches!(self.kind, Kind::Sandbox { .. })
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

    /// Take a variable out of the harness's environment; a later
    /// [`Placement::set_env`] of it still applies.
    pub fn remove_env(&mut self, name: &str) {
        match &mut self.kind {
            Kind::Local(env) => *env = env.clone().remove(name),
            Kind::Sandbox { env, .. } => {
                env.remove(std::ffi::OsStr::new(name));
            }
            Kind::Substrate(actor) => {
                actor.env.remove(std::ffi::OsStr::new(name));
            }
        }
    }

    /// The harness's working directory, as the harness sees it.
    pub fn cwd(&self) -> String {
        self.cwd.clone()
    }

    /// Apply the turn's egress: its variables now, and, for a confined
    /// local harness, its network namespace when [`Placement::start`]
    /// starts it.
    pub fn egress(&mut self, egress: crate::egress::Egress) {
        for (name, value) in egress.env() {
            self.set_env(name, value);
        }
        self.egress = Some(egress);
    }

    pub fn start(&self, driver: Box<dyn Driver>, open: Open) -> Result<Session, RuntimeError> {
        match &self.kind {
            Kind::Local(env) => match self.egress.as_ref().filter(|e| e.confined()) {
                Some(egress) => {
                    let (session, listener) = Session::start_confined(
                        driver,
                        open,
                        env,
                        None,
                        crate::egress::CONFINED_PORT,
                    )?;
                    egress.serve(listener).map_err(|source| RuntimeError::Io {
                        context: "the egress proxy",
                        source,
                    })?;
                    Ok(session)
                }
                None => Session::start(driver, open, env, None),
            },
            Kind::Sandbox {
                provider,
                name,
                env,
                ..
            } => Session::start_in(driver, open, provider.as_ref(), name, env.clone(), None),
            Kind::Substrate(actor) => Session::start_in(
                driver,
                open,
                actor.provider.provider(),
                &actor.name,
                actor.env.clone(),
                None,
            ),
        }
    }

    /// End the turn's sandbox, if any: park it for the next turn when the
    /// branch keeps its sandbox and the provider can pause, else destroy
    /// it. A Substrate actor's worktree and home come back first. Returns
    /// what to record.
    pub fn release(&mut self, yard: &Yard, record: &Record, fence: &Fence) -> Vec<Activity> {
        let said = self.release_sandbox(yard, record, fence);
        // Destroyed, or kept for the next turn in the sandbox store: no
        // longer this turn's to reclaim.
        self.service = None;
        said
    }

    fn release_sandbox(&mut self, yard: &Yard, record: &Record, fence: &Fence) -> Vec<Activity> {
        match &mut self.kind {
            Kind::Local(_) => Vec::new(),
            Kind::Sandbox {
                provider,
                name,
                released,
                ..
            } => {
                if std::mem::replace(released, true) {
                    return Vec::new();
                }
                snapshots::park(yard, record, fence, provider.as_ref(), name)
            }
            Kind::Substrate(actor) => {
                if std::mem::replace(&mut actor.released, true) {
                    return Vec::new();
                }
                let warnings = actor.bring_back();
                let mut said: Vec<Activity> = warnings.into_iter().map(Activity::Warning).collect();
                said.extend(snapshots::park(
                    yard,
                    record,
                    fence,
                    actor.provider.provider(),
                    &actor.name,
                ));
                said
            }
        }
    }

    /// Destroy the sandbox without keeping it; for a Substrate actor, bring
    /// the worktree and home back first. A failure is returned as a warning.
    pub fn discard(&mut self) -> Option<String> {
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
        self.discard();
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

/// The provider a Microsandbox branch runs through: the yard's own
/// ([`Yard::use_sandbox_provider`]), or the SDK's, with live branching
/// declared only when `options` opt in.
pub(crate) fn microsandbox(
    yard: &Yard,
    options: &SandboxOptions,
) -> Result<Arc<dyn SandboxProvider>, String> {
    if let Some(own) = crate::projection::lock(&yard.hub.sandbox_provider).clone() {
        return Ok(own);
    }
    sdk(options.live_branch)
}

#[cfg(feature = "microsandbox")]
fn sdk(live_branch: bool) -> Result<Arc<dyn SandboxProvider>, String> {
    branchyard_microsandbox::MicrosandboxProvider::new()
        .map(|p| Arc::new(p.with_live_branch(live_branch)) as Arc<dyn SandboxProvider>)
        .map_err(|e| format!("could not start the Microsandbox SDK: {e}"))
}

#[cfg(not(feature = "microsandbox"))]
fn sdk(_: bool) -> Result<Arc<dyn SandboxProvider>, String> {
    Err("this build has no Microsandbox support".into())
}

/// The provider configuration `options` describe, without the key.
fn substrate_config(options: &SubstrateOptions) -> branchyard_substrate::Config {
    let mut config = branchyard_substrate::Config::new(
        &options.endpoint,
        options.atespace(),
        &options.template,
        &options.router,
    );
    config.ca = options.ca.clone();
    config.client_cert = options.client_cert.clone();
    config.client_key = options.client_key.clone();
    config.router_ca = options.router_ca.clone();
    config.insecure = options.insecure;
    config
}

/// A provider for `options`; `signed` includes the bridge key.
pub(crate) fn substrate_provider(
    options: &SubstrateOptions,
    signed: bool,
) -> Result<SubstrateProvider, String> {
    let mut config = substrate_config(options);
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

/// A provider for `options` that signs bridge credentials.
pub(crate) fn substrate_signed(options: &SubstrateOptions) -> Result<SubstrateProvider, String> {
    substrate(options)
}

/// Where a turn's transfer to `actor` is staged, so recovery can delete it.
fn staging(yard: &Yard, actor: &str) -> PathBuf {
    yard.store().dir().join("transfer").join(actor)
}

/// Clean up the sandbox a stopped engine's turn journaled in its
/// `sandbox` step: a Substrate actor's work is brought back, then the actor
/// and its transfer's staging directory are deleted; a Microsandbox sandbox
/// is destroyed. A sandbox the turn had already parked (its `sandbox_park`
/// step finished with it kept) stays, recorded for the next turn; a kept
/// record for one destroyed here is removed. Returns what recovery should
/// report, if anything.
pub(crate) fn recover(
    yard: &Yard,
    record: &Record,
    intent: &Value,
    parked: Option<&Value>,
) -> Option<String> {
    let name = intent
        .get("actor")
        .or_else(|| intent.get("sandbox"))?
        .as_str()?
        .to_owned();
    if parked.and_then(|o| o.get("kept")).and_then(Value::as_bool) == Some(true) {
        return Some(format!(
            "its sandbox {name} was already kept paused for the next turn"
        ));
    }
    let said = match &record.provider {
        Some(Provider::Substrate(options)) => recover_actor(yard, record, options, &name),
        Some(Provider::Microsandbox(options)) => destroy_orphan(microsandbox(yard, options), &name),
        Some(Provider::Recipe(options)) => recover_machine(yard, record, options, &name),
        None | Some(Provider::Local) => return None,
    };
    let _ = yard
        .store()
        .sandboxes()
        .take_sandbox(&record.info.name, SandboxKind::Kept, &name);
    Some(said)
}

/// Destroy the Microsandbox sandbox `name` a stopped engine left, through
/// `provider`, and say what happened.
pub(crate) fn destroy_orphan(
    provider: Result<Arc<dyn SandboxProvider>, String>,
    name: &str,
) -> String {
    let destroyed = provider.and_then(|provider| {
        let existed = provider.inspect(name).map_err(|e| e.to_string())?.is_some();
        provider.destroy(name).map_err(|e| e.to_string())?;
        Ok(existed)
    });
    match destroyed {
        Ok(true) => format!("destroyed its Microsandbox sandbox {name}"),
        Ok(false) => format!("its Microsandbox sandbox {name} was already gone"),
        Err(error) => format!("could not destroy its Microsandbox sandbox {name}: {error}"),
    }
}

/// Bring back what the harness left in the actor a stopped engine's turn
/// journaled, if it still exists, as the turn's end would have: a fresh
/// attempt credential from the host key, the actor's working files applied
/// to the worktree only if the worktree still holds exactly what was sent,
/// and the home. Then delete the actor and the transfer's staging
/// directory. Returns what recovery should report.
fn recover_actor(yard: &Yard, record: &Record, options: &SubstrateOptions, actor: &str) -> String {
    let stage = (!actor.is_empty() && !actor.contains(['/', '.'])).then(|| staging(yard, actor));
    let mut said = Vec::new();
    // Deleting needs only the Control API; bringing work back needs the key.
    let deleted = substrate_provider(options, false).and_then(|provider| {
        let existed = provider.handle(actor).map_err(|e| e.to_string())?.is_some();
        if existed {
            said.push(bring_back(record, options, actor, stage.as_deref()));
        }
        provider.destroy(actor).map_err(|e| e.to_string())?;
        Ok(existed)
    });
    if let Some(stage) = &stage {
        let _ = std::fs::remove_dir_all(stage);
    }
    said.push(match deleted {
        Ok(true) => format!("deleted its Substrate actor {actor}"),
        Ok(false) => format!("its Substrate actor {actor} was already gone"),
        Err(error) => format!("could not delete its Substrate actor {actor}: {error}"),
    });
    said.join("; ")
}

/// Pull the worktree and home back from `actor`, and say what happened.
fn bring_back(
    record: &Record,
    options: &SubstrateOptions,
    actor: &str,
    stage: Option<&Path>,
) -> String {
    let pulled = (|| {
        let stage = stage.ok_or("its actor's name cannot name a staging directory")?;
        let provider = substrate(options)?;
        provider
            .begin_attempt(actor, "recovery")
            .map_err(|e| e.to_string())?;
        let endpoint = provider.endpoint(actor).map_err(|e| e.to_string())?;
        let paths = (Path::new(options.workdir()), Path::new(options.home()));
        let pulled = pull_staged(&endpoint, record, paths, stage);
        let _ = provider.end_attempt(actor);
        pulled
    })();
    said_back(pulled, "actor", actor)
}

/// Pull the worktree and home a stopped engine's push to `workdir` and
/// `home` left in `stage` back through `endpoint`: whether any file
/// changed, and what to add about the home.
fn pull_staged(
    endpoint: &dyn Guest,
    record: &Record,
    (workdir, home): (&Path, &Path),
    stage: &Path,
) -> Result<(bool, String), String> {
    let worktree = &record.info.worktree;
    if !stage.is_dir() {
        return Err(
            "its transfer's staging directory is gone: the worktree was never sent to it, \
             or was already brought back"
                .to_owned(),
        );
    }
    let pushed = transfer::reopen_staged(worktree, workdir, stage).map_err(|e| e.to_string())?;
    let pulled = transfer::pull(endpoint, &pushed, worktree).map_err(|e| e.to_string())?;
    let said = match &record.home {
        Some(host) => transfer::pull_tree(endpoint, home, host)
            .err()
            .map(|e| format!("; its home directory was not brought back: {e}")),
        None => None,
    };
    Ok((pulled.changed, said.unwrap_or_default()))
}

/// What recovery says of [`pull_staged`]'s result from `noun` `name`.
fn said_back(pulled: Result<(bool, String), String>, noun: &str, name: &str) -> String {
    match pulled {
        Ok((true, home)) => {
            format!("brought the harness's work in {noun} {name} back to the worktree{home}")
        }
        Ok((false, home)) => format!(
            "the harness had changed no files in {noun} {name}; the worktree is as it was{home}"
        ),
        Err(why) => format!("did not bring the harness's work back from {noun} {name}: {why}"),
    }
}

/// The provider for a recipe branch: its machines' records in the store's
/// `recipes` directory, shared by every process on this repository, and
/// `$BRANCHYARD_SSH` (default `ssh`) as the ssh program.
pub(crate) fn recipe_provider(yard: &Yard, options: &RecipeOptions) -> Arc<RecipeProvider> {
    let mut recipe = Recipe::new(&options.name, &yard.root, &options.create)
        .with_destroy(options.destroy.as_deref());
    recipe.suspend = options.suspend.clone();
    recipe.resume = options.resume.clone();
    if let Some(seconds) = options.timeout_seconds {
        recipe.timeout = Duration::from_secs(seconds);
    }
    let ssh = std::env::var("BRANCHYARD_SSH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "ssh".into());
    Arc::new(RecipeProvider::new(recipe, ssh).with_state_dir(yard.store().dir().join("recipes")))
}

/// Bring back what the harness left on the recipe machine a stopped
/// engine's turn journaled, if the machine is still recorded: what still
/// runs there is stopped first, then the worktree (only if the host's still
/// holds exactly what was sent) and the home come back, and the machine is
/// destroyed through the recipe. Returns what recovery should report.
fn recover_machine(yard: &Yard, record: &Record, options: &RecipeOptions, machine: &str) -> String {
    // A machine's name is `by-<branch>-<ms>`; a branch name may hold dots.
    let stage = (!machine.is_empty() && !machine.contains('/') && !machine.starts_with('.'))
        .then(|| staging(yard, machine));
    let provider = recipe_provider(yard, options);
    let mut said = Vec::new();
    let existed = matches!(provider.inspect(machine), Ok(Some(_)));
    if existed {
        let pulled = (|| {
            let stage = stage
                .as_deref()
                .ok_or("its machine's name cannot name a staging directory")?;
            // The harness outlives a dead engine's ssh connection: stop it,
            // then make the machine usable for the transfer again.
            provider.stop(machine).map_err(|e| e.to_string())?;
            provider
                .ensure(&SandboxSpec::new(machine))
                .map_err(|e| e.to_string())?;
            let (workdir, home) = recipe_paths(record, options);
            let endpoint = transfer::Exec::new(provider.as_ref(), machine);
            pull_staged(
                &endpoint,
                record,
                (Path::new(&workdir), Path::new(&home)),
                stage,
            )
        })();
        said.push(said_back(pulled, "machine", machine));
    }
    let destroyed = provider.destroy(machine);
    if let Some(stage) = &stage {
        let _ = std::fs::remove_dir_all(stage);
    }
    said.push(match (destroyed, existed) {
        (Ok(()), true) => format!("destroyed its machine {machine} (recipe {})", options.name),
        (Ok(()), false) => format!("its machine {machine} was already gone"),
        (Err(error), _) => format!("could not destroy its machine {machine}: {error}"),
    });
    said.join("; ")
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
        let dir = tempfile::Builder::new()
            .prefix("by-placement-")
            .tempdir()
            .unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        let yard = Yard::open(dir.path()).unwrap();
        assert!(check(&yard, None).is_ok());
        assert!(check(&yard, Some(&Provider::Local)).is_ok());
        let empty = Provider::Microsandbox(SandboxOptions::default());
        assert!(matches!(
            check(&yard, Some(&empty)),
            Err(Error::Unsupported(_))
        ));
        let image = Provider::Microsandbox(SandboxOptions {
            image: "alpine:3.20".into(),
            ..SandboxOptions::default()
        });
        assert_eq!(
            check(&yard, Some(&image)).is_ok(),
            branchyard_microsandbox::ENABLED
        );
        assert!(sandboxed(Some(&image)));
        assert!(!sandboxed(Some(&Provider::Local)));
    }

    #[test]
    fn substrate_options_are_checked_before_anything_is_created() {
        let dir = tempfile::Builder::new()
            .prefix("by-placement-")
            .tempdir()
            .unwrap();
        let key = dir.path().join("key");
        branchyard_bridge::Signer::write(&key).unwrap();
        let good = SubstrateOptions {
            endpoint: "http://127.0.0.1:9".into(),
            router: "http://127.0.0.1:9/{atespace}/{actor}/".into(),
            template: "by".into(),
            key: key.clone(),
            ..SubstrateOptions::default()
        };
        assert!(check_substrate(&good.clone()).is_ok());
        assert!(sandboxed(Some(&Provider::Substrate(good.clone()))));
        // TLS anywhere, or plain HTTP to another host when asked for.
        for fine in [
            SubstrateOptions {
                endpoint: "https://control.example".into(),
                router: "wss://router.example/{atespace}/{actor}/".into(),
                ..good.clone()
            },
            SubstrateOptions {
                endpoint: "http://control.example:8080".into(),
                insecure: true,
                ..good.clone()
            },
        ] {
            assert!(check_substrate(&fine.clone()).is_ok(), "{fine:?}");
        }
        let bad = [
            SubstrateOptions {
                endpoint: "ftp://control".into(),
                ..good.clone()
            },
            SubstrateOptions {
                endpoint: "http://control.example:8080".into(),
                ..good.clone()
            },
            SubstrateOptions {
                router: "ws://router.example/{actor}/".into(),
                ..good.clone()
            },
            SubstrateOptions {
                endpoint: "https://control.example".into(),
                ca: Some(dir.path().join("missing-ca.pem")),
                ..good.clone()
            },
            SubstrateOptions {
                endpoint: "https://control.example".into(),
                client_cert: Some(key.clone()),
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
                key: dir.path().join("missing"),
                ..good.clone()
            },
        ];
        for options in bad {
            assert!(
                matches!(
                    check_substrate(&options.clone()),
                    Err(Error::Unsupported(_))
                ),
                "{options:?}"
            );
        }
    }

    /// A stand-in for the Microsandbox provider, which this build may not
    /// have: it knows some sandboxes, and records what it destroys. Clones
    /// share their state.
    #[derive(Clone)]
    struct Standin {
        known: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        destroyed: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        fail: bool,
    }

    impl SandboxProvider for Standin {
        fn capabilities(&self) -> branchyard_sandbox::Capabilities {
            branchyard_sandbox::Capabilities::default()
        }
        fn ensure(
            &self,
            _: &SandboxSpec,
        ) -> Result<branchyard_sandbox::SandboxInfo, branchyard_sandbox::ProviderError> {
            unreachable!("recovery creates nothing")
        }
        fn inspect(
            &self,
            name: &str,
        ) -> Result<Option<branchyard_sandbox::SandboxInfo>, branchyard_sandbox::ProviderError>
        {
            let known = self.known.lock().unwrap().iter().any(|n| n == name);
            Ok(known.then(|| branchyard_sandbox::SandboxInfo {
                name: name.into(),
                state: branchyard_sandbox::SandboxState::Running,
            }))
        }
        fn exec(
            &self,
            _: &str,
            _: &branchyard_sandbox::ExecSpec,
        ) -> Result<Box<dyn branchyard_sandbox::Process>, branchyard_sandbox::ProviderError>
        {
            unreachable!("recovery runs nothing")
        }
        fn stop(&self, _: &str) -> Result<(), branchyard_sandbox::ProviderError> {
            unreachable!("recovery destroys")
        }
        fn destroy(&self, name: &str) -> Result<(), branchyard_sandbox::ProviderError> {
            if self.fail {
                return Err(branchyard_sandbox::ProviderError::Runtime(
                    "the VM would not stop".into(),
                ));
            }
            self.known.lock().unwrap().retain(|n| n != name);
            self.destroyed.lock().unwrap().push(name.into());
            Ok(())
        }
    }

    #[test]
    fn a_microsandbox_sandbox_left_by_a_stopped_engine_is_destroyed_through_its_provider() {
        let standin = |fail| Standin {
            known: std::sync::Arc::new(std::sync::Mutex::new(vec!["by-x-1".into()])),
            destroyed: Default::default(),
            fail,
        };
        let boxed =
            |p: &Standin| -> Result<Arc<dyn SandboxProvider>, String> { Ok(Arc::new(p.clone())) };
        let provider = standin(false);
        assert_eq!(
            destroy_orphan(boxed(&provider), "by-x-1"),
            "destroyed its Microsandbox sandbox by-x-1"
        );
        assert_eq!(*provider.destroyed.lock().unwrap(), ["by-x-1"]);
        assert_eq!(
            destroy_orphan(boxed(&provider), "by-x-1"),
            "its Microsandbox sandbox by-x-1 was already gone"
        );
        assert_eq!(
            destroy_orphan(boxed(&standin(true)), "by-x-1"),
            "could not destroy its Microsandbox sandbox by-x-1: sandbox runtime: the VM would \
             not stop"
        );
        if !branchyard_microsandbox::ENABLED {
            assert_eq!(
                destroy_orphan(sdk(false), "by-x-1"),
                "could not destroy its Microsandbox sandbox by-x-1: this build has no \
                 Microsandbox support"
            );
        }
    }

    /// Recovery leaves a sandbox its turn had parked (its record is how the
    /// next turn finds it), and destroys one it had not, removing any kept
    /// record of it.
    #[test]
    fn recovery_keeps_a_parked_sandbox_and_destroys_an_unparked_one() {
        use crate::state::{SandboxKind, SandboxRow};
        let dir = tempfile::Builder::new()
            .prefix("by-placement-")
            .tempdir()
            .unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        let yard = Yard::open(dir.path()).unwrap();
        let standin = Standin {
            known: std::sync::Arc::new(std::sync::Mutex::new(vec!["by-b-1".into()])),
            destroyed: Default::default(),
            fail: false,
        };
        yard.use_sandbox_provider(Arc::new(standin.clone()));
        let record: Record = serde_json::from_value(json!({
            "info": {
                "name": "b", "git_branch": "by/b", "worktree": "/w",
                "prompt": "p", "harness": "h", "profile": "p", "session": null,
                "parent": null, "base": "b", "candidate": null,
                "status": {"state": "running"}, "turns": 0, "cost_usd": null,
                "created_at": 0
            },
            "created_ms": 0, "check": null, "command": null, "home": null,
            "cost_baseline": null,
            "provider": {"kind": "microsandbox", "image": "i", "keep": "pause"}
        }))
        .unwrap();
        let intent = json!({ "provider": "microsandbox", "sandbox": "by-b-1" });
        let kept = json!({ "kept": true });
        assert_eq!(
            recover(&yard, &record, &intent, Some(&kept)).as_deref(),
            Some("its sandbox by-b-1 was already kept paused for the next turn")
        );
        assert!(standin.destroyed.lock().unwrap().is_empty());
        yard.store()
            .sandboxes()
            .put_sandbox(&SandboxRow {
                branch: "b".into(),
                incarnation: 1,
                kind: SandboxKind::Kept,
                provider: "microsandbox".into(),
                name: "by-b-1".into(),
                turn: None,
                detail: "{}".into(),
                used_ms: 1,
            })
            .unwrap();
        assert_eq!(
            recover(&yard, &record, &intent, None).as_deref(),
            Some("destroyed its Microsandbox sandbox by-b-1")
        );
        assert_eq!(*standin.destroyed.lock().unwrap(), ["by-b-1"]);
        assert!(yard.store().sandboxes().sandboxes("b").unwrap().is_empty());
    }
}
