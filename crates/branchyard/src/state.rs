//! Durable state: `.branchyard/state.db`, and the directories the engine
//! owns (worktrees, private homes, delegation tokens).
//!
//! [`Store`] is what the engine uses. It forwards to a [`Backend`], the one
//! abstraction for durable state: branch records, the event log, journaled
//! steps, leases, harness process identities, cancel signals and steered
//! input. Local mode
//! uses [`crate::sqlite::Sqlite`]; `docs/durability.md` maps the same
//! operations onto PostgreSQL for the server.
//!
//! Every write for a running turn carries a [`Fence`]: the turn's lease and
//! the generation it was granted. A backend refuses a fenced write once the
//! lease has moved on, in the same transaction as the write, so an engine
//! that lost its lease cannot change the branch.
//!
//! A branch name is reserved by creating its row without a record, with
//! the reserving engine named like a lease's owner; the record is written
//! when the branch is created. A reservation whose engine is gone from this
//! host, or that is older than [`RESERVATION_TTL`], is reclaimed by
//! recovery. A record's `children`
//! belong to [`Store::add_child`] and to a graph commit that creates
//! children ([`crate::graph::GraphBackend::commit_graph`], which is how the
//! engine adds them): every other write keeps the list in the
//! store, so a turn that ends after it spawned children cannot drop them.

use branchyard_support::time::now_ms;
use branchyard_support::{CondvarExt as _, LockExt as _};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::delegation::Grant;
use crate::graph::GraphBackend;
use crate::storage::StorageBackend;
use crate::{proc, BranchInfo, Error, Message, Provider, RecordedEvent, SteerState};

pub(crate) const DIR: &str = ".branchyard";

/// How long a lease lasts without renewal.
pub(crate) const LEASE_TTL: Duration = Duration::from_secs(30);
/// How often a running turn renews its lease.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(5);
/// How long a reservation may stay without its branch being created
/// before any engine may reclaim it. Creating the branch follows reserving
/// its name within the same call, so this is far longer than it takes.
pub(crate) const RESERVATION_TTL: Duration = Duration::from_secs(600);
/// How often a waiting reader looks for another process's writes.
const POLL: Duration = Duration::from_millis(100);

/// The `.branchyard` directory of the repository at `root`.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

/// A branch's record: its public info and what the engine needs to continue
/// it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Record {
    pub info: BranchInfo,
    /// Milliseconds since the Unix epoch, for ordering.
    pub created_ms: u64,
    pub check: Option<Vec<String>>,
    /// `check` is its parent's, inherited because its spawn gave none.
    /// Siblings that inherited one whole-suite check pass it only together.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check_inherited: bool,
    /// The limits its turns were last given (`--budget-usd`,
    /// `--max-turns`, `--max-minutes`), kept for a later turn that gives
    /// none: the `by send` that continues it after its engine stopped runs
    /// under the same limits, and `by inspect` still shows its budget. A
    /// turn that gives one replaces it. A parent's limits on a delegated
    /// child are in its grant and narrow these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<crate::delegation::Limits>,
    /// The prompt of its last turn that recovery found cut off (submitted
    /// with an unknown outcome, or never run), until a turn submits a
    /// prompt again: `by send <branch> --retry` submits it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<String>,
    /// Command override, reused by sends and forks.
    pub command: Option<Vec<String>>,
    /// Private `HOME` when the branch runs isolated.
    pub home: Option<PathBuf>,
    /// What the harness's cumulative cost counts that is not the branch's
    /// own session's spend: for a forked session, the parent's cost at the
    /// fork, which the harness keeps reporting as part of the fork's; once
    /// a fresh session starts, minus the branch's cost before it, which
    /// the session's total does not count.
    pub cost_baseline: Option<f64>,
    /// The native session `cost_baseline` is for: the one its last turn
    /// ran, and `None` while a fresh session has not yet reported its id.
    /// Every turn that resumes it keeps the baseline, so a turn cut off
    /// before the harness reported its total, whose live estimate the
    /// branch's cost already counts, is not counted again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_session: Option<String>,
    /// The cumulative cost of each native session the branch left for
    /// another (after a rewind, or a fresh session), by session id: its
    /// baseline plus the branch's cost when it was left. A rewind can resume
    /// an older session, whose total is lower than the latest one's: the
    /// turn that resumes it sets the baseline from that total, so the
    /// branch's cost never falls. Empty for a record from before it was
    /// kept.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub session_costs: BTreeMap<String, f64>,
    /// Where the harness runs; `None` is local.
    #[serde(default)]
    pub provider: Option<Provider>,
    /// What the branch may delegate, and for a delegated child the limits
    /// and denials its parent imposed. `None`: no delegation.
    #[serde(default)]
    pub grant: Option<Grant>,
    /// What to provision before each turn; secrets by source, never value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<branchyard_provision::Provisioning>,
    /// Scratch areas the branch is bound to for every turn; see
    /// `crate::graph`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<crate::graph::Binding>,
    /// For a `waiting` branch created with an explicit base, that base,
    /// resolved when it was planned; without one it starts from its
    /// parent's branch as it is when it starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_base: Option<String>,
    /// The checkpoint the branch is at: `Some(0)` for its base, the last
    /// turn's after a turn, the rewound-to one after a rewind. `None` for a
    /// branch created before checkpoints were recorded. See
    /// `crate::checkpoint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<u32>,
    /// A summary of earlier turns that the next turn's prompt starts with,
    /// because a rewind could not continue the harness's own session.
    /// Cleared once a turn submits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// The branch's workspace lifecycle: what prepares its worktree and
    /// cleans up after it, and whether its setup completed. See
    /// `crate::workspace`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<crate::workspace::WorkspaceState>,
    /// Where the branch's next sandbox should come from when it has none
    /// kept: a provider snapshot of this branch's source (its parent, or
    /// itself after a rewind). Cleared once a turn has used it. See
    /// `crate::snapshots`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_seed: Option<crate::snapshots::SandboxSeed>,
    /// The `merged` status the branch had when its running turn started: a
    /// turn that changes nothing keeps it, as its candidate is the one
    /// already integrated. Cleared when the turn ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged: Option<crate::BranchStatus>,
    /// Who the branch acts for at the connector gateway (its token's `sub`
    /// and `by_tenant`): a server's principal, recorded when the branch is
    /// created and inherited by its forks and children. `None`: the yard's
    /// gateway default. See `crate::connectors`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<crate::connectors::Actor>,
    /// The branch's plan, when it was started with one: while it is being
    /// written its turns run read-only. See `crate::plan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<crate::plan::PlanState>,
    /// The goal a judge verifies when a turn ends ready. See `crate::goal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<crate::goal::GoalState>,
    /// While the branch is [`crate::BranchStatus::WaitingOnChildren`]: what
    /// its parked turn ended with and what wakes it. See `crate::wake`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<crate::wake::Parked>,
    /// Automatic wakes since the last turn something else started; bounded
    /// by the envelope's `max_wakes`.
    #[serde(default, skip_serializing_if = "crate::wake::is_zero")]
    pub wakes: u32,
    /// Why its last turn was lost when its engine stopped, until a turn
    /// tells its harness: recovery sets it, and the next turn's prompt
    /// starts with what happened. See `crate::wake::recovered_note`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lost: Option<String>,
    /// Tools the branch was started denying (`by run --deny`), ahead of
    /// every policy its turns run under, and passed on to its children.
    /// A delegated child's own come from its parent, in its grant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// Delegated children that were removed, with what each one's subtree
    /// had spent: the ledger that keeps a removed child's spend in this
    /// branch's subtree cost and budget. Appended by the store as it
    /// deletes the child, and kept by every later write of this record, as
    /// its children are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<RemovedChild>,
}

