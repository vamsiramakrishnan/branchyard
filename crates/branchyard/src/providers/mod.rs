//! Where a branch's harness can run, as one trait with one implementation
//! per [`Provider`] variant.
//!
//! [`Provider`] is the closed, serialized enum a branch stores. Everything
//! the engine needs to know about a variant (its name and key, its sandbox
//! lifecycle, whether its options can run here, where the harness sees the
//! worktree and `HOME`, how a stopped engine's sandbox is recovered and a
//! reaped one destroyed, how its sandbox provider is opened) is a method of
//! [`ProviderKind`], implemented once in this directory:
//!
//! | variant | file |
//! |---|---|
//! | [`Provider::Local`] | `local.rs` |
//! | [`Provider::Microsandbox`] | `microsandbox.rs` |
//! | [`Provider::Substrate`] | `substrate.rs` |
//! | [`Provider::Recipe`] | `recipe.rs` |
//! | [`Provider::Container`] | `container.rs` |
//! | [`Provider::AgentSandbox`] | `agent_sandbox.rs` |
//!
//! [`Provider::kind`] is the only place a variant is mapped to its
//! implementation. Nothing else in the crate matches on [`Provider`]: call
//! `provider.kind().<method>()`, or [`of`] for a branch's optional provider
//! (`None` is local). The guard `tests/provider_seam.rs` fails when a match
//! on the enum appears elsewhere. See "Adding a provider" in
//! `docs/providers.md`.
//!
//! The trait is crate-private. The enum's serde shape, and so the wire
//! format and `schema/`, are the enum's own and do not depend on it.

use std::sync::Arc;

use branchyard_sandbox::SandboxProvider;

use crate::placement::{Placement, Planned, SandboxPlan};
use crate::snapshots::Lifecycle;
use crate::state::{Fence, Record};
use crate::{Error, Provider, RecipeOptions, SubstrateOptions, Yard};

mod agent_sandbox;
mod container;
pub(crate) mod local;
pub(crate) mod microsandbox;
mod native;
pub(crate) mod recipe;
pub(crate) mod substrate;

/// Where a provider expects its harness executable, for readiness checks.
///
/// This is a declaration of location, not evidence that the harness or
/// runtime is available. Borrowed options identify the exact target without
/// opening its transport or reading credentials.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HarnessLocation<'a> {
    /// The harness executable is on the worker host.
    Local,
    /// The harness executable is in a Microsandbox image.
    Microsandbox,
    /// The harness executable is in the selected Substrate actor template.
    Substrate(&'a SubstrateOptions),
    /// The harness executable is reached through this environment recipe.
    Recipe(&'a RecipeOptions),
    /// The harness executable is in a native per-turn allocation's image.
    Image,
}

/// Where a provider's turns run, as an operator plans compute for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunsOn {
    /// On the worker that claims the turn: a server or a `by worker`.
    Worker,
    /// In a Kubernetes allocation the claiming worker creates.
    Kubernetes,
    /// In an actor on a Substrate cluster.
    Substrate,
}

/// What separates a provider's turns from the worker's kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// A process as the worker's user: none.
    Process,
    /// gVisor's user-space kernel (Docker `runsc`, or GKE Sandbox).
    Gvisor,
    /// Linux namespaces sharing the worker's kernel.
    Runc,
    /// A Microsandbox KVM microVM on the worker.
    Microsandbox,
    /// Kata Containers with Cloud Hypervisor (GKE's managed MicroVM runtime).
    Kata,
    /// Whatever the Substrate template's actor runtime is.
    Substrate,
    /// Whatever the selected recipe's environment is.
    Recipe,
}

/// A provider's compute as an operator sees it: where its turns run, what
/// isolates them, and the network and namespace it was configured with.
/// It describes configuration; it is no evidence the runtime is ready.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ProviderCompute {
    /// Where its turns run.
    pub runs_on: RunsOn,
    /// What isolates them.
    pub isolation: Isolation,
    /// `deny` or `unrestricted` for the native providers; `None` otherwise.
    pub network: Option<&'static str>,
    /// The Kubernetes namespace an Agent Sandbox allocation is created in.
    pub namespace: Option<String>,
}

impl ProviderCompute {
    /// Compute with no network choice and no namespace.
    pub(crate) fn plain(runs_on: RunsOn, isolation: Isolation) -> ProviderCompute {
        ProviderCompute {
            runs_on,
            isolation,
            network: None,
            namespace: None,
        }
    }
}

/// What the engine asks of a kind of provider. One implementation per
/// [`Provider`] variant, on that variant's options (or, for a variant
/// without any, on a unit type).
pub(crate) trait ProviderKind {
    /// The variant's serialized `kind`, as recorded in events and sandbox rows.
    fn name(&self) -> &'static str;

    /// Where readiness checks look for the harness executable.
    fn harness_location(&self) -> HarnessLocation<'_>;

