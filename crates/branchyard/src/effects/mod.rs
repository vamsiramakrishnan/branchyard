//! Approvals, the effect ledger and undo (`docs/effects.md`).
//!
//! Rewinding a branch restores its files and its conversation; it cannot
//! unsend an email. This module keeps the second history: every connector
//! call that changes something outside the machine, in a ledger of
//! append-only events whose current state is a projection.
//!
//! - **Approvals.** Each tool and each connector operation resolves to
//!   allow, ask, block or stage ([`branchyard_provision::approvals`]),
//!   from an administrator's locked policy, the seat's, the person's and
//!   the preset's ([`ApprovalSettings`], [`crate::PolicyPreset::approvals`]).
//!   An `ask` is an [`ApprovalAsk`] in the store, answerable from any
//!   surface ([`crate::Yard::answer_approval`]); the turn waits within its
//!   budget.
//! - **The ledger.** A turn of a branch with connectors calls the gateway
//!   through a proxy of its own ([`proxy`]). For each effectful call the
//!   proxy writes the entry `begun` (committed) before it forwards the
//!   call, sends the entry's id as the idempotency key, and finishes the
//!   entry from the gateway's `_meta.effect` (or `X-Anvil-Effect`). A crash
//!   in between leaves `begun`, which recovery turns into `unknown`, which
//!   the [`reconcile`]r settles through the operation's declared lookup,
//!   never by calling it again.
//! - **Staging.** A `stage` decision performs an operation's draft form
//!   where it declares one, else holds the call in the outbox (its ask);
//!   [`crate::Yard::promote_effect`] performs it.
//! - **Undo.** [`undo`] plans from the ledger, grouped by what the
//!   upstream supports, and performs chosen inverses through the gateway
//!   under the branch's grant, recording each on its original entry.

mod api;
pub(crate) mod ask;
pub(crate) mod audit;
pub mod mcp;
#[cfg(test)]
pub(crate) mod memory;
pub mod proxy;
pub mod reconcile;
pub(crate) mod tool;
pub mod undo;

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use branchyard_provision::approvals::{
    is_deletion_name, narrow as narrow_approvals, resolve, Approval, ApprovalPolicy, EffectClass,
    Layer, Layers, Resolved, Subject,
};

use crate::Error;

/// Where an entry is in its life. `staged` → `begun` → `confirmed` |
/// `failed` | `unknown`; then `undone`, `compensated`, `undo_failed` or
/// `expired`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    /// Held: a draft upstream, or a call in the outbox, until approved.
    Staged,
    /// Written before the call; the call's outcome is not known yet.
    Begun,
    Confirmed,
    Failed,
    /// The call may or may not have happened; reconciliation asks the
    /// upstream, never calls again.
    Unknown,
    /// Its inverse was performed.
    Undone,
    /// Its compensation was performed.
    Compensated,
    /// Its inverse or compensation failed; the upstream's answer is kept.
    UndoFailed,
    /// Its inverse's deadline passed before it was asked for.
    Expired,
}

impl EffectState {
    pub const ALL: [EffectState; 9] = [
        EffectState::Staged,
        EffectState::Begun,
        EffectState::Confirmed,
        EffectState::Failed,
        EffectState::Unknown,
        EffectState::Undone,
        EffectState::Compensated,
        EffectState::UndoFailed,
        EffectState::Expired,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            EffectState::Staged => "staged",
            EffectState::Begun => "begun",
            EffectState::Confirmed => "confirmed",
            EffectState::Failed => "failed",
            EffectState::Unknown => "unknown",
            EffectState::Undone => "undone",
            EffectState::Compensated => "compensated",
            EffectState::UndoFailed => "undo_failed",
            EffectState::Expired => "expired",
        }
    }

    pub fn parse(text: &str) -> Result<EffectState, String> {
        EffectState::ALL
            .into_iter()
            .find(|s| s.as_str() == text)
            .ok_or_else(|| format!("{text:?} is not an effect state"))
    }
}

impl fmt::Display for EffectState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether an undo is a true inverse or a compensation.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UndoKind {
    Inverse,
    Compensate,
}

/// A follow-up call the gateway resolved (Anvil's `EffectCall`): the AIR
/// operation, the tool to call on this gateway (already prefixed with its
/// connector), and its concrete arguments.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FollowUp {
    /// The AIR operation id, such as `github.comments.delete`.
    pub operation: String,
    /// The gateway's tool name, such as `github__github_delete_comment`.
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
}

