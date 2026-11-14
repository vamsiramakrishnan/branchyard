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
use std::time::Instant;

use branchyard_harness::{Driver, Open};
use branchyard_runtime::{RuntimeError, Session};
use branchyard_sandbox::{SandboxProvider, SandboxSpec};
use branchyard_substrate::transfer::{self, Guest, Pushed};
use branchyard_substrate::SubstrateProvider;
use serde_json::{json, Value};

use crate::providers::microsandbox;
use crate::providers::recipe::recipe_provider;
use crate::providers::substrate::substrate_signed;
use crate::providers::{self, ProviderKind};
use crate::snapshots::{self, Placed, SandboxEvent, SandboxOrigin, Transfer};
use crate::state::{Begun, Fence, Record, SandboxKind};
use crate::{
    harness, Activity, Error, Provider, RecipeOptions, SandboxOptions, SubstrateOptions, Yard,
};

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

/// Where this provider can run the turn's credential proxy.
pub(crate) fn credential_site(provider: Option<&Provider>) -> crate::connectors::CredentialSite {
    providers::of(provider).credential_site()
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
    /// An interactive helper may resume only this branch's existing allocation.
    Retained,
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

/// Remove the gateway token file from the copied home `guest_home` in a
/// sandbox, so a paused sandbox keeps no token (`docs/tools-model.md`,
/// step 7). No file is not an error.
fn scrub_token(endpoint: &dyn Guest, guest_home: &Path) -> Result<(), String> {
    let spec = branchyard_sandbox::ExecSpec {
        argv: vec![
            "rm".into(),
            "-f".into(),
            guest_home
                .join(crate::connectors::TOKEN_FILE)
                .display()
                .to_string(),
        ],
        cwd: PathBuf::from("/"),
        env: BTreeMap::new(),
        writable: None,
    };
    let mut process = endpoint.exec(&spec).map_err(|e| e.to_string())?;
    let status = process.wait().map_err(|e| e.to_string())?;
    match status.success() {
        true => Ok(()),
        false => Err(format!("rm ended {status}")),
    }
}

/// Whether the turn about to run wakes a sandbox its branch kept paused
/// since its last turn (`keep = "pause"`): the branch holds a kept sandbox
/// of this run's incarnation. Such a turn resolves its manifest again
/// ([`crate::manifest::settle`]).
pub(crate) fn wakes_kept_sandbox(yard: &Yard, record: &Record, fence: &Fence) -> bool {
    yard.store()
        .sandboxes()
        .sandboxes(&record.info.name)
        .map(|rows| {
            rows.iter()
                .any(|row| row.kind == SandboxKind::Kept && row.incarnation == fence.incarnation)
        })
        // When the store cannot say, resolving again is the safe answer.
        .unwrap_or(true)
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
    /// Where a local harness may write, held by Landlock; `None`: where
    /// its user may. See [`crate::guard`].
    writable: Option<Vec<PathBuf>>,
    /// The sandbox's record in the repository's service registry while the
    /// turn holds it: what a reaper recovers and destroys if this process
    /// stops. Deregistered once the sandbox is released or discarded.
    service: Option<crate::services::Registration>,
    /// The variables that were not set because their value holds a guarded
    /// token, each with the backend whose token it is
    /// ([`Placement::set_env`]): for the branch's log.
    withheld: Vec<(String, String)>,
}

/// Register the turn's sandbox `name` in the repository's service
/// registry, owned by this process for `record`'s branch, with what
/// reclaims it: the branch's recovery, then its provider's destroy.
fn register_sandbox(
    yard: &Yard,
    record: &Record,
    name: &str,
) -> Option<crate::services::Registration> {
    yard.register_service(
        sandbox_service(yard, record, name)?,
        crate::services::DEFAULT_TTL,
    )
    .ok()
}

fn sandbox_service(yard: &Yard, record: &Record, name: &str) -> Option<crate::services::Service> {
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
    Some(service)
}

// Until a complete placement takes ownership, every exit must preserve the
// native reclaim intent unless allocation cleanup was positively successful.
struct NativeRegistration(Option<crate::services::Registration>);

impl NativeRegistration {
    fn new(yard: &Yard, record: &Record, name: &str) -> Result<Self, String> {
        let service = sandbox_service(yard, record, name)
            .ok_or("a native allocation has no sandbox provider")?;
        yard.register_service(service, crate::services::DEFAULT_TTL)
            .map(|registration| Self(Some(registration)))
            .map_err(|error| format!("could not register native allocation ownership: {error}"))
    }

    fn cleaned(&mut self) {
        self.0 = None;
    }
    fn take(&mut self) -> Option<crate::services::Registration> {
        self.0.take()
    }
}

impl Drop for NativeRegistration {
    fn drop(&mut self) {
        if let Some(registration) = self.0.take() {
            registration.abandon();
        }
    }
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
    Exec {
        provider: Arc<dyn SandboxProvider>,
        kind: ExecKind,
    },
}

enum ExecKind {
    Recipe,
    PerTurn(&'static str),
}

impl Remote {
    fn provider(&self) -> &dyn SandboxProvider {
        match self {
            Remote::Substrate(provider) => provider.as_ref(),
            Remote::Exec { provider, .. } => provider.as_ref(),
        }
    }

    /// `actor` or `machine`, for messages.
    fn noun(&self) -> &'static str {
        match self {
            Remote::Substrate(_) => "actor",
            Remote::Exec {
                kind: ExecKind::Recipe,
                ..
            } => "machine",
            Remote::Exec {
                kind: ExecKind::PerTurn(_),
                ..
            } => "sandbox",
        }
    }

    /// The provider's name in events: `substrate` or `recipe`.
    fn kind(&self) -> &'static str {
        match self {
            Remote::Substrate(_) => "substrate",
            Remote::Exec {
                kind: ExecKind::Recipe,
                ..
            } => "recipe",
            Remote::Exec {
                kind: ExecKind::PerTurn(kind),
                ..
            } => kind,
        }
    }

    /// How the transfer reaches sandbox `name`.
    fn guest<'a>(&'a self, name: &'a str) -> Result<Box<dyn Guest + 'a>, String> {
        match self {
            Remote::Substrate(provider) => provider
                .endpoint(name)
                .map(|endpoint| Box::new(endpoint) as Box<dyn Guest>)
                .map_err(|e| e.to_string()),
            Remote::Exec { provider, .. } => {
                Ok(Box::new(transfer::Exec::new(provider.as_ref(), name)))
            }
        }
    }

    fn destroy_only(&self) -> bool {
        matches!(
            self,
            Remote::Exec {
                kind: ExecKind::PerTurn(_),
                ..
            }
        )
    }
}