/// A delegated child that was removed, as its parent's record remembers it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct RemovedChild {
    pub name: String,
    /// What it and its descendants had spent when it was removed;
    /// unreported costs count as zero.
    pub spent_usd: f64,
    /// Milliseconds since the Unix epoch.
    pub removed_ms: u64,
}

impl Record {
    /// What this branch and its descendants have spent: its own reported
    /// cost, its removed children's, and its children's, read with
    /// `read`. Unreported costs count as zero.
    pub(crate) fn subtree_spent(&self, read: &mut dyn FnMut(&str) -> Option<Record>) -> f64 {
        fn walk(
            record: &Record,
            read: &mut dyn FnMut(&str) -> Option<Record>,
            seen: &mut std::collections::BTreeSet<String>,
        ) -> f64 {
            if !seen.insert(record.info.name.clone()) {
                return 0.0;
            }
            let mut children = 0.0;
            for name in &record.info.children {
                if let Some(child) = read(name) {
                    children += walk(&child, read, seen);
                }
            }
            record.info.cost_usd.unwrap_or(0.0) + record.removed_spent() + children
        }
        walk(self, read, &mut std::collections::BTreeSet::new())
    }

    /// What its removed children's subtrees had spent.
    pub(crate) fn removed_spent(&self) -> f64 {
        self.removed.iter().map(|r| r.spent_usd).sum()
    }
}

/// Record in `parent`'s ledger that its child `removed` is being deleted,
/// with what its subtree spent (its descendants read with `read`), and
/// take it off `parent`'s children: one list of children, which `inspect`,
/// `children` and the envelope all read. False, and nothing changed, when
/// `removed` is not one of `parent`'s delegated children (a fork names the
/// branch it came from as its parent too).
pub(crate) fn note_removed(
    parent: &mut Record,
    removed: &Record,
    read: &mut dyn FnMut(&str) -> Option<Record>,
) -> bool {
    let name = &removed.info.name;
    if !parent.info.children.contains(name) {
        return false;
    }
    parent.removed.push(RemovedChild {
        name: name.clone(),
        spent_usd: removed.subtree_spent(read),
        removed_ms: now_ms(),
    });
    parent.info.children.retain(|child| child != name);
    true
}

/// The right to write a branch's state for one turn: the branch's current
/// incarnation (a removed and recreated branch is a new one), the lease
/// generation, and the turn the lease was granted for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fence {
    pub branch: String,
    pub incarnation: i64,
    pub generation: u64,
    /// The engine call the lease was granted for; the key of its journaled
    /// steps. Equal to the generation it was granted with; a recovery that
    /// takes the lease over keeps the turn and bumps the generation.
    pub turn: u64,
}

/// An engine instance that can hold leases: one per opened [`Store`].
#[derive(Clone, Debug)]
pub(crate) struct Owner {
    pub id: String,
    /// Host and boot, from [`proc::host`].
    pub host: String,
    pub pid: u32,
    /// The process's start time, from [`proc::start_time`].
    pub start: String,
}

impl Owner {
    fn new() -> Owner {
        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        Owner {
            id: format!(
                "{pid}-{:x}-{}",
                branchyard_support::time::now_nanos(),
                N.fetch_add(1, Ordering::Relaxed)
            ),
            host: proc::host().to_owned(),
            pid,
            start: proc::own_start().to_owned(),
        }
    }
}

/// A branch's lease as stored.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LeaseRow {
    pub branch: String,
    pub incarnation: i64,
    pub generation: u64,
    pub turn: u64,
    /// `None` once released.
    pub owner: Option<String>,
    pub host: String,
    pub pid: u32,
    pub start: String,
    pub expires_ms: u64,
    /// The turn's `max_duration` deadline, when it has one.
    pub deadline_ms: Option<u64>,
}

/// Whether the process `pid` that started at `start` on `host` is known
/// to be gone: it ran on this host and boot, and is not running now.
pub(crate) fn gone(host: &str, pid: u32, start: &str) -> bool {
    host == proc::host() && !proc::alive(pid, start)
}

impl LeaseRow {
    /// Why this held lease no longer protects a live engine, if it does
    /// not: its owner is gone from this host, or it expired.
    pub fn stale(&self, now_ms: u64) -> Option<String> {
        self.owner.as_ref()?;
        if gone(&self.host, self.pid, &self.start) {
            return Some(format!(
                "its engine (pid {}) is no longer running",
                self.pid
            ));
        }
        if self.expires_ms <= now_ms {
            return Some(format!(
                "its engine's lease expired {}s ago",
                (now_ms - self.expires_ms) / 1000
            ));
        }
        None
    }
}

