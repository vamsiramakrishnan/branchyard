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
use crate::{Error, Provider, Yard};

pub(crate) mod local;
pub(crate) mod microsandbox;
pub(crate) mod recipe;
pub(crate) mod substrate;

/// What the engine asks of a kind of provider. One implementation per
/// [`Provider`] variant, on that variant's options (or, for a variant
/// without any, on a unit type).
pub(crate) trait ProviderKind {
    /// `local`, `microsandbox`, `substrate` or `recipe`: the variant's
    /// serialized `kind`, as recorded in events and sandbox rows.
    fn name(&self) -> &'static str;

    /// Which provider, and where, holds a sandbox or snapshot row:
    /// sandboxes and snapshots are only ever used through the same one.
    /// Defaults to [`ProviderKind::name`].
    fn key(&self) -> String {
        self.name().to_owned()
    }

    /// What these options ask of the sandbox's lifecycle; `None` when
    /// there is no sandbox (a local harness).
    fn lifecycle(&self) -> Option<Lifecycle>;

    /// Whether the harness runs somewhere other than this host, in a
    /// sandbox this provider makes.
    fn sandboxed(&self) -> bool {
        true
    }

    /// Whether a network policy can be enforced on this host for a
    /// harness run by this provider, rather than only for a local one.
    fn confines_egress(&self) -> bool {
        false
    }

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
    fn open(&self, yard: &Yard) -> Result<Arc<dyn SandboxProvider>, String>;

    /// Destroy sandbox `sandbox` of a branch this provider ran, if it is
    /// still there, and say what happened.
    fn destroy(&self, yard: &Yard, sandbox: &str) -> Result<String, String>;
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
        }
    }
}

#[cfg(test)]
mod tests;