/// How [`Placement::copied`] makes and reaches its sandbox.
struct Copied<'a> {
    remote: Remote,
    /// A fresh sandbox's spec.
    spec: SandboxSpec,
    /// The `sandbox` step's intent for a sandbox's name.
    intent: Box<dyn Fn(&str) -> Value + 'a>,
    workdir: String,
    home: String,
    pass_env: &'a [String],
    /// Where the guest also sees the tool tree (`ProviderKind::tools_root`):
    /// the engine copies the tree there after the home and sets
    /// `BRANCHYARD_TOOLS` to it.
    tools: Option<&'static str>,
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
    released: ReleaseState,
    transfer_error: Option<String>,
    capture: Option<crate::contract::capture::CopiedCapture>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReleaseState {
    Owned,
    Released,
    Reclaim,
}

impl Actor {
    /// Bring the worktree and home back. Returns what went wrong, if
    /// anything, and the home's copy when it came back.
    fn bring_back(&mut self) -> (Vec<String>, Option<Transfer>) {
        let mut warnings = Vec::new();
        let mut home_pull = None;
        let noun = self.provider.noun();
        match self.provider.guest(&self.name) {
            Ok(endpoint) => {
                if let Some(pushed) = &self.pushed {
                    if let Err(error) = transfer::pull(endpoint.as_ref(), pushed, &self.worktree) {
                        self.transfer_error = Some(error.to_string());
                        warnings.push(format!(
                            "could not bring the worktree back from {noun} {}: {error}",
                            self.name
                        ));
                    }
                    if let Some(capture) = &self.capture {
                        if let Err(error) = capture.receive(endpoint.as_ref(), &pushed.guest) {
                            warnings
                                .push(format!("could not stage declared artifact outbox: {error}"));
                        }
                    }
                }
                if let Some((host, guest)) = &self.home {
                    let pulling = Instant::now();
                    match transfer::pull_tree(endpoint.as_ref(), guest, host) {
                        Ok(bytes) => {
                            home_pull = Some(Transfer {
                                bytes,
                                ms: ms_since(pulling),
                            });
                        }
                        Err(error) => warnings.push(format!(
                            "could not bring the home directory back from {noun} {}: {error}",
                            self.name
                        )),
                    }
                    // The actor is about to be paused or destroyed: the
                    // token it was copied in with does not stay in it.
                    if let Err(error) = scrub_token(endpoint.as_ref(), guest) {
                        warnings.push(format!(
                            "could not remove the gateway token from {noun} {}: {error}",
                            self.name
                        ));
                    }
                }
            }
            Err(error) if self.pushed.is_some() => {
                self.transfer_error = Some(error.clone());
                warnings.push(format!(
                    "could not bring anything back from {noun} {}: {error}",
                    self.name
                ));
            }
            Err(_) => {}
        }
        self.pushed = None;
        (warnings, home_pull)
    }