/// A name reserved for a branch not yet created, and the engine that
/// reserved it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReservationRow {
    pub name: String,
    /// The reserving [`Owner`]'s ID.
    pub owner: String,
    pub host: String,
    pub pid: u32,
    pub start: String,
    pub reserved_ms: u64,
}

impl ReservationRow {
    /// Why this reservation can be reclaimed, if it can: the engine that
    /// made it is gone from this host, or it is older than
    /// [`RESERVATION_TTL`].
    pub fn stale(&self, now_ms: u64) -> Option<String> {
        if gone(&self.host, self.pid, &self.start) {
            return Some(format!(
                "the engine that reserved it (pid {}) is no longer running",
                self.pid
            ));
        }
        let age = now_ms.saturating_sub(self.reserved_ms);
        (age >= RESERVATION_TTL.as_millis() as u64)
            .then(|| format!("it was reserved {}s ago and never created", age / 1000))
    }
}

/// What [`Backend::acquire`] found.
#[derive(Debug)]
pub(crate) enum Acquired {
    Granted(Fence),
    /// Another owner holds the lease.
    Held(LeaseRow),
}

/// A journaled step as [`Backend::begin_step`] found it.
#[derive(Debug, PartialEq)]
pub(crate) enum Begun {
    /// The intent is now recorded; carry out the effect.
    Fresh,
    /// An earlier attempt recorded this intent and no outcome: the effect
    /// may or may not have happened.
    Pending(Value),
    /// The step already completed with this outcome.
    Done(Value),
}

/// One journaled step.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StepRow {
    pub step: String,
    pub intent: Value,
    pub outcome: Option<Value>,
}

/// A harness process started for a turn.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProcessRow {
    pub pid: u32,
    pub pgid: u32,
    pub start: String,
    pub host: String,
}

/// Input queued for a branch's running turn, bound to that turn like a
/// cancel.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SteerRow {
    pub id: u64,
    pub branch: String,
    /// The engine call ([`Fence::turn`]) it was queued for.
    pub turn: u64,
    pub by: String,
    pub text: String,
    pub requested_ms: u64,
    pub state: SteerState,
    /// The inbox message this input carries, when a message is being
    /// delivered into the running turn by steering
    /// ([`crate::inbox::SteerDelivery`]).
    pub message: Option<u64>,
    /// Whether that message is already delivered, by this input or, when
    /// it raced the turn's start, in the turn's prompt.
    pub message_delivered: bool,
}

impl SteerState {
    /// The stored state name (the text serde puts in `state`) and reason.
    pub(crate) fn columns(&self) -> (&'static str, Option<&str>) {
        let reason = match self {
            SteerState::Refused { reason } => Some(reason.as_str()),
            _ => None,
        };
        (self.into(), reason)
    }

    /// The state a row stores; an error for a name no state has, rather
    /// than reading it as a refusal.
    pub(crate) fn from_columns(
        state: &str,
        reason: Option<String>,
    ) -> Result<SteerState, crate::store_codec::CodecError> {
        match state {
            "pending" => Ok(SteerState::Pending),
            // `delivered` is what `written` was stored as before.
            "written" | "delivered" => Ok(SteerState::Written),
            "accepted" => Ok(SteerState::Accepted),
            "refused" => Ok(SteerState::Refused {
                reason: reason.unwrap_or_default(),
            }),
            other => Err(crate::store_codec::CodecError::UnknownText {
                field: "steer state",
                text: other.to_owned(),
            }),
        }
    }
}

/// One event in the repository-wide feed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FeedRow {
    pub id: u64,
    pub branch: String,
    pub event: RecordedEvent,
}

/// Durable state for one repository. Every method is atomic. Methods that
/// take a [`Fence`] fail with [`Error::Fenced`] unless the branch's lease
/// still has the fence's incarnation and generation, checked in the same
/// transaction as the write.
pub(crate) trait Backend: Send + Sync + fmt::Debug {
    /// Reserve `name` for `owner`; false if it is already taken.
    fn reserve(&self, name: &str, owner: &Owner) -> Result<bool, Error>;
    /// Give up a reservation that has no record yet.
    fn release(&self, name: &str) -> Result<(), Error>;
    /// Every reservation whose branch has not been created, with the engine
    /// that made it. A reservation made by an earlier version names no
    /// engine and is not listed.
    fn reservations(&self) -> Result<Vec<ReservationRow>, Error>;
    /// Free the name `row` reserved, unless it changed since it was read:
    /// created, released, or reserved again. True if it was freed.
    fn reclaim(&self, row: &ReservationRow) -> Result<bool, Error>;
    fn taken(&self, name: &str) -> Result<bool, Error>;
    /// The created branch's record; `None` for a reservation or no branch.
    fn read(&self, name: &str) -> Result<Option<Record>, Error>;
    /// Every created branch, oldest first.
    fn list(&self) -> Result<Vec<Record>, Error>;
    /// Replace the record, keeping the stored `children`.
    fn write(&self, record: &Record, fence: Option<&Fence>) -> Result<(), Error>;
    /// Children are added by a graph commit; this remains for tests and
    /// tools that build a tree directly.
    #[cfg_attr(not(test), allow(dead_code))]
    fn add_child(&self, parent: &str, child: &str) -> Result<(), Error>;
    /// Delete the branch's record, lease, steps, processes, cancels and
    /// steered input. Its events stay in the feed.
    fn delete(&self, name: &str) -> Result<(), Error>;