/// How an effect can be undone: the call the gateway resolved, and until
/// when.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Undo {
    /// The AIR operation id of the inverse or compensation.
    pub operation: String,
    /// The tool to call on the gateway.
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
    pub kind: UndoKind,
    /// Milliseconds since the Unix epoch after which it may no longer work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

/// How reconciliation asks the upstream whether a call happened: a read
/// called by the idempotency key or by an id, with concrete arguments.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lookup {
    /// The AIR operation id.
    pub operation: String,
    /// The tool to call on the gateway.
    pub tool: String,
    /// `idempotency_key` or `id`.
    pub by: String,
    #[serde(default)]
    pub arguments: Value,
}

/// Who answered for an effect, and how.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub by: String,
    pub at_ms: u64,
    /// `cli`, `watch`, `companion`, `api`, `parent`, `sdk`, or `policy`
    /// when the policy allowed it without asking.
    pub surface: String,
    pub allowed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A draft the gateway made for a staged call (Anvil's `staged`): its
/// handle, and the calls that promote and discard it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    /// The AIR operation the draft was made with.
    pub draft_operation: String,
    #[serde(default)]
    pub handle: Value,
    /// The call that performs the real effect; `None` when it could not be
    /// resolved (`unavailable` says why).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promote: Option<FollowUp>,
    /// The call that throws the draft away, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discard: Option<FollowUp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

/// How a staged entry is held.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Staged {
    /// The draft upstream, when the operation declares a draft form;
    /// `None` when the call is held in the outbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<Draft>,
    /// The ask that holds the call and its arguments.
    pub ask: String,
}

/// One effectful call, as the ledger projects it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectEntry {
    /// A ULID; also the call's idempotency key upstream.
    pub id: String,
    /// The task the branch serves: its delegation tree's root.
    pub task: String,
    pub branch: String,
    pub turn: u32,
    /// Who the branch acts for.
    pub subject: String,
    pub connector: String,
    /// The tool called, without its connector prefix.
    pub operation: String,
    /// The AIR operation id, when the gateway names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub class: EffectClass,
    pub state: EffectState,
    /// blake3 of the canonical request (connector, operation, arguments);
    /// never the arguments themselves.
    pub request_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<Undo>,
    /// A reversible effect's compensation, for after its inverse's
    /// deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensate: Option<Undo>,
    /// Why there is no undo although the class has one (the gateway's
    /// `undo_unavailable`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_unavailable: Option<String>,
    /// Who approved it, when policy asked; or the policy, when it allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalRecord>,
    /// What the approval policy decided before the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided: Option<Resolved>,
    /// Who approved its inverse or compensation, once one was performed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_approval: Option<ApprovalRecord>,
    #[serde(default)]
    pub deletion: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<Staged>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookup: Option<Lookup>,
    /// Whether the gateway described the effect (`_meta.effect`); without
    /// it, the entry is irreversible with no undo.
    #[serde(default)]
    pub declared: bool,
    /// The upstream's last answer, or why the state is what it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The idempotency key the gateway sent upstream, when it sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_key: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
}

impl EffectEntry {
    /// `slack: chat_post`, for people.
    pub fn title(&self) -> String {
        format!("{}: {}", self.connector, self.operation)
    }

    /// Whether its undo's deadline passed at `now_ms`.
    pub fn expired_at(&self, now_ms: u64) -> bool {
        self.undo
            .as_ref()
            .and_then(|u| u.deadline_ms)
            .is_some_and(|deadline| now_ms > deadline)
    }

    /// The compensation that still works at `now_ms` after the inverse
    /// expired, if the gateway gave one.
    pub fn compensation_at(&self, now_ms: u64) -> Option<&Undo> {
        self.compensate
            .as_ref()
            .filter(|c| c.deadline_ms.is_none_or(|d| now_ms <= d))
    }
}

/// A change to an entry after it was opened. Fields left out are kept.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EffectMove {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<EffectState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<EffectClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<Undo>,
    /// Drop the undo: the gateway said there is none.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_undo: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensate: Option<Undo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_unavailable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookup: Option<Lookup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_approval: Option<ApprovalRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_key: Option<String>,
    /// The draft, when staging made one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<Draft>,
}

