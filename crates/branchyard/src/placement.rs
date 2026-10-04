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
//! An environment recipe's machine (the `recipe` provider) is reached over
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
use std::sync::Arc;

use branchyard_harness::{Driver, Open};
use branchyard_recipe::RecipeProvider;
use branchyard_runtime::{RuntimeError, Session};
use branchyard_sandbox::{SandboxProvider, SandboxSpec};
use branchyard_substrate::transfer::{self, Guest, Pushed};
use branchyard_substrate::SubstrateProvider;
use serde_json::{json, Value};

use crate::providers::microsandbox;
use crate::providers::recipe::recipe_provider;
use crate::providers::substrate::substrate_signed;
use crate::providers::{self, ProviderKind};
use crate::snapshots::{self, SandboxEvent, SandboxOrigin};
use crate::state::{Begun, Fence, Record, SandboxKind};
use crate::{
    harness, Activity, Error, Provider, RecipeOptions, SandboxOptions, SubstrateOptions, Yard,
};
use branchyard_support::time::now_ms;

/// Where the worktree appears in a sandbox.
pub const WORKSPACE: &str = "/workspace";
/// Where the branch's private home appears in a sandbox.
pub const HOME: &str = "/branchyard/home";
/// Where a Microsandbox turn's authorized scratch areas are mounted, one
/// subdirectory per area named for it.
pub(crate) const SCRATCH_MOUNT_BASE: &str = "/branchyard/scratch";
/// The journaled step naming a turn's sandbox, before it is created.
pub(crate) const STEP_SANDBOX: &str = "sandbox";

/// Whether this provider runs the harness somewhere other than this host.
pub(crate) fn sandboxed(provider: Option<&Provider>) -> bool {
    providers::of(provider).sandboxed()
}

/// Refuse a provider this build or its options cannot run, before anything
/// is created.
pub(crate) fn check(yard: &Yard, provider: Option<&Provider>) -> Result<(), Error> {
    providers::of(provider).check(yard)
}

/// The harness's working directory and `HOME` as the harness will see
/// them, before anything is created.
pub(crate) fn guest_paths(record: &Record) -> (String, String) {
    providers::of(record.provider.as_ref()).guest_paths(record)
}

/// A sandbox's spec and the provider it is created through.
pub(crate) type Planned = (SandboxSpec, Arc<dyn SandboxProvider>);

/// For a fan whose setup runs once: the spec of `record`'s sandbox, when
/// its placement mounts the worktree (so a sandbox branched from another
/// can be rebound to it), and its provider. `None` otherwise.
pub(crate) fn fan_spec(yard: &Yard, record: &Record) -> Result<Option<Planned>, String> {
    providers::of(record.provider.as_ref()).fan_spec(yard, record)
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
        .map_or("", snapshots::provider_name);
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
    use crate::services::{Reclaim, Service, ServiceOwner};
    let provider = record.provider.clone()?;
    let kind = provider.kind();
    if !kind.sandboxed() {
        return None;
    }
    let branch = &record.info.name;
    let service = Service::new(
        kind.service_kind(),
        ServiceOwner::this_process().for_branch(branch),
    )
    .with("provider", kind.name())
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
        providers::of(record.provider.as_ref()).prepare(yard, record, fence, plan)
    }

    /// A harness on this host.
    pub(crate) fn local(record: &Record) -> Placement {
        Placement {
            cwd: record.info.worktree.display().to_string(),
            kind: Kind::Local(harness::environment(record.home.as_deref())),
            started: None,
            egress: None,
            service: None,
        }
    }

    /// A Microsandbox microVM with the worktree and home mounted.
    pub(crate) fn microsandbox(
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        options: &SandboxOptions,
        plan: &SandboxPlan,
    ) -> Result<Placement, String> {
        let (spec, env) = microsandbox::spec(yard, record, options)?;
        let provider = microsandbox::microsandbox(yard, options)?;
        let key = options.key();
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
                    branchyard_support::best_effort(
                        "destroy the sandbox",
                        provider.destroy(&spec.name),
                    );
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
                    provider.as_ref(),
                    &journal,
                    snapshots::Wanted {
                        record,
                        fence,
                        key: &key,
                        spec: &spec,
                        environment: environment.as_ref(),
                    },
                )
                .inspect_err(|_| {
                    branchyard_support::best_effort(
                        "destroy the sandbox",
                        provider.destroy(&spec.name),
                    );
                })?
            }
        };
        let store = yard.store();
        branchyard_support::best_effort(
            "finish the journal step",
            store.backend().finish_step(
                fence,
                fence.turn,
                STEP_SANDBOX,
                &json!({ "created": true, "sandbox": acquired.name }),
            ),
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

    pub(crate) fn substrate(
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        options: &SubstrateOptions,
        plan: &SandboxPlan,
    ) -> Result<Placement, String> {
        let provider = substrate_signed(options)?;
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

    pub(crate) fn recipe(
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        options: &RecipeOptions,
        plan: &SandboxPlan,
    ) -> Result<Placement, String> {
        let (workdir, home) = options.guest_paths(record);
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
        let key = providers::of(record.provider.as_ref()).key();
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
                    remote.provider(),
                    &journal,
                    snapshots::Wanted {
                        record,
                        fence,
                        key: &key,
                        spec: &spec,
                        environment: environment.as_ref(),
                    },
                )
            }
        };
        let name = acquired
            .as_ref()
            .map_or_else(|_| spec.name.clone(), |a| a.name.clone());
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
        branchyard_support::best_effort(
            "finish the journal step",
            store
                .backend()
                .finish_step(fence, fence.turn, STEP_SANDBOX, &finished),
        );
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
pub(crate) fn sandbox_env(
    home: &str,
    pass_env: &[String],
) -> Result<BTreeMap<OsString, OsString>, String> {
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
pub(crate) fn sandbox_name(branch: &str, ms: u64) -> String {
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

/// Where a turn's transfer to `actor` is staged, so recovery can delete it.
pub(crate) fn staging(yard: &Yard, actor: &str) -> PathBuf {
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
    let said = providers::of(record.provider.as_ref()).recover(yard, record, &name)?;
    branchyard_support::best_effort(
        "take the sandbox's row",
        yard.store()
            .sandboxes()
            .take_sandbox(&record.info.name, SandboxKind::Kept, &name),
    );
    Some(said)
}

/// Pull the worktree and home a stopped engine's push to `workdir` and
/// `home` left in `stage` back through `endpoint`: whether any file
/// changed, and what to add about the home.
pub(crate) fn pull_staged(
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
pub(crate) fn said_back(pulled: Result<(bool, String), String>, noun: &str, name: &str) -> String {
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
}