    /// Write `record` and take the branch's lease for a new turn, unless a
    /// lease is held. Creating a reserved branch requires that `owner` made
    /// the reservation; one that was reclaimed and reserved again by
    /// another engine is refused with [`Error::BranchExists`].
    fn acquire(&self, record: &Record, owner: &Owner, ttl: Duration) -> Result<Acquired, Error>;
    fn renew(&self, fence: &Fence, ttl: Duration) -> Result<(), Error>;
    /// Write `record` and append `event`, if given, then release the lease:
    /// atomically.
    fn finish(
        &self,
        fence: &Fence,
        record: Option<&Record>,
        event: Option<&RecordedEvent>,
    ) -> Result<(), Error>;
    /// Every held lease.
    fn leases(&self) -> Result<Vec<LeaseRow>, Error>;
    /// Take over `lease`, as observed, for recovery: a new generation for
    /// the same turn. `None` if it changed meanwhile.
    fn take_over(
        &self,
        lease: &LeaseRow,
        owner: &Owner,
        ttl: Duration,
    ) -> Result<Option<Fence>, Error>;
    fn set_deadline(&self, fence: &Fence, deadline_ms: Option<u64>) -> Result<(), Error>;

    /// Record the intent of step `(branch, turn, step)` unless it exists.
    fn begin_step(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        intent: &Value,
    ) -> Result<Begun, Error>;
    fn finish_step(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        outcome: &Value,
    ) -> Result<(), Error>;
    /// Forget a step whose effect failed without effect, so it can run
    /// again.
    fn abandon_step(&self, fence: &Fence, turn: u64, step: &str) -> Result<(), Error>;
    /// [`Backend::begin_step`], and, when this call records the intent,
    /// mark the inbox messages `deliver` delivered in the same transaction
    /// (as [`Backend::mark_delivered`] does), stamped with the step's start
    /// time: a turn's prompt and the messages it carries become durable
    /// together, or neither does.
    fn begin_step_delivering(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        intent: &Value,
        deliver: &[u64],
    ) -> Result<Begun, Error>;
    /// [`Backend::abandon_step`], and return to pending, in the same
    /// transaction, those of `deliver` that the step's
    /// [`Backend::begin_step_delivering`] marked delivered (not ones a
    /// steered input or another path delivered).
    fn abandon_step_delivering(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        deliver: &[u64],
    ) -> Result<(), Error>;
    /// The steps of one turn of the branch's current incarnation.
    fn steps(&self, name: &str, turn: u64) -> Result<Vec<StepRow>, Error>;

    fn record_process(&self, fence: &Fence, process: &ProcessRow) -> Result<(), Error>;
    /// The processes recorded for one turn.
    fn processes(&self, name: &str, turn: u64) -> Result<Vec<ProcessRow>, Error>;

    /// Ask the branch's running turn to stop; false when no turn holds its
    /// lease. The first request for a turn is kept.
    fn request_cancel(&self, name: &str, by: &str, subtree: bool) -> Result<bool, Error>;
    /// Who asked to cancel the fenced turn, if anyone did.
    fn cancel_requested(&self, fence: &Fence) -> Result<Option<String>, Error>;

    /// Queue `text` from `by` for the branch's running turn, bound to that
    /// turn as a cancel is; its ID, or `None` when no turn holds the
    /// branch's lease. With `message`, the input carries that inbox
    /// message, linked to it in the same transaction; it fails with
    /// [`Error::Denied`], queueing nothing, when the message is unknown or
    /// already delivered.
    fn request_steer(
        &self,
        name: &str,
        by: &str,
        text: &str,
        message: Option<u64>,
    ) -> Result<Option<u64>, Error>;
    /// The fenced turn's steered input still [`SteerState::Pending`],
    /// oldest first.
    fn pending_steers(&self, fence: &Fence) -> Result<Vec<SteerRow>, Error>;
    /// Record what became of the fenced turn's steered input `id`. When it
    /// carries an inbox message, the message's delivery moves in the same
    /// transaction: [`SteerState::Written`] or [`SteerState::Accepted`]
    /// marks it delivered, and returns its id if this call did so;
    /// [`SteerState::Refused`] returns a message this input had delivered
    /// to pending, unlinked, for the recipient's next turn start.
    fn settle_steer(
        &self,
        fence: &Fence,
        id: u64,
        state: &SteerState,
    ) -> Result<Option<u64>, Error>;
    /// One steered input of the branch's current incarnation.
    fn steer(&self, name: &str, id: u64) -> Result<Option<SteerRow>, Error>;

    /// Append an event to the branch's log; returns its sequence number in
    /// the branch, counting from 1.
    fn append(
        &self,
        name: &str,
        event: &RecordedEvent,
        fence: Option<&Fence>,
    ) -> Result<u64, Error>;
    /// Up to `limit` events of the branch with sequence numbers after
    /// `after`.
    fn events_since(
        &self,
        name: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<(u64, RecordedEvent)>, Error>;
    /// How many events the branch has.
    fn event_count(&self, name: &str) -> Result<u64, Error>;
    /// Up to `limit` events of every branch with feed positions after
    /// `after`. Positions only grow, in commit order.
    fn feed_since(&self, after: u64, limit: usize) -> Result<Vec<FeedRow>, Error>;
    /// The last feed position; 0 when empty.
    fn head(&self) -> Result<u64, Error>;

    /// Store a harness-to-harness message: assigns its id (counting from 1
    /// across the repository) and `at_ms`, and returns the stored copy.
    fn send_message(&self, message: &Message) -> Result<Message, Error>;
    /// One message by id, if it exists.
    fn message(&self, id: u64) -> Result<Option<Message>, Error>;
    /// Every message addressed to `to`, oldest first.
    fn inbox(&self, to: &str) -> Result<Vec<Message>, Error>;
    /// Mark these messages delivered; already-delivered and unknown ids are
    /// ignored. Steered input still queued with a message marked here
    /// finds it delivered ([`SteerRow::message_delivered`]) and is refused
    /// unwritten.
    fn mark_delivered(&self, ids: &[u64]) -> Result<(), Error>;
    /// The first message that answers the question `question_id`, if one
    /// has arrived.
    fn answer_to(&self, question_id: u64) -> Result<Option<Message>, Error>;
    /// The steered input carrying message `id`, if one was queued for it
    /// (see [`Backend::request_steer`]) and has not been refused.
    fn message_steer(&self, id: u64) -> Result<Option<u64>, Error>;
    /// Record that a waiter is blocked for an answer to question `id` until
    /// `until_ms` (milliseconds since the Unix epoch), or, with `None`,
    /// that it stopped waiting.
    fn set_awaiting(&self, id: u64, until_ms: Option<u64>) -> Result<(), Error>;
    /// Whether `from` sent a question that has no answer yet and a waiter
    /// whose deadline is after `now_ms`.
    fn awaiting_answer(&self, from: &str, now_ms: u64) -> Result<bool, Error>;
}

/// Ports reserved for branches (`BRANCHYARD_PORT`), one per branch name,
/// none shared: each is unique across the store (across every repository
/// in a PostgreSQL database). A branch's reservation is deleted with the
/// branch by [`Backend::delete`]. See `crate::workspace`.
pub(crate) trait PortBackend: Send + Sync + fmt::Debug {
    /// `branch`'s port. When it has none, reserve the first port from
    /// `start` on, wrapping within [`crate::workspace::PORT_RANGE`], that no
    /// branch holds and for which `usable` is true, in one transaction.
    fn reserve_port(
        &self,
        branch: &str,
        start: u16,
        usable: &(dyn Fn(u16) -> bool + Sync),
    ) -> Result<u16, Error>;
    /// `branch`'s reserved port, if it has one.
    fn port(&self, branch: &str) -> Result<Option<u16>, Error>;
}

/// What a sandbox row records: a branch's kept sandbox, or one of its
/// sandbox snapshots. See `crate::snapshots`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SandboxKind {
    /// The branch's sandbox, paused between turns (`keep = "pause"`).
    Kept,
    /// A provider snapshot taken at one of the branch's checkpoints.
    Snapshot,
}