impl EffectMove {
    pub fn to(state: EffectState) -> EffectMove {
        EffectMove {
            state: Some(state),
            ..EffectMove::default()
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> EffectMove {
        self.detail = Some(detail.into());
        self
    }

    /// Apply to `entry` at `at_ms`: the one projection every backend uses.
    pub fn apply(&self, entry: &mut EffectEntry, at_ms: u64) {
        if let Some(state) = self.state {
            entry.state = state;
        }
        if let Some(class) = self.class {
            entry.class = class;
        }
        if let Some(id) = &self.operation_id {
            entry.operation_id = Some(id.clone());
        }
        if self.no_undo {
            entry.undo = None;
        }
        if let Some(undo) = &self.undo {
            entry.undo = Some(undo.clone());
        }
        if let Some(compensate) = &self.compensate {
            entry.compensate = Some(compensate.clone());
        }
        if let Some(why) = &self.undo_unavailable {
            entry.undo_unavailable = Some(why.clone());
        }
        if let Some(lookup) = &self.lookup {
            entry.lookup = Some(lookup.clone());
        }
        if let Some(approval) = &self.approval {
            entry.approval = Some(approval.clone());
        }
        if let Some(approval) = &self.undo_approval {
            entry.undo_approval = Some(approval.clone());
        }
        if let Some(declared) = self.declared {
            entry.declared = declared;
        }
        if let Some(detail) = &self.detail {
            entry.detail = Some(detail.clone());
        }
        if let Some(key) = &self.upstream_key {
            entry.upstream_key = Some(key.clone());
        }
        if let Some(draft) = &self.draft {
            if let Some(staged) = entry.staged.as_mut() {
                staged.draft = Some(draft.clone());
            }
        }
        entry.updated_ms = entry.updated_ms.max(at_ms);
    }
}

/// One event in an entry's history.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectChange {
    /// The entry, as first written (`staged` or `begun`).
    Opened { entry: Box<EffectEntry> },
    /// A change after that.
    Moved(Box<EffectMove>),
}

/// An entry's event, as stored.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectEvent {
    /// Increases across the store.
    pub seq: u64,
    pub id: String,
    pub at_ms: u64,
    pub change: EffectChange,
}

/// Fold `events` (one entry's, in order) into its current state.
pub fn project(events: &[EffectEvent]) -> Option<EffectEntry> {
    let mut entry: Option<EffectEntry> = None;
    for event in events {
        match &event.change {
            EffectChange::Opened { entry: opened } => entry = Some((**opened).clone()),
            EffectChange::Moved(change) => {
                if let Some(entry) = entry.as_mut() {
                    change.apply(entry, event.at_ms);
                }
            }
        }
    }
    entry
}

/// What an ask is about.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AskAbout {
    /// A harness's tool.
    Tool { tool: String },
    /// A connector operation, before it is called.
    Operation {
        connector: String,
        operation: String,
        class: EffectClass,
        #[serde(default)]
        deletion: bool,
    },
    /// Performing a staged effect for real.
    Promote {
        connector: String,
        operation: String,
        class: EffectClass,
    },
}

impl AskAbout {
    /// One line for people.
    pub fn describe(&self) -> String {
        match self {
            AskAbout::Tool { tool } => format!("tool {tool}"),
            AskAbout::Operation {
                connector,
                operation,
                class,
                deletion,
            } => format!(
                "{connector} {operation} ({class}{})",
                if *deletion { ", deletes" } else { "" }
            ),
            AskAbout::Promote {
                connector,
                operation,
                class,
            } => format!("perform the staged {connector} {operation} ({class})"),
        }
    }
}

/// The answer to an ask.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswer {
    pub allow: bool,
    pub by: String,
    /// `cli`, `watch`, `companion`, `api`, `parent`, `sdk`, or `expired`
    /// when the turn's budget ran out first.
    pub surface: String,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl AskAnswer {
    pub fn record(&self) -> ApprovalRecord {
        ApprovalRecord {
            by: self.by.clone(),
            at_ms: self.at_ms,
            surface: self.surface.clone(),
            allowed: self.allow,
            reason: self.reason.clone(),
        }
    }
}

/// A question in the person's inbox: allow this, or not.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalAsk {
    pub id: String,
    pub branch: String,
    pub turn: u32,
    /// Who the branch acts for.
    pub subject: String,
    pub about: AskAbout,
    /// The ledger entry it is about, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    /// What the policy decided, and which layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<Resolved>,
    /// The tool's input or the call's arguments, so the person sees what
    /// they approve; for a call held in the outbox, what is performed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Value>,
    pub created_ms: u64,
    /// When the waiting turn gives up: its budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<AskAnswer>,
}