    /// Bring the worktree and home back, then delete the actor. Returns
    /// what went wrong, if anything; the actor is deleted regardless.
    fn release(&mut self) -> Vec<String> {
        if self.released != ReleaseState::Owned {
            return Vec::new();
        }
        self.released = ReleaseState::Released;
        let (mut warnings, _) = self.bring_back();
        if let Some(error) = self.destroy() {
            warnings.push(error);
        }
        warnings
    }

    fn destroy(&mut self) -> Option<String> {
        if let Err(error) = self.provider.provider().destroy(&self.name) {
            if self.provider.destroy_only() {
                self.released = ReleaseState::Reclaim;
            }
            return Some(match &self.provider {
                Remote::Substrate(_) => format!("could not delete actor {}: {error}", self.name),
                Remote::Exec { .. } => format!(
                    "could not destroy {} {}: {error}",
                    self.provider.noun(),
                    self.name
                ),
            });
        }
        None
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
    /// Stop owning cleanup after a workspace lease was lost. Recovery owns it.
    pub(crate) fn detach_workspace(&mut self) {
        match &mut self.kind {
            Kind::Local(_) => {}
            Kind::Sandbox { released, .. } => *released = true,
            Kind::Substrate(actor) => {
                actor.released = if actor.provider.destroy_only() {
                    ReleaseState::Reclaim
                } else {
                    ReleaseState::Released
                }
            }
        }
        self.finish_native_registration();
        self.service = None;
    }

    /// Execute an operator-controlled workspace helper through this placement.
    pub(crate) fn exec_workspace(
        &self,
        argv: Vec<String>,
        env: BTreeMap<OsString, OsString>,
    ) -> Result<Box<dyn branchyard_sandbox::Process>, String> {
        let spec = branchyard_sandbox::ExecSpec {
            argv,
            cwd: self.cwd().into(),
            env,
            writable: self.writable.clone(),
        };
        match &self.kind {
            Kind::Local(_) => branchyard_runtime::LocalProvider::spawn(&spec)
                .map(|process| Box::new(process) as Box<dyn branchyard_sandbox::Process>)
                .map_err(|error| error.to_string()),
            Kind::Sandbox { provider, name, .. } => provider
                .exec(name, &spec)
                .map_err(|error| error.to_string()),
            Kind::Substrate(actor) => actor
                .provider
                .provider()
                .exec(&actor.name, &spec)
                .map_err(|error| error.to_string()),
        }
    }

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
            withheld: Vec::new(),
            writable: None,
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
        let (spec, mut env) = microsandbox::spec(yard, record, options)?;
        // A passed variable that holds a control plane's token is not the
        // sandbox's, on any turn.
        let withheld = take_guarded(&mut env);
        let provider =
            branchyard_sandbox::traced("microsandbox", microsandbox::microsandbox(yard, options)?);
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
            SandboxPlan::Default | SandboxPlan::Retained => {
                let store = yard.store();
                let environment = environment(yard, record, &key);
                let acquire = if matches!(plan, SandboxPlan::Retained) {
                    snapshots::acquire_retained
                } else {
                    snapshots::acquire
                };
                acquire(
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
                placed: None,
            }),
            egress: None,
            withheld,
            writable: None,
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
                spec: SandboxSpec::new(actor_name(&record.info.name)?),
                // Journaled before the actor exists, so recovery can delete it.
                intent: Box::new(
                    move |name| json!({ "provider": "substrate", "actor": name, "atespace": atespace }),
                ),
                workdir: options.workdir().to_owned(),
                home: options.home().to_owned(),
                pass_env: &options.pass_env,
                tools: None,
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
                remote: Remote::Exec {
                    provider: branchyard_sandbox::traced("recipe", recipe_provider(yard, options)),
                    kind: ExecKind::Recipe,
                },
                spec: SandboxSpec::new(sandbox_name(&record.info.name)?),
                // Journaled before `create` runs, so recovery can destroy it.
                intent: Box::new(
                    move |name| json!({ "provider": "recipe", "sandbox": name, "recipe": recipe }),
                ),
                workdir,
                home,
                pass_env: &options.pass_env,
                tools: None,
            },
        )
    }

    pub(crate) fn exec_copied(
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        plan: &SandboxPlan,
        provider: Arc<dyn SandboxProvider>,
        spec: SandboxSpec,
        pass_env: &[String],
    ) -> Result<Placement, String> {
        let kind = providers::of(record.provider.as_ref()).name();
        Placement::copied(
            yard,
            record,
            fence,
            plan,
            Copied {
                remote: Remote::Exec {
                    provider,
                    kind: ExecKind::PerTurn(kind),
                },
                spec,
                intent: Box::new(move |name| json!({"provider":kind,"sandbox":name})),
                workdir: WORKSPACE.into(),
                home: HOME.into(),
                pass_env,
                tools: providers::of(record.provider.as_ref()).tools_root(),
            },
        )
    }

    /// A sandbox the worktree and home are copied into, and back from when
    /// the turn ends. Its `started` event records how long it took to be
    /// ready and what the copies in sent ([`Placed`]).
    fn copied(
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        plan: &SandboxPlan,
        how: Copied<'_>,
    ) -> Result<Placement, String> {
        // Copy inputs before the initial guest upload. Sandbox setup happens
        // afterward, and its local copy alone cannot reach a copied worktree.
        if let Some(workspace) = record.workspace.as_ref().filter(|state| !state.ready) {
            let copied = crate::workspace::copy(
                yard.root(),
                &record.info.worktree,
                &workspace.spec.copy,
                None,
            );
            if !copied.ok {
                return Err(copied.failure());
            }
        }
        // The sandbox's readiness, not this host's copying of inputs.
        let began = Instant::now();
        let home = record
            .home
            .clone()
            .ok_or("a sandboxed branch has no private home")?;
        let mut env = sandbox_env(&how.home, how.pass_env)?;
        let withheld = take_guarded(&mut env);
        if let Some(root) = how.tools {
            env.insert(crate::tooltree::ENV_TOOLS.into(), root.into());
        }
        let (remote, spec) = (how.remote, how.spec);
        let fresh =
            SandboxPlan::Fresh("native providers allocate a fresh sandbox for every turn".into());
        let plan =
            if remote.destroy_only() {
                match plan {
                    SandboxPlan::Default => &fresh,
                    SandboxPlan::Fresh(_) => plan,
                    SandboxPlan::Retained | SandboxPlan::Handed { .. } => return Err(
                        "native per-turn providers cannot resume or branch retained allocations"
                            .into(),
                    ),
                }
            } else {
                plan
            };
        let noun = remote.noun();
        let key = providers::of(record.provider.as_ref()).key();
        let mut native_registration = if remote.destroy_only() {
            Some(NativeRegistration::new(yard, record, &spec.name)?)
        } else {
            None
        };
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
                        Remote::Exec { .. } => {
                            format!("could not create {noun} {}: {e}", spec.name)
                        }
                    })
            }),
            SandboxPlan::Default | SandboxPlan::Retained => {
                let environment = environment(yard, record, &key);
                let acquire = if matches!(plan, SandboxPlan::Retained) {
                    snapshots::acquire_retained
                } else {
                    snapshots::acquire
                };
                acquire(
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
        let ready_ms = ms_since(began);
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
            Remote::Exec { .. } => json!({ "created": acquired.is_ok(), "sandbox": name }),
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
            released: ReleaseState::Owned,
            transfer_error: None,
            capture: crate::contract::capture::CopiedCapture::for_record(yard, record, fence),
        });
        let fail = |actor: &mut Actor,
                    registration: &mut Option<NativeRegistration>,
                    why: String|
         -> Result<Placement, String> {
            let warnings = actor.release();
            if actor.released == ReleaseState::Released {
                if let Some(registration) = registration.as_mut() {
                    registration.cleaned();
                }
            }
            match warnings.is_empty() {
                true => Err(why),
                false => Err(format!("{why}; {}", warnings.join("; "))),
            }
        };
        let acquired = match acquired {
            Ok(acquired) => acquired,
            Err(error) => return fail(&mut actor, &mut native_registration, error),
        };
        let service = if actor.provider.destroy_only() {
            None
        } else {
            register_sandbox(yard, record, &name)
        };
        let workdir = PathBuf::from(&how.workdir);
        let guest_home = PathBuf::from(&how.home);
        let pushed = (|| -> Result<(Pushed, Transfer), String> {
            let endpoint = actor.provider.guest(&name)?;
            if let Some(capture) = &actor.capture {
                capture
                    .probe(endpoint.as_ref(), &workdir)
                    .map_err(|error| error.to_string())?;
            }
            // A resumed or branched sandbox still holds a worktree and a
            // home from before: the worktree's files go (what git ignores,
            // such as what setup installed, stays), and so does the home,
            // before this host's are sent.
            let cleared = match (&acquired.origin, &actor.provider) {
                (
                    SandboxOrigin::Fresh { .. },
                    Remote::Substrate(_)
                    | Remote::Exec {
                        kind: ExecKind::PerTurn(_),
                        ..
                    },
                ) => Ok(()),
                // A recipe may hand out a machine that outlives its
                // sandboxes (a lab box): an earlier push there goes too.
                (
                    SandboxOrigin::Fresh { .. },
                    Remote::Exec {
                        kind: ExecKind::Recipe,
                        ..
                    },
                ) => transfer::clear_previous_push(endpoint.as_ref(), &workdir),
                _ => transfer::clear_for_push(endpoint.as_ref(), &workdir, &guest_home),
            };
            cleared.map_err(|e| format!("could not clear {noun} {name} for this turn: {e}"))?;
            let stage = staging(yard, &name);
            let pushing = Instant::now();
            let (pushed, bytes) = transfer::push_staged(
                endpoint.as_ref(),
                &record.info.worktree,
                &workdir,
                Some(&stage),
            )
            .map_err(|e| format!("could not copy the worktree into {noun} {name}: {e}"))?;
            let sent = Transfer {
                bytes,
                ms: ms_since(pushing),
            };
            Ok((pushed, sent))
        })();
        let worktree_push = match pushed {
            Ok((pushed, sent)) => {
                actor.pushed = Some(pushed);
                sent
            }
            Err(error) => return fail(&mut actor, &mut native_registration, error),
        };
        let home_sent = actor
            .provider
            .guest(&name)
            .and_then(|endpoint| {
                let pushing = Instant::now();
                let bytes = transfer::push_tree(endpoint.as_ref(), &home, &guest_home)
                    .map_err(|e| e.to_string())?;
                Ok(Transfer {
                    bytes,
                    ms: ms_since(pushing),
                })
            })
            .map_err(|e| format!("could not copy the home directory into {noun} {name}: {e}"));
        let home_push = match home_sent {
            Ok(sent) => sent,
            Err(error) => return fail(&mut actor, &mut native_registration, error),
        };
        // The tool tree is also at the guest root (`/tools`): it was written into
        // the home, so a copy of it goes to its own volume once the home is in.
        let tree = home.join(crate::tooltree::DIRECTORY);
        if let (Some(root), true) = (how.tools, tree.is_dir()) {
            let copied = actor
                .provider
                .guest(&name)
                .and_then(|endpoint| {
                    transfer::push_tree(endpoint.as_ref(), &tree, Path::new(root))
                        .map_err(|e| e.to_string())
                })
                .map_err(|e| format!("could not copy the tool tree into {noun} {name}: {e}"));
            if let Err(error) = copied {
                return fail(&mut actor, &mut native_registration, error);
            }
        }
        actor.home = Some((home, guest_home));
        Ok(Placement {
            cwd: how.workdir,
            started: Some(SandboxEvent::Started {
                provider: actor.provider.kind().into(),
                sandbox: name,
                origin: acquired.origin,
                placed: Some(Placed {
                    ready_ms,
                    worktree_push,
                    home_push,
                }),
            }),
            egress: None,
            withheld,
            writable: None,
            service: native_registration
                .as_mut()
                .and_then(NativeRegistration::take)
                .or(service),
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

    /// Whether the harness's environment would let a variable named
    /// `name` through from this process's: not stripped (an isolated
    /// harness's credential prefixes) nor removed. A sandbox's environment
    /// is its own, so nothing from this host is admitted there.
    pub fn admits(&self, name: &str) -> bool {
        match &self.kind {
            Kind::Local(env) => env.admits(name),
            Kind::Sandbox { .. } | Kind::Substrate(_) => false,
        }
    }

    /// Add a variable to the harness's environment. A value that holds a
    /// control plane's token is not set, whoever the writer is (the
    /// workspace's table, a secret, a snippet's export): it is noted for
    /// the branch's log ([`Placement::take_withheld`]) and the turn runs.
    pub fn set_env(&mut self, name: &str, value: &str) {
        if name.eq_ignore_ascii_case(branchyard_support::MANAGER_TOKEN_ENV) {
            return;
        }
        if let Some(backend) = branchyard_support::child::guarded(value) {
            if !self.withheld.iter().any(|(held, _)| held == name) {
                self.withheld.push((name.to_owned(), backend));
            }
            return;
        }
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

    /// The variables [`Placement::set_env`] and the sandbox's passed
    /// variables left out since the last call, each with the backend whose
    /// token its value holds.
    pub fn take_withheld(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.withheld)
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

    /// The `HOME` a local harness runs with, as its environment resolves
    /// it now (empty when unset); `None` for a harness in a sandbox.
    pub fn local_home(&self) -> Option<PathBuf> {
        match &self.kind {
            Kind::Local(env) => Some(
                env.resolve()
                    .get(std::ffi::OsStr::new("HOME"))
                    .map(PathBuf::from)
                    .unwrap_or_default(),
            ),
            Kind::Sandbox { .. } | Kind::Substrate(_) => None,
        }
    }

    /// Hold a local harness to writing beneath `writable` when
    /// [`Placement::start`] starts it ([`crate::guard`]). Nothing for a
    /// harness in a sandbox.
    pub fn guard(&mut self, writable: Vec<PathBuf>) {
        if let Kind::Local(_) = self.kind {
            self.writable = Some(writable);
        }
    }

    pub fn start(&self, driver: Box<dyn Driver>, open: Open) -> Result<Session, RuntimeError> {
        match &self.kind {
            Kind::Local(env) => {
                let egress = self.egress.as_ref().filter(|e| e.confined());
                let confinement = branchyard_runtime::Confinement {
                    egress_port: egress.map(|_| crate::egress::CONFINED_PORT),
                    writable: self.writable.clone(),
                };
                let (session, listener) =
                    Session::start_local(driver, open, env, None, &confinement)?;
                if let (Some(egress), Some(listener)) = (egress, listener) {
                    egress.serve(listener).map_err(|source| RuntimeError::Io {
                        context: "the egress proxy",
                        source,
                    })?;
                }
                Ok(session)
            }
            Kind::Sandbox {
                provider,
                name,
                env,
                ..
            } => Session::start_in(
                driver,
                open,
                provider.as_ref(),
                name,
                without_guarded(env),
                None,
            ),
            Kind::Substrate(actor) => Session::start_in(
                driver,
                open,
                actor.provider.provider(),
                &actor.name,
                without_guarded(&actor.env),
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
        self.finish_native_registration();
        // Destroyed, or kept for the next turn in the sandbox store: no
        // longer this turn's to reclaim.
        self.service = None;
        said
    }

    /// Whether copied work failed to return; cleanup warnings alone do not fail capture.
    pub fn transfer_error(&self) -> Option<&str> {
        match &self.kind {
            Kind::Substrate(actor) => actor.transfer_error.as_deref(),
            _ => None,
        }
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
                if actor.released != ReleaseState::Owned {
                    return Vec::new();
                }
                actor.released = ReleaseState::Released;
                let (warnings, home_pull) = actor.bring_back();
                let mut said: Vec<Activity> = warnings.into_iter().map(Activity::Warning).collect();
                if let Some(home_pull) = home_pull {
                    said.push(snapshots::event(SandboxEvent::Returned {
                        provider: actor.provider.kind().into(),
                        sandbox: actor.name.clone(),
                        home_pull,
                    }));
                }
                if actor.provider.destroy_only() {
                    if let Some(error) = actor.destroy() {
                        said.push(Activity::Warning(error));
                    }
                } else {
                    said.extend(snapshots::park(
                        yard,
                        record,
                        fence,
                        actor.provider.provider(),
                        &actor.name,
                    ));
                }
                said
            }
        }
    }

    /// Destroy the sandbox without keeping it; for a Substrate actor, bring
    /// the worktree and home back first. A failure is returned as a warning.
    pub fn discard(&mut self) -> Option<String> {
        let warning = match &mut self.kind {
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
        };
        self.finish_native_registration();
        warning
    }

    fn finish_native_registration(&mut self) {
        if let Kind::Substrate(actor) = &self.kind {
            if actor.provider.destroy_only() {
                if let Some(registration) = self.service.take() {
                    if actor.released == ReleaseState::Reclaim {
                        registration.abandon();
                    }
                }
            }
        }
    }
}

impl Drop for Placement {
    fn drop(&mut self) {
        self.discard();
    }
}

/// Milliseconds since `began`, on the monotonic clock.
fn ms_since(began: Instant) -> u64 {
    u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// `HOME` and the passed variables, each of which must be set here.
pub(crate) fn sandbox_env(
    home: &str,
    pass_env: &[String],
) -> Result<BTreeMap<OsString, OsString>, String> {
    let mut env = BTreeMap::from([(OsString::from("HOME"), OsString::from(home))]);
    for name in pass_env {
        if name.eq_ignore_ascii_case(branchyard_support::MANAGER_TOKEN_ENV) {
            return Err("the operator manager token cannot be passed to a sandbox".into());
        }
        let value = std::env::var_os(name)
            .ok_or_else(|| format!("{name} is to be passed to the sandbox but is not set"))?;
        env.insert(name.into(), value);
    }
    Ok(env)
}

/// Take out of `env` the variables whose value holds a control plane's
/// token (`branchyard_support::child::guard`), and name them, each with the
/// backend whose token it is.
fn take_guarded(env: &mut BTreeMap<OsString, OsString>) -> Vec<(String, String)> {
    let mut withheld = Vec::new();
    env.retain(
        |name, value| match branchyard_support::child::guarded(&value.to_string_lossy()) {
            Some(backend) => {
                withheld.push((name.to_string_lossy().into_owned(), backend));
                false
            }
            None => true,
        },
    );
    withheld
}

/// `env` as a sandbox is started with it: without a variable whose value
/// holds a control plane's token, whoever set it.
fn without_guarded(env: &BTreeMap<OsString, OsString>) -> BTreeMap<OsString, OsString> {
    let mut env = env.clone();
    take_guarded(&mut env);
    env
}

/// `by-<branch>-<ulid>`, unique across repositories and concurrent allocations,
/// within the runtime's 128-byte limit. Branch names are already `[a-z0-9._-]`.
pub(crate) fn sandbox_name(branch: &str) -> Result<String, String> {
    let id = branchyard_support::new_ulid().map_err(|error| error.to_string())?;
    let suffix = format!("-{}", id.to_ascii_lowercase());
    let room = 128 - "by-".len() - suffix.len();
    let branch: String = branch.chars().take(room).collect();
    Ok(format!("by-{branch}{suffix}"))
}

/// A unique `by-<branch>-<ulid>` DNS label of at most 63 characters;
/// `.` and `_` become `-` and the branch is shortened.
fn actor_name(branch: &str) -> Result<String, String> {
    let id = branchyard_support::new_ulid().map_err(|error| error.to_string())?;
    let suffix = format!("-{}", id.to_ascii_lowercase());
    let room = 63 - "by-".len() - suffix.len();
    let branch: String = branch
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' => c,
            _ => '-',
        })
        .take(room)
        .collect();
    Ok(format!("by-{}{suffix}", branch.trim_end_matches('-')))
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
    fn a_variable_that_holds_a_guarded_token_is_not_a_sandboxes() {
        // What `--pass-env` copied in, whatever it is named: taken out and
        // named, with the backend, before the sandbox is given anything.
        const TOKEN: &str = "placement-test-token-0123456789";
        branchyard_support::child::guard("ap", TOKEN);
        let mut env: BTreeMap<OsString, OsString> = [
            ("HOME", "/home/agent".to_owned()),
            ("GEMINI_API_KEY", TOKEN.to_owned()),
            ("AUTH_HEADER", format!("Bearer {TOKEN}")),
            ("OTHER_KEY", "another-value-0123456789".to_owned()),
        ]
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect();
        let started_with = without_guarded(&env);
        assert_eq!(
            take_guarded(&mut env),
            [
                ("AUTH_HEADER".to_owned(), "ap".to_owned()),
                ("GEMINI_API_KEY".to_owned(), "ap".to_owned())
            ]
        );
        let names: Vec<&OsString> = env.keys().collect();
        assert_eq!(names, ["HOME", "OTHER_KEY"]);
        assert_eq!(started_with, env);
        // And a local harness's environment, the same, whoever set it.
        let mut placement = Placement {
            cwd: "/".into(),
            kind: Kind::Local(harness::environment(None)),
            started: None,
            egress: None,
            withheld: Vec::new(),
            writable: None,
            service: None,
        };
        placement.set_env("MY_TOKEN", TOKEN);
        placement.set_env("MY_HEADER", &format!("Bearer {TOKEN}"));
        placement.set_env("MY_OTHER", "another-value-0123456789");
        assert_eq!(
            placement.take_withheld(),
            [
                ("MY_TOKEN".to_owned(), "ap".to_owned()),
                ("MY_HEADER".to_owned(), "ap".to_owned())
            ]
        );
        assert!(placement.take_withheld().is_empty());
        let Kind::Local(env) = &placement.kind else {
            unreachable!("a local placement")
        };
        let resolved = env.resolve();
        assert_eq!(
            resolved.get(std::ffi::OsStr::new("MY_OTHER")),
            Some(&OsString::from("another-value-0123456789"))
        );
        assert!(resolved
            .values()
            .all(|value| !value.to_string_lossy().contains(TOKEN)));
    }

    #[test]
    fn sandbox_pass_through_refuses_operator_manager_credentials_before_reading_them() {
        for name in [
            branchyard_support::MANAGER_TOKEN_ENV,
            "branchyard_manager_token",
        ] {
            let error = sandbox_env("/home/agent", &[name.into()]).unwrap_err();
            assert_eq!(
                error,
                "the operator manager token cannot be passed to a sandbox"
            );
        }
    }

    #[test]
    fn sandbox_names_fit_the_runtime() {
        let short = sandbox_name("fix-parser").unwrap();
        assert!(short.starts_with("by-fix-parser-"));
        assert_eq!(short.len(), "by-fix-parser-".len() + 26);
        let long = sandbox_name(&"a".repeat(200)).unwrap();
        assert_eq!(long.len(), 128);
        assert!(branchyard_microsandbox::plan::name(&long).is_ok());
    }

    #[test]
    fn actor_names_are_dns_labels() {
        let short = actor_name("fix.parser_2").unwrap();
        assert!(short.starts_with("by-fix-parser-2-"));
        assert_eq!(short.len(), "by-fix-parser-2-".len() + 26);
        let long = actor_name(&"a".repeat(200)).unwrap();
        assert_eq!(long.len(), 63);
        assert!(long
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'));
        let dashed = actor_name(&format!("{}.x", "a".repeat(32))).unwrap();
        assert!(!dashed.contains("--"), "{dashed}");
    }

    #[test]
    fn same_branch_allocations_always_receive_distinct_names() {
        let mut actors = std::collections::HashSet::new();
        let mut sandboxes = std::collections::HashSet::new();
        for _ in 0..256 {
            assert!(actors.insert(actor_name("agy-substrate").unwrap()));
            assert!(sandboxes.insert(sandbox_name("agy-substrate").unwrap()));
        }
    }
}