impl SandboxKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxKind::Kept => "kept",
            SandboxKind::Snapshot => "snapshot",
        }
    }

    pub fn parse(text: &str) -> Result<SandboxKind, Error> {
        match text {
            "kept" => Ok(SandboxKind::Kept),
            "snapshot" => Ok(SandboxKind::Snapshot),
            other => Err(Error::State(format!("unknown sandbox kind {other:?}"))),
        }
    }
}

/// A provider-side sandbox or snapshot a branch owns, so that a later turn,
/// a fork or a removal in any process can find it, and eviction can pick
/// the least recently used across the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SandboxRow {
    pub branch: String,
    pub incarnation: i64,
    pub kind: SandboxKind,
    /// Which provider holds it, and where: `crate::snapshots::provider_key`.
    pub provider: String,
    /// The sandbox's name, or the snapshot's handle.
    pub name: String,
    /// For a snapshot, its checkpoint; for a kept sandbox, the checkpoint
    /// its state corresponds to, once recorded.
    pub turn: Option<u32>,
    /// Provider details as JSON: `crate::snapshots::Detail`.
    pub detail: String,
    /// Last parked or taken, for least-recently-used eviction.
    pub used_ms: u64,
}

/// Sandboxes and sandbox snapshots owned by branches. A row is claimed by
/// deleting it ([`SandboxBackend::take_sandbox`]): whoever deletes it owns
/// the sandbox, so an engine resuming a kept sandbox and another evicting
/// it never both act on it. Rows of a branch are deleted with it by
/// [`Backend::delete`]; the provider-side sandboxes are the engine's to
/// destroy first.
pub(crate) trait SandboxBackend: Send + Sync + fmt::Debug {
    /// Insert or replace the row for (`branch`, `kind`, `name`).
    fn put_sandbox(&self, row: &SandboxRow) -> Result<(), Error>;
    /// Every row of `branch`, oldest first.
    fn sandboxes(&self, branch: &str) -> Result<Vec<SandboxRow>, Error>;
    /// Every row of `kind` in the store whose provider is `provider`, least
    /// recently used first.
    fn sandboxes_of(&self, kind: SandboxKind, provider: &str) -> Result<Vec<SandboxRow>, Error>;
    /// Delete the row and return it, or `None` if another took it first.
    fn take_sandbox(
        &self,
        branch: &str,
        kind: SandboxKind,
        name: &str,
    ) -> Result<Option<SandboxRow>, Error>;
}

/// Where a warm pool's slot is in its life. See `crate::pool`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SlotState {
    /// Its worktree is being made by the process its row names.
    Filling,
    /// Ready and unclaimed.
    Ready,
    /// Taken by the process its row names: for a branch, or to be
    /// discarded. Never ready again.
    Claimed,
}

impl SlotState {
    pub fn as_str(self) -> &'static str {
        match self {
            SlotState::Filling => "filling",
            SlotState::Ready => "ready",
            SlotState::Claimed => "claimed",
        }
    }

    pub fn parse(text: &str) -> Result<SlotState, Error> {
        match text {
            "filling" => Ok(SlotState::Filling),
            "ready" => Ok(SlotState::Ready),
            "claimed" => Ok(SlotState::Claimed),
            other => Err(Error::State(format!("unknown pool slot state {other:?}"))),
        }
    }
}

/// A warm pool's slot as stored: a prepared worktree on one host's
/// checkout (`place`), and who is making or taking it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SlotRow {
    pub id: String,
    /// `<hostname>:<repository root>`: only processes there see its
    /// worktree.
    pub place: String,
    /// The pool it belongs to: what made it (`crate::pool::recipe`).
    pub recipe: String,
    pub state: SlotState,
    /// The commit its worktree is at.
    pub base: String,
    /// Its worktree.
    pub path: String,
    /// JSON: the environment restored into it, how, and for a claim the
    /// branch's worktree (`crate::pool::Detail`).
    pub detail: String,
    /// The process filling or claiming it, as a lease names its holder.
    pub host: String,
    pub pid: u32,
    pub start: String,
    /// The branch it was claimed for; `None` when claimed to be discarded.
    pub branch: Option<String>,
    pub created_ms: u64,
    /// When it last changed state.
    pub changed_ms: u64,
}