    /// The image the provider's sandbox is made from, as it was given (the
    /// reference, `@sha256:...` where pinned); `None` for a provider that
    /// names none (a local harness, a recipe's machine, a Substrate actor
    /// whose template fixes its image).
    fn image(&self) -> Option<String> {
        None
    }

    /// Pure option validation, where supported. Other checks belong to `check`.
    fn validate_options(&self) -> Result<(), Error> {
        Ok(())
    }

    /// How the provider is named to a person and recorded on a branch
    /// (`BranchInfo::provider`): the name, with a recipe's own. Defaults to
    /// [`ProviderKind::name`].
    fn label(&self) -> String {
        self.name().to_owned()
    }

    /// Which provider, and where, holds a sandbox or snapshot row:
    /// sandboxes and snapshots are only ever used through the same one.
    /// Defaults to [`ProviderKind::name`].
    fn key(&self) -> String {
        self.name().to_owned()
    }

    /// `by`'s banner for a run under this provider: the mode word (`local`,
    /// `isolated` or `sandbox`) and what the mode does and does not give,
    /// said as plainly as what it does. `isolated`: the branch has a private
    /// home, which only a local run minds. `server`: the harness runs on a
    /// server, as the server's user.
    fn banner(&self, isolated: bool, server: bool) -> (&'static str, String);

    /// What these options ask of the sandbox's lifecycle; `None` when
    /// there is no sandbox (a local harness).
    fn lifecycle(&self) -> Option<Lifecycle>;

    /// Whether the harness runs somewhere other than this host, in a
    /// sandbox this provider makes.
    fn sandboxed(&self) -> bool {
        true
    }

    /// Whether worktree files cross the guest transport instead of being local or mounted.
    fn copies_worktree(&self) -> bool;

    /// Whether a harness in this provider's sandbox can open a connection
    /// to this host, so that it can be served the delegation tools over
    /// HTTP (`crate::endpoint`). A sandbox that cannot runs a delegating
    /// turn without them, and the turn says so.
    fn reaches_host(&self) -> bool {
        false
    }

    /// Where this provider can run the turn's credential proxy, so that the
    /// harness holds no gateway token ([`crate::connectors::CredentialSite`]).
    /// The host, unless the provider says it has a place in the sandbox.
    fn credential_site(&self) -> crate::connectors::CredentialSite {
        crate::connectors::CredentialSite::Host
    }

    /// Whether the harness sees the yard's tool store
    /// ([`crate::toolstore`]) at the store's own path, so a turn may link
    /// its skills and connector packages there instead of copying them. A
    /// local harness does; a provider that copies the home in does not
    /// (the links would dangle), and one that mounts the store says so.
    fn sees_store(&self, _yard: &Yard) -> bool {
        !self.sandboxed()
    }

    /// Whether a turn under this provider mounts every ready skill, chosen
    /// or not (`docs/skill-registry.md`, R2): where the harness reads the
    /// tree where the engine writes it (a local harness; Microsandbox's
    /// live mount of the home) or links the shared store (an Agent Sandbox
    /// with a bucket), an unchosen skill costs a directory entry. Where the
    /// home is copied in for the turn (a container, Substrate, a recipe
    /// machine) it would cost its files every turn, so the turn stays
    /// chosen-only until a fetch exists (R5). Defaults to
    /// [`ProviderKind::sees_store`].
    fn mounts_ready_skills(&self, yard: &Yard) -> bool {
        self.sees_store(yard)
    }

    /// Where the guest also sees the tool tree (`docs/tools-model.md`), when
    /// this provider controls the guest filesystem: `/tools`, as a read-only
    /// mount of the host's tree or a copy made after the home. A guest then
    /// gets `BRANCHYARD_TOOLS` set to it. `None` keeps the tree under the
    /// home only, as for a local harness.
    fn tools_root(&self) -> Option<&'static str> {
        None
    }

    /// Whether a network policy can be enforced on this host for a
    /// harness run by this provider, rather than only for a local one.
    fn confines_egress(&self) -> bool {
        false
    }

    /// Where its turns run and what isolates them, for operators.
    fn compute(&self) -> ProviderCompute;