impl ApprovalAsk {
    pub fn pending(&self) -> bool {
        self.answer.is_none()
    }
}

/// What the ledger and approvals did on a branch, recorded as
/// [`crate::Activity::Effect`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectActivity {
    /// The turn's connector calls go through the ledger's proxy at `url`.
    Proxy { url: String },
    /// A person was asked.
    Asked {
        ask: String,
        about: AskAbout,
        resolved: Resolved,
    },
    /// The ask was answered.
    Answered {
        ask: String,
        allow: bool,
        by: String,
        surface: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A call was refused by policy before it was made.
    Blocked {
        connector: String,
        operation: String,
        resolved: Resolved,
    },
    /// A ledger entry changed.
    Entry {
        id: String,
        connector: String,
        operation: String,
        class: EffectClass,
        state: EffectState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

impl EffectActivity {
    pub fn of(entry: &EffectEntry) -> EffectActivity {
        EffectActivity::Entry {
            id: entry.id.clone(),
            connector: entry.connector.clone(),
            operation: entry.operation.clone(),
            class: entry.class,
            state: entry.state,
            detail: entry.detail.clone(),
        }
    }

    /// One line for people, as `by log` prints it.
    pub fn describe(&self) -> String {
        match self {
            EffectActivity::Proxy { url } => {
                format!("effects: connector calls are ledgered at {url}")
            }
            EffectActivity::Asked {
                ask,
                about,
                resolved,
            } => format!(
                "approval {ask}: asked about {} ({})",
                about.describe(),
                resolved.describe()
            ),
            EffectActivity::Answered {
                ask,
                allow,
                by,
                surface,
                reason,
            } => format!(
                "approval {ask}: {} by {by} ({surface}){}",
                if *allow { "allowed" } else { "denied" },
                reason
                    .as_deref()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default()
            ),
            EffectActivity::Blocked {
                connector,
                operation,
                resolved,
            } => format!(
                "effect: {connector} {operation} blocked ({})",
                resolved.describe()
            ),
            EffectActivity::Entry {
                id,
                connector,
                operation,
                class,
                state,
                detail,
            } => format!(
                "effect {}: {connector} {operation} {state} ({class}){}",
                short_id(id),
                detail
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            ),
        }
    }
}

/// The last eight characters of a ULID: what people type.
pub fn short_id(id: &str) -> &str {
    let n = id.len();
    &id[n.saturating_sub(8)..]
}

/// The ledger and the asks, in SQLite or PostgreSQL beside the branches,
/// or in memory. Entries are kept when their branch is removed: they are
/// what happened to the world.
pub(crate) trait EffectBackend: Send + Sync + fmt::Debug {
    /// Write a new entry and its `opened` event; refused if the id exists.
    fn open_effect(&self, entry: &EffectEntry) -> Result<(), Error>;
    /// Apply `change` at `at_ms` when the entry's state is one of `from`
    /// (any, when empty), with its event. `None` when the entry is missing
    /// or in another state.
    fn move_effect(
        &self,
        id: &str,
        from: &[EffectState],
        change: &EffectMove,
        at_ms: u64,
    ) -> Result<Option<EffectEntry>, Error>;
    fn effect(&self, id: &str) -> Result<Option<EffectEntry>, Error>;
    /// Every entry, or `branch`'s, oldest first.
    fn effects(&self, branch: Option<&str>) -> Result<Vec<EffectEntry>, Error>;
    /// An entry's events, in order.
    fn effect_events(&self, id: &str) -> Result<Vec<EffectEvent>, Error>;
    /// Store a new ask; refused if the id exists.
    fn put_ask(&self, ask: &ApprovalAsk) -> Result<(), Error>;
    /// Answer an unanswered ask. `None` when there is no such ask; the
    /// flag says whether this call answered it (false: it already was).
    fn answer_ask(
        &self,
        id: &str,
        answer: &AskAnswer,
    ) -> Result<Option<(ApprovalAsk, bool)>, Error>;
    fn ask(&self, id: &str) -> Result<Option<ApprovalAsk>, Error>;
    /// Every ask, or only those waiting, oldest first.
    fn asks(&self, pending_only: bool) -> Result<Vec<ApprovalAsk>, Error>;
}

/// The policies besides the seat's (which a branch carries in its
/// provisioning) that approvals resolve from. Set with
/// [`crate::Yard::use_approvals`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalSettings {
    /// An administrator's locked policy: nothing below it can loosen it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin: Option<ApprovalPolicy>,
    /// The person's policy, for anyone without an entry in `people`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person: Option<ApprovalPolicy>,
    /// Each person's policy, by subject (`local:ana`, a server principal).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub people: BTreeMap<String, ApprovalPolicy>,
}

impl ApprovalSettings {
    pub fn person_for(&self, subject: &str) -> Option<&ApprovalPolicy> {
        self.people.get(subject).or(self.person.as_ref())
    }

    pub fn check(&self) -> Result<(), String> {
        for policy in self
            .admin
            .iter()
            .chain(self.person.iter())
            .chain(self.people.values())
        {
            policy.check()?;
        }
        Ok(())
    }
}

/// A ULID for `now_ms`: 48 bits of time and 80 random bits, in Crockford's
/// base32, so ids sort by when they were made.
pub fn ulid(now_ms: u64) -> Result<String, Error> {
    let mut random = [0u8; 10];
    branchyard_support::rng::fill_random(&mut random)
        .map_err(|_| Error::State("could not generate a random id".into()))?;
    Ok(branchyard_support::ulid_from_parts(now_ms, random))
}

/// blake3 of the canonical request: the connector, the operation and the
/// arguments as JSON with sorted keys.
pub fn request_digest(connector: &str, operation: &str, arguments: &Value) -> String {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let sorted: BTreeMap<&String, Value> =
                    map.iter().map(|(k, v)| (k, canonical(v))).collect();
                Value::Object(sorted.into_iter().map(|(k, v)| (k.clone(), v)).collect())
            }
            Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
            other => other.clone(),
        }
    }
    let text = serde_json::to_string(&serde_json::json!({
        "connector": connector,
        "operation": operation,
        "arguments": canonical(arguments),
    }))
    .unwrap_or_default();
    format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex())
}