/// Warm pool slots ([`SlotRow`]). A slot changes state only by
/// compare-and-set ([`PoolBackend::update_slot`]), so of several engines
/// claiming one ready slot exactly one gets it, and a claimed slot is
/// never ready again. Rows belong to no branch: deleting a branch leaves
/// them.
pub(crate) trait PoolBackend: Send + Sync + fmt::Debug {
    /// Insert a new row; fails if its id exists.
    fn insert_slot(&self, row: &SlotRow) -> Result<(), Error>;
    /// Every row at `place` (every place with `None`), oldest first.
    fn slots(&self, place: Option<&str>) -> Result<Vec<SlotRow>, Error>;
    /// Replace the row with `row` if it is still in state `expected`;
    /// whether it was.
    fn update_slot(&self, row: &SlotRow, expected: SlotState) -> Result<bool, Error>;
    /// Delete the row; whether it existed.
    fn delete_slot(&self, id: &str) -> Result<bool, Error>;
}

/// [`PortBackend`], [`SandboxBackend`], the outcome store
/// ([`crate::fleet::OutcomeBackend`]), the knowledge store
/// ([`crate::knowledge::KnowledgeBackend`]), pool slots ([`PoolBackend`])
/// the model usage store ([`crate::models::UsageBackend`]) and the effect
/// ledger ([`crate::effects::EffectBackend`]) together,
/// so a [`Store`] holds one trait object for them.
pub(crate) trait Extras:
    PortBackend
    + SandboxBackend
    + crate::fleet::OutcomeBackend
    + crate::knowledge::KnowledgeBackend
    + PoolBackend
    + crate::models::UsageBackend
    + crate::effects::EffectBackend
{
}

impl<
        T: PortBackend
            + SandboxBackend
            + crate::fleet::OutcomeBackend
            + crate::knowledge::KnowledgeBackend
            + PoolBackend
            + crate::models::UsageBackend
            + crate::effects::EffectBackend,
    > Extras for T
{
}

/// The port a reservation takes: from `start`, the first not in `taken`
/// for which `usable` holds.
pub(crate) fn pick_port(
    start: u16,
    taken: &std::collections::BTreeSet<u16>,
    usable: &(dyn Fn(u16) -> bool + Sync),
) -> Result<u16, Error> {
    let (low, high) = crate::workspace::PORT_RANGE;
    let start = start.clamp(low, high);
    let mut port = start;
    loop {
        if !taken.contains(&port) && usable(port) {
            return Ok(port);
        }
        port = crate::workspace::next_port(port);
        if port == start {
            return Err(Error::State(format!(
                "no free port between {low} and {high} for a branch"
            )));
        }
    }
}

/// Wakes readers in this process when events are appended to a store.
#[derive(Debug, Default)]
struct Signal {
    appended: Mutex<u64>,
    changed: Condvar,
}

fn signal_for(path: &Path) -> Arc<Signal> {
    static SIGNALS: OnceLock<Mutex<HashMap<PathBuf, Arc<Signal>>>> = OnceLock::new();
    let mut signals = SIGNALS
        .get_or_init(Default::default)
        .lock_recovering("task signals");
    signals.entry(path.to_path_buf()).or_default().clone()
}

/// The engine's handle on durable state. Cheap to clone; clones share the
/// backend and the owner identity.
#[derive(Clone, Debug)]
pub(crate) struct Store {
    dir: PathBuf,
    backend: Arc<dyn Backend>,
    /// Artifact and scratch-area metadata: the same backend as `backend`,
    /// coerced to a second trait object so that feature does not enlarge
    /// [`Backend`]. See [`crate::storage`].
    storage: Arc<dyn StorageBackend>,
    /// Dependencies and graph revisions: the same backend again, as for
    /// `storage`. See [`crate::graph`].
    graph: Arc<dyn GraphBackend>,
    /// Branch ports, kept sandboxes and sandbox snapshots: the same backend
    /// again, as one trait object for both. See [`PortBackend`] and
    /// [`SandboxBackend`].
    extras: Arc<dyn Extras>,
    owner: Arc<Owner>,
    signal: Arc<Signal>,
}

impl Store {
    /// Open the repository's store, creating `.branchyard/` and importing
    /// branch records and event logs left by earlier versions.
    pub fn open(root: &Path) -> Result<Store, Error> {
        let dir = dir(root);
        let worktrees = dir.join("worktrees");
        std::fs::create_dir_all(&worktrees)
            .map_err(|e| Error::State(format!("create {}: {e}", worktrees.display())))?;
        let backend = Arc::new(crate::sqlite::Sqlite::open(&dir)?);
        let signal = signal_for(backend.path());
        Ok(Store {
            dir,
            backend: backend.clone(),
            storage: backend.clone(),
            graph: backend.clone(),
            extras: backend,
            owner: Arc::new(Owner::new()),
            signal,
        })
    }

    /// Open the repository's store in the PostgreSQL database at `url`,
    /// scoped to `scope`, keeping `.branchyard/` for worktrees, homes and
    /// delegation tokens. Nothing is imported from `state.db`.
    #[cfg(feature = "postgres")]
    pub fn open_postgres(root: &Path, url: &str, scope: &str) -> Result<Store, Error> {
        let dir = dir(root);
        let worktrees = dir.join("worktrees");
        std::fs::create_dir_all(&worktrees)
            .map_err(|e| Error::State(format!("create {}: {e}", worktrees.display())))?;
        let backend = Arc::new(crate::pg::Postgres::open(url, scope)?);
        let signal = signal_for(&dir.join(format!("postgres/{scope}")));
        Ok(Store {
            dir,
            backend: backend.clone(),
            storage: backend.clone(),
            graph: backend.clone(),
            extras: backend,
            owner: Arc::new(Owner::new()),
            signal,
        })
    }