    /// The registry kind of a turn's sandbox in the service registry.
    fn service_kind(&self) -> &'static str {
        crate::services::KIND_SANDBOX
    }

    /// Refuse options this build cannot run, before anything is created.
    fn check(&self, yard: &Yard) -> Result<(), Error>;

    /// The harness's working directory and `HOME` as the harness will see
    /// them, before anything is created.
    fn guest_paths(&self, record: &Record) -> (String, String);

    /// Get the turn's harness location ready: for a sandbox, get it now.
    /// A failure is the turn's failure reason.
    fn prepare(
        &self,
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        plan: &SandboxPlan,
    ) -> Result<Placement, String>;

    /// For a fan whose setup runs once: the spec of `record`'s sandbox
    /// when its placement mounts the worktree, and its provider. `None`
    /// for the rest.
    fn fan_spec(&self, _yard: &Yard, _record: &Record) -> Result<Option<Planned>, String> {
        Ok(None)
    }

    /// Clean up the sandbox `sandbox` a stopped engine's turn journaled,
    /// and say what happened; `None` when there is nothing to clean up.
    fn recover(&self, yard: &Yard, record: &Record, sandbox: &str) -> Option<String>;

    /// The sandbox provider these options run through, as a trait object.
    fn open_untraced(&self, yard: &Yard) -> Result<Arc<dyn SandboxProvider>, String>;

    /// [`ProviderKind::open_untraced`], with each action it is asked to do
    /// a span and an observation in
    /// `branchyard_sandbox_action_duration_seconds` under this provider's
    /// kind.
    fn open(&self, yard: &Yard) -> Result<Arc<dyn SandboxProvider>, String> {
        self.open_untraced(yard)
            .map(|provider| branchyard_sandbox::traced(self.name(), provider))
    }

    /// Destroy sandbox `sandbox` of a branch this provider ran, if it is
    /// still there, and say what happened.
    fn destroy(&self, yard: &Yard, sandbox: &str) -> Result<String, String>;
}

/// A branch's provider as [`Provider::label`] names it; `None` is local.
pub(crate) fn label_of(provider: Option<&Provider>) -> String {
    of(provider).label()
}

/// The implementation for a branch's provider; `None` is local.
pub(crate) fn of(provider: Option<&Provider>) -> &dyn ProviderKind {
    match provider {
        Some(provider) => provider.kind(),
        None => &local::LOCAL,
    }
}

impl Provider {
    /// This variant's implementation of [`ProviderKind`]: the one place a
    /// [`Provider`] is matched on.
    pub(crate) fn kind(&self) -> &dyn ProviderKind {
        match self {
            Provider::Local => &local::LOCAL,
            Provider::Microsandbox(options) => options,
            Provider::Substrate(options) => options,
            Provider::Recipe(options) => options,
            Provider::Container(options) => options,
            Provider::AgentSandbox(options) => options,
        }
    }

    /// The stable serialized provider kind, without a target or recipe name.
    pub fn name(&self) -> &'static str {
        self.kind().name()
    }

    /// Whether this provider uses a native owned allocation's runtime image.
    /// This does not establish runtime readiness or isolation qualification.
    pub fn is_native_image(&self) -> bool {
        matches!(self.harness_location(), HarnessLocation::Image)
    }

    /// Validate options that support pure validation, without creating state,
    /// opening a transport or reading credentials. Native image providers
    /// validate their image, target, resources and environment names here;
    /// other providers defer their checks to task preparation.
    pub fn validate_options(&self) -> Result<(), Error> {
        self.kind().validate_options()
    }

    /// Where readiness checks should look for this provider's harness.
    /// Borrowed target options remain tied to this provider.
    pub fn harness_location(&self) -> HarnessLocation<'_> {
        self.kind().harness_location()
    }

    /// How this provider is named in a branch's record and to a person:
    /// its stable kind, with the recipe's name when one is selected.
    pub fn label(&self) -> String {
        self.kind().label()
    }

    /// Where this provider's turns run, what isolates them, and its
    /// configured network and namespace; configuration, not readiness.
    pub fn compute(&self) -> ProviderCompute {
        self.kind().compute()
    }

    /// The image this provider's sandbox is made from, as it was given
    /// (docs/images.md); `None` when it names none.
    pub fn image(&self) -> Option<String> {
        self.kind().image()
    }

    /// `by`'s banner for a run under `provider` (`None` is local): the mode
    /// word and what the mode gives, as the provider's `ProviderKind::banner`
    /// says them.
    pub fn banner_of(
        provider: Option<&Provider>,
        isolated: bool,
        server: bool,
    ) -> (&'static str, String) {
        of(provider).banner(isolated, server)
    }

    /// Whether `provider` (`None` is local) runs the harness on this host,
    /// in no sandbox.
    pub fn is_local(provider: Option<&Provider>) -> bool {
        !of(provider).sandboxed()
    }

    /// A Microsandbox provider with `options`: how a caller outside this
    /// directory builds one (the configuration's `[microsandbox]` in
    /// `crate::configure`), since only this directory names a variant.
    pub fn microsandbox(options: crate::SandboxOptions) -> Provider {
        Provider::Microsandbox(options)
    }

    /// A Substrate provider using a saved connection and actor lifecycle settings.
    /// Construction stays with the provider implementations, as for Microsandbox.
    pub fn substrate(options: crate::SubstrateOptions) -> Provider {
        Provider::Substrate(options)
    }

    /// An owned per-turn container using the operator's configured worker.
    pub fn container(options: crate::ContainerOptions) -> Provider {
        Provider::Container(options)
    }

    /// An owned per-turn Kubernetes SIGs Agent Sandbox on managed GKE.
    pub fn agent_sandbox(options: crate::AgentSandboxOptions) -> Provider {
        Provider::AgentSandbox(options)
    }
}

#[cfg(test)]
mod tests;