/// Find one entry by its id or the end of it (what people type).
pub(crate) fn find_effect(backend: &dyn EffectBackend, id: &str) -> Result<EffectEntry, Error> {
    if let Some(entry) = backend.effect(id)? {
        return Ok(entry);
    }
    let wanted = id.to_ascii_uppercase();
    let matches: Vec<EffectEntry> = backend
        .effects(None)?
        .into_iter()
        .filter(|e| e.id.ends_with(&wanted))
        .collect();
    let count = matches.len();
    match matches.into_iter().next() {
        Some(only) if count == 1 => Ok(only),
        None => Err(Error::Denied(format!("no effect {id} in the ledger"))),
        Some(_) => Err(Error::Denied(format!(
            "{id} names {count} effects; give more of its id"
        ))),
    }
}

/// Find one ask by its id or the end of it.
pub(crate) fn find_ask(backend: &dyn EffectBackend, id: &str) -> Result<ApprovalAsk, Error> {
    if let Some(ask) = backend.ask(id)? {
        return Ok(ask);
    }
    let wanted = id.to_ascii_uppercase();
    let matches: Vec<ApprovalAsk> = backend
        .asks(false)?
        .into_iter()
        .filter(|a| a.id.ends_with(&wanted))
        .collect();
    let count = matches.len();
    match matches.into_iter().next() {
        Some(only) if count == 1 => Ok(only),
        None => Err(Error::Denied(format!("no approval {id}"))),
        Some(_) => Err(Error::Denied(format!(
            "{id} names {count} approvals; give more of its id"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulids_sort_by_time_and_are_crockford() {
        let a = ulid(1_000).unwrap();
        let b = ulid(2_000).unwrap();
        assert_eq!(a.len(), 26);
        assert!(a < b, "{a} {b}");
        assert!(
            a.bytes()
                .all(|c| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&c)),
            "{a}"
        );
        assert_ne!(ulid(1_000).unwrap(), a);
        assert_eq!(&ulid(0).unwrap()[..10], "0000000000");
    }

    #[test]
    fn digests_ignore_key_order_and_hold_no_arguments() {
        let a = request_digest(
            "g",
            "o",
            &serde_json::json!({"a": 1, "b": {"y": 2, "x": "secret"}}),
        );
        let b = request_digest(
            "g",
            "o",
            &serde_json::json!({"b": {"x": "secret", "y": 2}, "a": 1}),
        );
        assert_eq!(a, b);
        assert!(a.starts_with("blake3:") && !a.contains("secret"));
        assert_ne!(a, request_digest("g", "p", &serde_json::json!({"a": 1})));
    }
}