    /// The `.branchyard` directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn owner(&self) -> &Owner {
        &self.owner
    }

    pub fn backend(&self) -> &dyn Backend {
        self.backend.as_ref()
    }

    /// Artifact and scratch-area metadata; see [`crate::storage`].
    pub fn storage(&self) -> &dyn StorageBackend {
        self.storage.as_ref()
    }

    /// Dependencies and graph revisions; see [`crate::graph`].
    pub fn graph(&self) -> &dyn GraphBackend {
        self.graph.as_ref()
    }

    /// Branch ports; see [`PortBackend`].
    pub fn ports(&self) -> &dyn PortBackend {
        self.extras.as_ref()
    }

    /// Kept sandboxes and sandbox snapshots; see [`SandboxBackend`].
    pub fn sandboxes(&self) -> &dyn SandboxBackend {
        self.extras.as_ref()
    }

    /// Finished branches' outcomes; see [`crate::fleet::OutcomeBackend`].
    pub fn outcomes(&self) -> &dyn crate::fleet::OutcomeBackend {
        self.extras.as_ref()
    }

    /// Repository knowledge; see [`crate::knowledge::KnowledgeBackend`].
    pub fn knowledge(&self) -> &dyn crate::knowledge::KnowledgeBackend {
        self.extras.as_ref()
    }

    /// Warm pool slots; see [`PoolBackend`].
    pub fn pool(&self) -> &dyn PoolBackend {
        self.extras.as_ref()
    }

    /// Calls through the model gateway; see [`crate::models::UsageBackend`].
    pub fn usage(&self) -> &dyn crate::models::UsageBackend {
        self.extras.as_ref()
    }

    /// The effect ledger and approval asks; see
    /// [`crate::effects::EffectBackend`].
    pub fn effects(&self) -> &dyn crate::effects::EffectBackend {
        self.extras.as_ref()
    }

    pub fn worktree(&self, name: &str) -> PathBuf {
        self.dir.join("worktrees").join(name)
    }

    pub fn home(&self, name: &str) -> PathBuf {
        self.dir.join("homes").join(name)
    }

    /// `name`'s private temporary directory, the `TMPDIR` its local
    /// harness runs with; see `docs/egress.md`.
    pub fn tmp(&self, name: &str) -> PathBuf {
        self.dir.join("tmp").join(name)
    }

    /// Where the engine running `name` writes its delegation token and the
    /// address of its broker, while a turn runs.
    pub fn token_path(&self, name: &str) -> PathBuf {
        self.dir.join("delegation").join(format!("{name}.json"))
    }

    /// Whether a record or reservation exists for `name`. An unreadable
    /// store counts as taken.
    pub fn taken(&self, name: &str) -> bool {
        self.backend.taken(name).unwrap_or(true)
    }

    /// Reserve `name`; false if it is already taken.
    pub fn reserve(&self, name: &str) -> Result<bool, Error> {
        self.backend.reserve(name, &self.owner)
    }

    /// Give up a reservation made by [`Store::reserve`].
    pub fn release(&self, name: &str) {
        branchyard_support::best_effort("release a name reservation", self.backend.release(name));
    }

    /// Replace the record outside any turn. The `children` already stored
    /// are kept.
    pub fn write(&self, record: &Record) -> Result<(), Error> {
        self.backend.write(record, None)
    }

    /// Replace the record for the fenced turn.
    pub fn write_fenced(&self, record: &Record, fence: &Fence) -> Result<(), Error> {
        self.backend.write(record, Some(fence))
    }

    /// Append `child` to `parent`'s children.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn add_child(&self, parent: &str, child: &str) -> Result<(), Error> {
        self.backend.add_child(parent, child)
    }

    pub fn read(&self, name: &str) -> Result<Record, Error> {
        if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
            return Err(Error::UnknownBranch(name.to_owned()));
        }
        self.backend
            .read(name)?
            .ok_or_else(|| Error::UnknownBranch(name.to_owned()))
    }

    /// Every created branch, oldest first.
    pub fn list(&self) -> Result<Vec<Record>, Error> {
        self.backend.list()
    }

    /// Delete a branch's record and what the engine kept for its turns,
    /// whoever holds its lease; [`Store::delete_held`] outside tests.
    #[cfg(test)]
    pub fn delete(&self, name: &str) -> Result<(), Error> {
        self.backend.delete(name)
    }

    /// Delete a branch whose lease `lease` holds: its lease goes with its
    /// record, so the lease is spent, not released (releasing it after
    /// would find no lease and warn that another engine took it). On an
    /// error nothing was deleted, and the lease is released as usual.
    pub fn delete_held(&self, lease: Lease) -> Result<(), Error> {
        let mut lease = lease;
        // Stopped first, so no renewal races the delete.
        lease.heartbeat.take();
        self.backend.delete(&lease.fence.branch)?;
        lease.done = true;
        Ok(())
    }

    /// Write `record` and take its branch's lease for a new turn. Refused
    /// with [`Error::Running`] while a live engine holds it; a lease left by
    /// a stopped engine is reported as [`Taken::Stale`] for recovery first.
    pub fn acquire(&self, record: &Record) -> Result<Taken, Error> {
        match self.backend.acquire(record, &self.owner, LEASE_TTL)? {
            Acquired::Granted(fence) => Ok(Taken::Granted(Lease::new(self.clone(), fence))),
            Acquired::Held(row) => match row.stale(now_ms()) {
                Some(_) => Ok(Taken::Stale),
                None => Err(Error::Running(record.info.name.clone())),
            },
        }
    }

    /// Ask the branch's running turn to stop, on behalf of `by`. False when
    /// no turn is running.
    pub fn request_cancel(&self, name: &str, by: &str, subtree: bool) -> Result<bool, Error> {
        self.backend.request_cancel(name, by, subtree)
    }

    /// Append an event; wakes waiting readers.
    pub fn append(
        &self,
        name: &str,
        event: &RecordedEvent,
        fence: Option<&Fence>,
    ) -> Result<u64, Error> {
        let seq = self.backend.append(name, event, fence)?;
        self.notify();
        Ok(seq)
    }

    /// Wake readers waiting in this process.
    pub fn notify(&self) {
        let mut appended = self.signal.appended.lock_recovering("appended");
        *appended += 1;
        self.signal.changed.notify_all();
    }

    /// Wait until `ready` returns something or `timeout` passes. Appends in
    /// this process wake the wait at once; another process's are seen
    /// within [`POLL`].
    pub fn wait<T>(
        &self,
        timeout: Duration,
        mut ready: impl FnMut() -> Result<Option<T>, Error>,
    ) -> Result<Option<T>, Error> {
        // A timeout too long to add to the clock (`Duration::MAX`) is a wait
        // with no deadline.
        let deadline = Instant::now().checked_add(timeout);
        loop {
            let seen = *self.signal.appended.lock_recovering("appended");
            if let Some(found) = ready()? {
                return Ok(Some(found));
            }
            let now = Instant::now();
            let remaining = match deadline {
                Some(deadline) if now >= deadline => return Ok(None),
                Some(deadline) => POLL.min(deadline - now),
                None => POLL,
            };
            let appended = self.signal.appended.lock_recovering("appended");
            if *appended == seen {
                drop(
                    self.signal
                        .changed
                        .wait_timeout_recovering(appended, remaining, "changed"),
                );
            }
        }
    }
}

/// What [`Store::acquire`] got.
pub(crate) enum Taken {
    Granted(Lease),
    /// Held by an engine that stopped; recover the branch, then retry.
    Stale,
}

/// A held lease, renewed by a [`Heartbeat`] while held. Released when
/// dropped unless finished first, so a turn that never ran does not keep
/// its branch.
pub(crate) struct Lease {
    store: Store,
    fence: Fence,
    heartbeat: Option<Heartbeat>,
    done: bool,
}

impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease").field("fence", &self.fence).finish()
    }
}

impl Lease {
    pub fn new(store: Store, fence: Fence) -> Lease {
        let heartbeat = Heartbeat::start(&store, &fence);
        Lease {
            store,
            fence,
            heartbeat: Some(heartbeat),
            done: false,
        }
    }

    pub fn fence(&self) -> &Fence {
        &self.fence
    }

    /// Whether a renewal was refused because another engine took the
    /// lease.
    pub fn lost(&self) -> bool {
        self.heartbeat.as_ref().is_some_and(Heartbeat::lost)
    }

    /// Write the final record and event and release the lease,
    /// atomically; wakes waiting readers.
    pub fn finish(
        mut self,
        record: Option<&Record>,
        event: Option<&RecordedEvent>,
    ) -> Result<(), Error> {
        self.done = true;
        self.heartbeat.take();
        self.store.backend.finish(&self.fence, record, event)?;
        if event.is_some() {
            self.store.notify();
        }
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.heartbeat.take();
        if !self.done {
            // A lease dropped without finishing (an early return, a panic)
            // still has to be released, and nothing here can return the
            // error: so it is logged, and the lease expires on its own.
            branchyard_support::best_effort(
                "release a lease dropped without finish",
                self.store.backend.finish(&self.fence, None, None),
            );
        }
    }
}

/// Renews a lease every [`HEARTBEAT`] until dropped. `lost` turns true
/// once a renewal is refused because the lease moved on.
pub(crate) struct Heartbeat {
    stop: Arc<(Mutex<bool>, Condvar)>,
    lost: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Heartbeat {
    pub fn start(store: &Store, fence: &Fence) -> Heartbeat {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (store, fence) = (store.clone(), fence.clone());
        let (stopping, losing) = (stop.clone(), lost.clone());
        let thread = std::thread::Builder::new()
            .name(format!("by-lease-{}", fence.branch))
            .spawn(move || {
                let (flag, wake) = &*stopping;
                let mut stopped = flag.lock_recovering("heartbeat stop flag");
                loop {
                    let deadline = Instant::now() + HEARTBEAT;
                    while !*stopped && Instant::now() < deadline {
                        let left = deadline.saturating_duration_since(Instant::now());
                        stopped = wake.wait_timeout_recovering(stopped, left, "wake").0;
                    }
                    if *stopped {
                        return;
                    }
                    match store.backend.renew(&fence, LEASE_TTL) {
                        Ok(()) => {}
                        Err(Error::Fenced(_)) => {
                            losing.store(true, Ordering::Release);
                            return;
                        }
                        // A busy or unreadable store: try again next beat,
                        // while the lease still has time.
                        Err(_) => {}
                    }
                }
            })
            .ok();
        Heartbeat { stop, lost, thread }
    }

    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        let (flag, wake) = &*self.stop;
        *flag.lock_recovering("heartbeat stop flag") = true;
        wake.notify_all();
        if let Some(thread) = self.thread.take() {
            branchyard_support::join_reporting("lease heartbeat", thread);
        }
    }
}

#[cfg(test)]
mod drop_tests {
    use super::*;
    use branchyard_support::testing::{capture, Level};

    fn record(name: &str) -> Record {
        serde_json::from_value(serde_json::json!({
            "info": {
                "name": name, "git_branch": format!("by/{name}"), "worktree": "/w",
                "prompt": "p", "harness": "h", "profile": "p", "session": null,
                "parent": null, "base": "b", "candidate": null,
                "status": {"state": "running"}, "turns": 0, "cost_usd": null,
                "created_at": 0
            },
            "created_ms": 0, "check": null, "command": null, "home": null,
            "cost_baseline": null
        }))
        .unwrap()
    }

    fn lease_on(store: &Store, name: &str) -> Lease {
        assert!(store.reserve(name).unwrap());
        match store.acquire(&record(name)).unwrap() {
            Taken::Granted(lease) => lease,
            Taken::Stale => panic!("a fresh branch has no stale lease"),
        }
    }

    #[test]
    fn a_lease_dropped_without_finish_releases_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let lease = lease_on(&store, "quiet");
        let (_, events) = capture(|| drop(lease));
        assert!(events.is_empty(), "{events:?}");
        // Released: the branch can be leased again.
        assert!(matches!(
            store.acquire(&record("quiet")).unwrap(),
            Taken::Granted(_)
        ));
    }

    #[test]
    fn a_failed_release_on_drop_is_logged_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let lease = lease_on(&store, "fenced");
        // The branch's record and lease go away under the holder, as when
        // another engine took the lease over: its release is now refused.
        store.delete("fenced").unwrap();
        let (_, events) = capture(|| drop(lease));
        let warnings: Vec<_> = events.iter().filter(|e| e.level == Level::WARN).collect();
        assert_eq!(warnings.len(), 1, "{events:?}");
        assert!(
            warnings[0]
                .text
                .contains("release a lease dropped without finish"),
            "{}",
            warnings[0].text
        );
    }
}
