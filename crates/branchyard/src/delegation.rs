//! Delegation: a branch creating and coordinating child branches.
//!
//! There is one set of operations, [`Local`]'s, and one authority model.
//! Every surface reaches them: the SDK's [`Delegate`], `by spawn` and its
//! siblings, the Python module (which runs `by`), and the MCP server. Inside
//! a harness they go through the broker to the engine that runs the turn,
//! because children run on that engine's threads.
//!
//! Authority is a branch, never a name in a request. A [`Delegate`] acts as
//! exactly one branch and only on that branch's descendants, found through
//! the `children` the engine recorded. A harness proves which branch it is
//! with a token the engine issues when the turn starts and revokes when it
//! ends ([`Yard::as_branch`], [`Delegate::from_env`]); code that owns the
//! process, such as `by` run by a person, acts as a branch directly
//! ([`crate::Branch::delegate`]). Either way the same envelope applies:
//!
//! - Depth, width and harnesses only shrink from parent to child.
//! - A child's cost limit must fit in what its parent has left, counting
//!   the parent's own spend and every other child's reservation. A child
//!   reserves its whole limit, or what its subtree has spent if that is
//!   more, so a subtree never spends past its root's limit through
//!   children that report their costs.
//! - A child runs under its parent's permission policy, narrowed by any
//!   denials the parent adds; nothing a child asks for widens it.
//!
//! Not guaranteed: cost limits cannot hold for harnesses that report no
//! cost, and in local mode a harness running as your user can read the
//! token files in `.branchyard/` and act as any branch with a running
//! turn. Tokens stop honest mistakes, not a hostile harness.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use branchyard_harness::profiles::{self, Profile};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::broker::Remote;
use crate::engine::{self, Turn};
use crate::graph::{
    self, After, Binding, Dependency, DependencyRef, Graph, GraphApplied, GraphCommit, GraphEdit,
    SpawnSpec, MAX_EDITS,
};
use crate::operations::Capability;
use crate::projection::{lock, same_token, ENV_BRANCH, ENV_ROOT, ENV_TOKEN};
use crate::record::{self, Recorder};
use crate::recover;
use crate::run::{self, NewBranch, Prepared};
use crate::seats::{Seat, Seats};
use crate::state::{Record, Store};
use crate::{
    git, harness, inbox, integrate, names, Activity, BranchInfo, BranchStatus, Budget,
    CandidateInfo, Error, Event, Merged, MergedAll, Message, MessageKind, Policy, RecordedEvent,
    Rule, SharedCheck, Steer, SteerState, TaskOptions, Yard,
};

/// How long `steer` waits for the input to be delivered.
const STEER_WAIT: Duration = Duration::from_secs(10);

/// Most events one `events` call returns.
const EVENTS_MAX: usize = 200;
/// Characters of the last message `inspect` returns.
const LAST_MESSAGE_MAX: usize = 4000;
/// Cost comparisons tolerate float rounding of this much.
const EPSILON_USD: f64 = 1e-9;
/// How often [`Delegate::wait`] looks.
const WAIT_POLL: Duration = Duration::from_millis(100);
/// How often a wait for a turn in another process looks again for a
/// stopped engine to recover, and for children on this process's threads.
const SETTLE_EVERY: Duration = Duration::from_secs(1);

/// What a branch may delegate. Children get an envelope at most as wide as
/// their parent's, one level shallower.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Levels of descendants below this branch: 1 allows children but no
    /// grandchildren; 0 allows no children.
    pub max_depth: u32,
    /// Children this branch may have, counting finished ones until they
    /// are removed.
    pub max_children: u32,
    /// Harness or profile IDs children may run. Empty: only this branch's
    /// own profile.
    pub harnesses: Vec<String>,
    /// Times the branch's next turn may start on its own after its turn
    /// ended while children it delegated were still running, once they
    /// have settled (`docs/delegation.md`, "Waiting on children"). 0 turns
    /// this off (`--no-wake`): such a turn then ends as it is. A child's is
    /// at most its parent's. Absent in a stored envelope:
    /// [`DEFAULT_MAX_WAKES`].
    #[serde(
        default = "default_max_wakes",
        skip_serializing_if = "is_default_max_wakes"
    )]
    pub max_wakes: u32,
}

/// The automatic wakes an envelope allows unless it says otherwise.
pub const DEFAULT_MAX_WAKES: u32 = 8;

fn default_max_wakes() -> u32 {
    DEFAULT_MAX_WAKES
}

fn is_default_max_wakes(wakes: &u32) -> bool {
    *wakes == DEFAULT_MAX_WAKES
}

impl Default for Envelope {
    /// Children only, at most four, on the parent's own profile, woken
    /// when they settle at most [`DEFAULT_MAX_WAKES`] times.
    fn default() -> Self {
        Envelope {
            max_depth: 1,
            max_children: 4,
            harnesses: Vec::new(),
            max_wakes: DEFAULT_MAX_WAKES,
        }
    }
}

impl Envelope {
    /// The default envelope with `max_depth` levels.
    pub fn depth(max_depth: u32) -> Self {
        Envelope {
            max_depth,
            ..Envelope::default()
        }
    }

    /// Never start the branch's next turn on its own when its children
    /// settle (`max_wakes` 0): a turn that ends while they run ends as it
    /// is, and the caller waits for them (`by wait`, [`Delegate::wait`]).
    pub fn no_wake(mut self) -> Self {
        self.max_wakes = 0;
        self
    }

    pub fn harnesses<I, S>(mut self, harnesses: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.harnesses = harnesses.into_iter().map(Into::into).collect();
        self
    }

    fn allows(&self, profile: &Profile, own: &Profile) -> bool {
        if self.harnesses.is_empty() {
            return profile.id == own.id;
        }
        self.harnesses
            .iter()
            .any(|h| h == profile.id || h == profile.harness)
    }

    /// The harnesses or profiles children may run, as a refusal names
    /// them: the list, or the branch's own profile by its label.
    fn allowed_text(&self, own: &Profile) -> String {
        match self.harnesses.is_empty() {
            true => format!("{} only (its own)", own.label()),
            false => self.harnesses.join(", "),
        }
    }

    /// What `--harness` its children may name: the list as given, or,
    /// when it is empty, the branch's own `profile` alone.
    pub fn allowed(&self, profile: &str) -> Vec<String> {
        match self.harnesses.is_empty() {
            true => vec![profile.to_owned()],
            false => self.harnesses.clone(),
        }
    }

    /// A child's envelope: one level shallower, narrowed by `request`.
    fn child(&self, request: &Spawn, own: &Profile) -> Result<Envelope, Error> {
        let depth = self.max_depth.saturating_sub(1);
        let harnesses = match &request.harnesses {
            None => self.harnesses.clone(),
            Some(list) => {
                for id in list {
                    let profile = harness::select(Some(id))?;
                    if !self.allows(profile, own) {
                        return Err(Error::Denied(format!(
                            "a child may not be allowed {id}; this branch may delegate only to {}",
                            self.allowed_text(own)
                        )));
                    }
                }
                list.clone()
            }
        };
        Ok(Envelope {
            max_depth: request.max_depth.map_or(depth, |d| d.min(depth)),
            max_children: request
                .max_children
                .map_or(self.max_children, |c| c.min(self.max_children)),
            harnesses,
            max_wakes: self.max_wakes,
        })
    }
}

/// Limits a parent put on a delegated child. They bound every turn of the
/// child, whoever sends it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Limits {
    pub max_usd: Option<f64>,
    pub max_turns: Option<u32>,
    pub max_duration_ms: Option<u64>,
}

/// A branch's delegation record.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Grant {
    pub envelope: Envelope,
    /// Tool patterns denied before the policy is consulted.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Set for delegated children only.
    #[serde(default)]
    pub limits: Option<Limits>,
    /// For a branch in a rig: its seat and the seats it may spawn. Such a
    /// branch spawns only by seat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seats: Option<Seats>,
}

impl Grant {
    pub fn root(envelope: Envelope) -> Grant {
        Grant {
            envelope,
            deny: Vec::new(),
            limits: None,
            seats: None,
        }
    }

    pub fn can_spawn(&self) -> bool {
        self.envelope.max_depth > 0
    }
}

/// A child to create. Unset fields default to the parent's, within its
/// envelope.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spawn {
    pub prompt: String,
    /// Harness or profile ID; defaults to the parent's profile.
    pub harness: Option<String>,
    /// Branch name; defaults to a slug of the prompt, made unique.
    pub name: Option<String>,
    /// Revision to start from. Defaults to the parent's current work: its
    /// uncommitted changes are committed to its branch first.
    pub base: Option<String>,
    /// The child's limits. `max_usd` is required when the parent has a cost
    /// limit, and must fit in what it has left. Turns and duration default
    /// to the parent's and may not exceed them.
    pub budget: Budget,
    /// The child's merge check; defaults to the parent's.
    pub check: Option<Vec<String>>,
    /// Narrow the child's envelope below the default of one level fewer.
    pub max_depth: Option<u32>,
    pub max_children: Option<u32>,
    /// Harnesses the child may delegate to; each must be allowed to the
    /// parent.
    pub harnesses: Option<Vec<String>>,
    /// Tool patterns the child's policy denies outright, like
    /// [`Policy::deny`].
    pub deny: Vec<String>,
    /// A seat of the parent's rig to fill: the seat sets the child's
    /// harness, check, isolation and provisioning, and its limits and
    /// denials, which the fields above may only narrow. A branch in a rig
    /// must name one of the seats its own seat delegates to; any other
    /// branch may not name one. Unset `name` defaults to
    /// `<parent>-<seat>`.
    pub seat: Option<String>,
    /// Siblings (other children of the same parent) this child waits for:
    /// it is created `waiting` and starts its first turn when each has
    /// settled; see `docs/graph.md`.
    pub depends_on: Vec<String>,
    /// When each of `depends_on` counts as done.
    pub after: After,
    /// Scratch areas the child is bound to for every turn.
    pub bindings: Vec<Binding>,
    /// The child's connector grant (`--connector`). It is always narrowed
    /// to the parent's grant, an entry the parent allows nothing of being
    /// refused. Unset: its seat's, else its parent's. See
    /// `docs/connectors.md`.
    pub connectors: Option<Vec<crate::connectors::GrantEntry>>,
    /// Plan first: the child's first turn runs read-only and proposes a
    /// plan, which is escalated to this branch's inbox; it changes nothing
    /// until this branch (or a person) approves it. See
    /// `docs/plans-and-goals.md`.
    pub plan: bool,
}

impl Spawn {
    pub fn new(prompt: impl Into<String>) -> Spawn {
        Spawn {
            prompt: prompt.into(),
            ..Spawn::default()
        }
    }

    /// The `spawn` tool's arguments.
    fn arguments(&self) -> Value {
        serde_json::to_value(SpawnSpec::from_spawn(self)).unwrap_or(Value::Null)
    }
}

/// A child's limits as the tools show them.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChildBudget {
    pub max_usd: Option<f64>,
    pub max_turns: Option<u32>,
    /// Per turn.
    pub max_minutes: Option<f64>,
}

impl From<&Budget> for ChildBudget {
    fn from(budget: &Budget) -> Self {
        ChildBudget {
            max_usd: budget.max_usd,
            max_turns: budget.max_turns,
            max_minutes: budget.max_duration.map(|d| d.as_secs_f64() / 60.0),
        }
    }
}

impl ChildBudget {
    pub(crate) fn to_budget(&self) -> Result<Budget, Error> {
        let max_duration = match self.max_minutes {
            None => None,
            Some(m) if m.is_finite() && m > 0.0 => Duration::try_from_secs_f64(m * 60.0).ok(),
            Some(m) => {
                return Err(Error::Denied(format!(
                    "max_minutes must be a positive number, not {m}"
                )))
            }
        };
        Ok(Budget {
            max_usd: self.max_usd,
            max_turns: self.max_turns,
            max_duration,
            ..Budget::default()
        })
    }
}

/// A child that was created and started.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Spawned {
    pub name: String,
    pub git_branch: String,
    pub harness: String,
    pub profile: String,
    pub base: String,
    pub depth: u32,
    pub status: BranchStatus,
    pub budget: ChildBudget,
    /// The seat it fills, for a child spawned by seat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat: Option<String>,
    /// The siblings it waits for, for a child with prerequisites; it is
    /// `waiting` (or `blocked`) until they settle, and `base` is empty
    /// until it starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// The check that must pass on its merge when it is integrated: the
    /// one the spawn gave, else its parent's. Omitted when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Vec<String>>,
    /// `check` is its parent's, inherited because the spawn gave none.
    /// Siblings under a parent's whole-suite check pass it only together:
    /// integrate them together ([`Delegate::integrate_all`]). Omitted when
    /// false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check_inherited: bool,
}

/// A descendant whose next turn was started.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sent {
    pub name: String,
    pub status: BranchStatus,
}

/// The branches a cancel asked to stop.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cancelled {
    pub cancelled: Vec<String>,
    /// When nothing was running: what the branch is, and the command that
    /// does what a cancel cannot (`by discard` sets a settled child aside).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Cancelled {
    /// What a cancel of `branch`, now `status`, stopped.
    pub fn of(cancelled: Vec<String>, branch: &str, status: &BranchStatus) -> Cancelled {
        let note = match (cancelled.is_empty(), status) {
            (false, _) => None,
            (true, BranchStatus::Ready) => Some(format!(
                "{branch} is not running: it ended ready, so there is nothing to cancel. \
                 Keep its work with `by integrate {branch}`, or set it aside with \
                 `by discard {branch} --reason TEXT`, which frees its slot"
            )),
            (true, BranchStatus::Merged { target, .. }) => Some(format!(
                "{branch} is not running: it was merged into {target}. `by rm {branch}` \
                 removes its worktree"
            )),
            (true, BranchStatus::Discarded { .. }) => {
                Some(format!("{branch} is not running: it was already discarded"))
            }
            (true, BranchStatus::Running) => Some(format!("{branch} was already asked to stop")),
            (true, status) => Some(format!(
                "{branch} is not running ({}), so there is nothing to cancel. Continue it \
                 with `by send {branch} \"<prompt>\"`, or set it aside with \
                 `by discard {branch} --reason TEXT`, which frees its slot",
                status_word(status)
            )),
        };
        Cancelled { cancelled, note }
    }
}

/// A status's `state`, as JSON names it.
fn status_word(status: &BranchStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v["state"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// A branch's subtree.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Children {
    pub branch: String,
    /// Every descendant, oldest first.
    pub descendants: Vec<BranchInfo>,
}

/// A branch's own inbox: every message addressed to it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    pub branch: String,
    pub messages: Vec<Message>,
}

/// A question sent, and its answer if one arrived within the wait.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Asked {
    pub message: Message,
    /// `None` when asked without `--wait`, or the wait passed with no
    /// answer yet; ask again, or `inbox` to check later.
    pub answer: Option<Message>,
}

/// A branch as a delegating parent sees it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inspection {
    pub name: String,
    pub status: BranchStatus,
    pub harness: String,
    pub profile: String,
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub depth: u32,
    pub turns: u32,
    pub candidate: Option<CandidateInfo>,
    /// The branch's own cost estimate, while a turn runs too (from the
    /// harness's usage as it comes); `None` when its harness has reported
    /// none.
    pub cost_usd: Option<f64>,
    /// Its own and its descendants' reported costs, removed descendants'
    /// included. Unreported costs count as zero here.
    pub subtree_cost_usd: f64,
    /// The cost limit its turns run under, when there is one.
    pub max_usd: Option<f64>,
    /// What is left of `max_usd` after its own spend, what its live
    /// children hold (`reserved_usd`) and what its settled and removed
    /// children spent (`settled_children_usd`): what it can still spend or
    /// grant.
    pub remaining_usd: Option<f64>,
    /// What its live children (running, waiting, blocked or awaiting plan
    /// approval) hold of its budget: each one's whole limit, or what its
    /// subtree spent if that is more, since it may spend that much without
    /// asking. A child that settles (ready, merged, failed, ...) holds only
    /// what it spent, until it is sent something again.
    #[serde(default)]
    pub reserved_usd: f64,
    /// How many live children hold `reserved_usd`.
    #[serde(default)]
    pub reserving_children: u32,
    /// What its settled and removed children's subtrees spent.
    #[serde(default)]
    pub settled_children_usd: f64,
    pub envelope: Option<Envelope>,
    /// The check its merge must pass when it is integrated. Omitted when
    /// it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Vec<String>>,
    /// `check` is its parent's, inherited because its spawn gave none.
    /// Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check_inherited: bool,
    /// Its siblings that share `check` and may still be integrated (ready,
    /// or not settled yet): a whole-suite check passes only with all of
    /// them, so they are integrated together. Omitted when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check_shared_with: Vec<String>,
    /// What its children may run, as `--harness` names it: the envelope's
    /// `harnesses`, or its own profile when that list is empty (which
    /// means "its own only", not "none"). Empty without an envelope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_harnesses: Vec<String>,
    /// The harness's text since the branch's last prompt, truncated from
    /// the front.
    pub last_message: String,
    /// The rig seat the branch occupies, if it is in a rig.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat: Option<String>,
    /// The seats it may spawn, if it is in a rig.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seats: Vec<String>,
    /// See [`crate::BranchInfo::stalled`].
    #[serde(default)]
    pub stalled: bool,
    /// The branch's own graph revision: bumped by each graph proposal it
    /// commits and each child it spawns. Omitted while 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub graph_revision: u64,
    /// The siblings this branch waits for, or waited for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<Dependency>,
    /// The scratch areas it is bound to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Binding>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Recorded events from `cursor` on.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventPage {
    pub branch: String,
    pub events: Vec<RecordedEvent>,
    /// Pass back to continue after the last event returned.
    pub next_cursor: usize,
    /// Events recorded so far.
    pub total: usize,
}

/// What a wait for several branches found ([`Delegate::wait_for`],
/// `by wait`, the `wait` tool).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Waited {
    /// The branches waited for that have settled (not running, waiting
    /// for prerequisites or waiting on children), as inspected when the
    /// wait returned, in the order asked.
    pub settled: Vec<Inspection>,
    /// Those that have not.
    pub pending: Vec<String>,
    /// The timeout passed before the wait was satisfied. Omitted when
    /// false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
}

/// Acts as one branch, on that branch and its descendants only.
///
/// In the process that runs the branch's turn, or for SDK code that owns
/// the yard, it calls the operations directly; elsewhere, such as inside
/// the harness, it reaches that process through the broker. Both run the
/// same operations and give the same answers.
pub struct Delegate {
    branch: String,
    via: Via,
}

enum Via {
    Local(Box<Local>),
    Remote(Mutex<Remote>),
}

impl fmt::Debug for Delegate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let via = match self.via {
            Via::Local(_) => "local",
            Via::Remote(_) => "broker",
        };
        f.debug_struct("Delegate")
            .field("branch", &self.branch)
            .field("via", &via)
            .finish()
    }
}

impl Delegate {
    /// Act as the branch whose running turn was issued `token`, reaching
    /// the engine under `root` that runs it.
    pub fn connect(root: impl AsRef<Path>, token: &str) -> Result<Delegate, Error> {
        let (branch, remote) = Remote::find(root.as_ref(), token)?;
        Ok(Delegate {
            branch,
            via: Via::Remote(Mutex::new(remote)),
        })
    }

    /// Act as the branch the engine started this harness for, from
    /// `BRANCHYARD_ROOT` and `BRANCHYARD_DELEGATION`. When
    /// `BRANCHYARD_BRANCH` is set too, the token must belong to it.
    pub fn from_env() -> Result<Delegate, Error> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let (Some(root), Some(token)) = (var(ENV_ROOT), var(ENV_TOKEN)) else {
            return Err(Error::Denied(format!(
                "not inside a delegating turn: {ENV_ROOT} and {ENV_TOKEN} are not both set"
            )));
        };
        let delegate = Delegate::connect(root, &token)?;
        match var(ENV_BRANCH) {
            Some(branch) if branch != delegate.branch => Err(Error::Denied(format!(
                "{ENV_TOKEN} was issued to {}, not {branch}",
                delegate.branch
            ))),
            _ => Ok(delegate),
        }
    }

    /// The branch this acts as.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Call one operation with the JSON arguments its tool takes, and
    /// return its JSON result. `docs/delegation.md` lists the tools and
    /// their shapes.
    pub fn call(&self, tool: &str, arguments: Value) -> Result<Value, Error> {
        match &self.via {
            Via::Local(local) => dispatch(local, tool, arguments),
            Via::Remote(remote) => lock(remote).call(tool, arguments),
        }
    }

    fn typed<T: DeserializeOwned>(&self, tool: &str, arguments: Value) -> Result<T, Error> {
        let value = self.call(tool, arguments)?;
        serde_json::from_value(value)
            .map_err(|e| Error::State(format!("unreadable {tool} result: {e}")))
    }

    /// Create a child branch and start its first turn on a thread of the
    /// engine running this branch. Returns once the child has started.
    pub fn spawn(&self, spawn: Spawn) -> Result<Spawned, Error> {
        match &self.via {
            Via::Local(local) => local.spawn(&spawn),
            Via::Remote(_) => self.typed("spawn", spawn.arguments()),
        }
    }

    /// A branch's state, for this branch or a descendant.
    pub fn inspect(&self, branch: &str) -> Result<Inspection, Error> {
        match &self.via {
            Via::Local(local) => local.inspect(branch),
            Via::Remote(_) => self.typed("inspect", json!({ "branch": branch })),
        }
    }

    /// Up to `limit` recorded events of this branch or a descendant, from
    /// `cursor`, or the most recent without one.
    pub fn events(
        &self,
        branch: &str,
        cursor: Option<usize>,
        limit: usize,
    ) -> Result<EventPage, Error> {
        match &self.via {
            Via::Local(local) => local.events(branch, cursor, limit),
            Via::Remote(_) => self.typed(
                "events",
                json!({"branch": branch, "cursor": cursor, "limit": limit}),
            ),
        }
    }

    /// Continue a descendant's session with `prompt`. Refused while it is
    /// running a turn; returns once the new turn has started.
    pub fn send(&self, branch: &str, prompt: &str) -> Result<Sent, Error> {
        match &self.via {
            Via::Local(local) => local.send(branch, prompt),
            Via::Remote(_) => self.typed("send", json!({"branch": branch, "prompt": prompt})),
        }
    }

    /// [`Delegate::send`], then wait for the turn it started to settle, for
    /// up to `timeout`. Race-free: a branch's lease admits one turn at a
    /// time, so once this send's turn is running, nothing but its own end
    /// (or recovery, for a stopped engine) makes the branch stop running;
    /// there is no later turn to be confused with. Refused with
    /// [`Error::Running`] if `branch` is already running when asked (no
    /// steer channel to reach it yet).
    pub fn send_and_wait(
        &self,
        branch: &str,
        prompt: &str,
        timeout: Duration,
    ) -> Result<Inspection, Error> {
        self.send(branch, prompt)?;
        self.wait(branch, timeout)
    }

    /// Approve a descendant's plan, which awaits approval, as proposed or
    /// as `edited`, and start the turn that carries it out; see
    /// `docs/plans-and-goals.md`. Returns once that turn has started.
    pub fn approve_plan(&self, branch: &str, edited: Option<&str>) -> Result<Sent, Error> {
        match &self.via {
            Via::Local(local) => local.approve_plan(branch, edited),
            Via::Remote(_) => {
                self.typed("approve_plan", json!({"branch": branch, "edited": edited}))
            }
        }
    }

    /// Answer a descendant's approval ask (`docs/effects.md`): allow or
    /// deny the tool or connector call its turn waits on, or a staged
    /// effect it holds. Only an ancestor may; it is recorded as answered
    /// by this branch, through `parent`.
    pub fn answer_approval(
        &self,
        id: &str,
        allow: bool,
        reason: Option<&str>,
    ) -> Result<crate::effects::ApprovalAsk, Error> {
        match &self.via {
            Via::Local(local) => local.answer_approval(id, allow, reason),
            Via::Remote(_) => self.typed(
                "answer_approval",
                json!({"id": id, "allow": allow, "reason": reason}),
            ),
        }
    }

    /// Reject a descendant's plan: the descendant fails, or with `replan`,
    /// it plans again with `reason` (that turn started when this returns).
    pub fn reject_plan(
        &self,
        branch: &str,
        reason: Option<&str>,
        replan: bool,
    ) -> Result<Sent, Error> {
        match &self.via {
            Via::Local(local) => local.reject_plan(branch, reason, replan),
            Via::Remote(_) => self.typed(
                "reject_plan",
                json!({"branch": branch, "reason": reason, "replan": replan}),
            ),
        }
    }

    /// Merge a descendant's candidate into this branch's own git branch
    /// (`by/<name>`, never the user's branches), after the descendant's
    /// check passes on the exact merge. This branch's uncommitted work is
    /// committed first, and its worktree moves to the merge.
    ///
    /// A candidate this branch already contains (say, brought in by a
    /// sibling that merged it) is not an error: the descendant is recorded
    /// as merged through the commit that brought it in, and the result
    /// says `already`.
    pub fn integrate(&self, branch: &str) -> Result<Merged, Error> {
        match &self.via {
            Via::Local(local) => local.integrate(branch),
            Via::Remote(_) => self.typed("propose_integration", json!({ "branch": branch })),
        }
    }

    /// Integrate several descendants together, all or none: their
    /// candidates are merged in the order given in one temporary worktree,
    /// each distinct check of theirs runs once on the result, and this
    /// branch moves once. For siblings that share one test suite, which
    /// none passes alone. A conflict names the descendant that conflicted
    /// and those merged before it ([`Error::ConflictBetween`]). Every
    /// descendant is recorded as merged.
    pub fn integrate_all(&self, branches: &[&str]) -> Result<MergedAll, Error> {
        let names: Vec<String> = branches.iter().map(|b| (*b).to_owned()).collect();
        match &self.via {
            Via::Local(local) => local.integrate_all(&names),
            Via::Remote(_) => self.typed("propose_integration", json!({ "branches": names })),
        }
    }

    /// Deliver `text` into a descendant's running turn without
    /// interrupting it, as input from this branch; see
    /// [`crate::Yard::steer_as`]. Waits up to ten seconds for the engine
    /// running that turn to deliver it, and returns what became of it:
    /// delivered, accepted, refused with the reason, or still pending.
    /// Refused up front when the descendant's harness cannot take input
    /// mid-turn ([`Error::Unsupported`]) or is not running a turn
    /// ([`Error::NotRunning`]).
    pub fn steer(&self, branch: &str, text: &str) -> Result<Steer, Error> {
        match &self.via {
            Via::Local(local) => local.steer(branch, text),
            Via::Remote(_) => self.typed("steer", json!({"branch": branch, "text": text})),
        }
    }

    /// Stop a descendant's running turn and every turn running below it.
    pub fn cancel(&self, branch: &str) -> Result<Cancelled, Error> {
        match &self.via {
            Via::Local(local) => local.cancel(branch),
            Via::Remote(_) => self.typed("cancel", json!({ "branch": branch })),
        }
    }

    /// Set a settled descendant aside: it ends `discarded` with `reason`,
    /// runs no more turns, is never integrated, keeps its record and cost,
    /// and frees its slot in this branch's `max_children`. Refused while it
    /// runs (cancel it first) and once it was merged. Returns it as
    /// [`Delegate::inspect`] shows it.
    pub fn discard(&self, branch: &str, reason: Option<&str>) -> Result<Inspection, Error> {
        match &self.via {
            Via::Local(local) => local.discard(branch, reason),
            Via::Remote(_) => self.typed("discard", json!({"branch": branch, "reason": reason})),
        }
    }

    /// This branch's descendants.
    pub fn children(&self) -> Result<Children, Error> {
        match &self.via {
            Via::Local(local) => local.children(),
            Via::Remote(_) => self.typed("children", json!({})),
        }
    }

    /// Apply a graph proposal to this branch's children: spawn children,
    /// some waiting for others, and add or remove dependencies between
    /// children that have not started, all or nothing. Refused with
    /// [`Error::StaleRevision`] unless `expected_revision` is this branch's
    /// current graph revision ([`Delegate::graph`]). Children with no
    /// prerequisites start before this returns, as with
    /// [`Delegate::spawn`]; see `docs/graph.md`.
    pub fn apply_graph(
        &self,
        edits: Vec<GraphEdit>,
        expected_revision: u64,
    ) -> Result<GraphApplied, Error> {
        match &self.via {
            Via::Local(local) => local.apply_graph(&edits, expected_revision),
            Via::Remote(_) => self.typed(
                "apply_graph",
                json!({"edits": edits, "expected_revision": expected_revision}),
            ),
        }
    }

    /// The graph of this branch or a descendant: its children, the
    /// dependencies among them, and its revision.
    pub fn graph(&self, branch: &str) -> Result<Graph, Error> {
        match &self.via {
            Via::Local(local) => local.graph(branch),
            Via::Remote(_) => self.typed("graph", json!({ "branch": branch })),
        }
    }

    /// Publish `path` as a new immutable artifact of this branch; see
    /// `docs/storage.md`.
    ///
    /// `media_type` is recorded with it (default
    /// `application/octet-stream`); its `digest` is the blake3 hash of its
    /// bytes, in lower-case hex.
    pub fn publish_artifact(
        &self,
        path: &Path,
        name: Option<String>,
        media_type: Option<String>,
        labels: std::collections::BTreeMap<String, String>,
    ) -> Result<crate::ArtifactRef, Error> {
        match &self.via {
            Via::Local(local) => local.publish_artifact(path, name, media_type, labels),
            Via::Remote(_) => self.typed(
                "publish_artifact",
                json!({"path": path, "name": name, "media_type": media_type, "labels": labels}),
            ),
        }
    }

    /// Every artifact this branch may read.
    pub fn artifacts(&self) -> Result<Vec<crate::ArtifactRef>, Error> {
        match &self.via {
            Via::Local(local) => local.list_artifacts(),
            Via::Remote(_) => self.typed("list_artifacts", json!({})),
        }
    }

    /// Copy artifact `id`'s bytes to `out` for this branch.
    pub fn read_artifact(&self, id: &str, out: &Path) -> Result<crate::ArtifactRef, Error> {
        match &self.via {
            Via::Local(local) => local.get_artifact(id, out),
            Via::Remote(_) => self.typed("get_artifact", json!({"id": id, "out": out})),
        }
    }

    /// Share artifact `id` with branch `to`.
    pub fn share_artifact(&self, id: &str, to: &str) -> Result<(), Error> {
        match &self.via {
            Via::Local(local) => local.share_artifact(id, to),
            Via::Remote(_) => self
                .typed::<Value>("share_artifact", json!({"id": id, "to": to}))
                .map(|_| ()),
        }
    }

    /// Export the artifacts in `ids` into a portable bundle at `out`; see
    /// `docs/storage.md` "Portable bundles". Refused over the delegation
    /// server, which has no bundle endpoint: run `by artifact export`
    /// locally instead.
    pub fn export_artifacts(
        &self,
        ids: &[String],
        out: &Path,
    ) -> Result<Vec<crate::BundleEntry>, Error> {
        match &self.via {
            Via::Local(local) => local.export_artifacts(ids, out),
            Via::Remote(_) => Err(Error::Unsupported(
                "artifact bundles are exported only locally (`by artifact export`), not through \
                 the delegation server"
                    .into(),
            )),
        }
    }

    /// Import a bundle written by [`Delegate::export_artifacts`], owned by
    /// this branch. Refused over the delegation server, for the same
    /// reason as [`Delegate::export_artifacts`].
    pub fn import_artifacts(&self, path: &Path) -> Result<Vec<crate::ArtifactRef>, Error> {
        match &self.via {
            Via::Local(local) => local.import_artifacts(path),
            Via::Remote(_) => Err(Error::Unsupported(
                "artifact bundles are imported only locally (`by artifact import`), not through \
                 the delegation server"
                    .into(),
            )),
        }
    }

    /// Create scratch area `name`, owned by this branch.
    pub fn create_scratch(&self, name: &str) -> Result<crate::ScratchArea, Error> {
        match &self.via {
            Via::Local(local) => local.create_scratch(name),
            Via::Remote(_) => self.typed("create_scratch", json!({"name": name})),
        }
    }

    /// Every scratch area this branch may reach.
    pub fn scratch_areas(&self) -> Result<Vec<crate::ScratchArea>, Error> {
        match &self.via {
            Via::Local(local) => local.list_scratch(),
            Via::Remote(_) => self.typed("list_scratch", json!({})),
        }
    }

    /// Share scratch area `name` with branch `to`.
    pub fn share_scratch(&self, name: &str, to: &str) -> Result<(), Error> {
        match &self.via {
            Via::Local(local) => local.share_scratch(name, to),
            Via::Remote(_) => self
                .typed::<Value>("share_scratch", json!({"name": name, "to": to}))
                .map(|_| ()),
        }
    }

    /// Acquire scratch area `name`'s writer lock for this branch.
    pub fn lock_scratch(&self, name: &str) -> Result<crate::ScratchLock, Error> {
        match &self.via {
            Via::Local(local) => local.lock_scratch(name),
            Via::Remote(_) => self.typed("lock_scratch", json!({"name": name})),
        }
    }

    /// Release scratch area `name`'s lock if this branch holds it.
    pub fn unlock_scratch(&self, name: &str) -> Result<(), Error> {
        match &self.via {
            Via::Local(local) => local.unlock_scratch(name),
            Via::Remote(_) => self
                .typed::<Value>("unlock_scratch", json!({"name": name}))
                .map(|_| ()),
        }
    }

    /// Ask this branch's parent a question. Without `wait`, returns once
    /// the message is sent; with it, blocks (in the process that runs this
    /// branch's turn, so across processes when reached through the
    /// broker) for up to that long for an answer (`in_reply_to` the
    /// question). A wait that passes with no answer yet is not an error:
    /// `answer` is `None`; ask `inbox` or wait again.
    pub fn ask(&self, text: &str, wait: Option<Duration>) -> Result<Asked, Error> {
        self.typed(
            "ask",
            json!({"text": text, "wait_seconds": wait.map(|d| d.as_secs_f64())}),
        )
    }

    /// Report to this branch's parent; no answer is expected.
    pub fn report(&self, text: &str) -> Result<Message, Error> {
        self.typed("report", json!({"text": text}))
    }

    /// Escalate to this branch's parent, or, when its rig seat's
    /// `escalates_to` names one, an ancestor further up.
    pub fn escalate(&self, text: &str) -> Result<Message, Error> {
        self.typed("escalate", json!({"text": text}))
    }

    /// Answer a descendant's message (usually a question) with `text`.
    pub fn answer(&self, message_id: u64, text: &str) -> Result<Message, Error> {
        self.typed("answer", json!({"message_id": message_id, "text": text}))
    }

    /// This branch's own inbox: every message addressed to it, oldest
    /// first.
    pub fn inbox(&self) -> Result<Inbox, Error> {
        self.typed("inbox", json!({}))
    }

    /// The messages addressed to this branch not yet delivered to a turn.
    pub fn unread(&self) -> Result<Inbox, Error> {
        self.typed("inbox", json!({"unread": true}))
    }

    /// Inspect `branch` until it is not running a turn, for up to
    /// `timeout`. Fails with [`Error::Running`] if it still is.
    ///
    /// The branch's turn may run in any process using the repository: the
    /// wait reads its durable status, and a turn whose engine stopped is
    /// recovered, so it ends `interrupted` rather than being waited for.
    pub fn wait(&self, branch: &str, timeout: Duration) -> Result<Inspection, Error> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            let inspection = self.inspect(branch)?;
            if !crate::wake::unsettled(&inspection.status) {
                return Ok(inspection);
            }
            let now = Instant::now();
            if deadline.is_some_and(|d| now >= d) {
                return Err(Error::Running(branch.to_owned()));
            }
            let left = deadline.map_or(SETTLE_EVERY, |d| (d - now).min(SETTLE_EVERY));
            match &self.via {
                Via::Local(local) => {
                    recover::settle(&local.yard, branch)?;
                    if inspection.status == BranchStatus::Waiting {
                        graph::advance(&local.yard, &[branch.to_owned()], None)?;
                    }
                    if inspection.status == BranchStatus::WaitingOnChildren {
                        crate::wake::look(&local.yard, branch, None)?;
                    }
                    let store = local.store();
                    let before = inspection.status.clone();
                    store.wait(left, || {
                        Ok((store.read(branch)?.info.status != before).then_some(()))
                    })?;
                }
                Via::Remote(_) => std::thread::sleep(WAIT_POLL.min(left)),
            }
        }
    }

    /// [`Delegate::wait_for`] the first of `branches` to settle.
    pub fn wait_any(&self, branches: &[&str], timeout: Option<Duration>) -> Result<Waited, Error> {
        self.wait_for(branches, true, timeout)
    }

    /// [`Delegate::wait_for`] all of `branches` to settle.
    pub fn wait_all(&self, branches: &[&str], timeout: Option<Duration>) -> Result<Waited, Error> {
        self.wait_for(branches, false, timeout)
    }

    /// Block until `branches` (descendants, or this branch) have settled:
    /// any one of them with `any`, else all, or until `timeout` passes
    /// (then `timed_out` is set; it is not an error). With no branches, it
    /// waits for this branch's children that are still running or
    /// waiting, and returns at once, listing every child, when none is.
    ///
    /// The wait is the store's, as [`Delegate::wait`]'s: it wakes when a
    /// status changes, at once in the engine's process and within 100 ms
    /// in another. Through the broker it blocks in the engine's process.
    pub fn wait_for(
        &self,
        branches: &[&str],
        any: bool,
        timeout: Option<Duration>,
    ) -> Result<Waited, Error> {
        let names: Vec<String> = branches.iter().map(|b| (*b).to_owned()).collect();
        match &self.via {
            Via::Local(local) => local.wait_for(names, any, timeout),
            Via::Remote(_) => self.typed(
                "wait",
                json!({
                    "branches": names,
                    "any": any,
                    "timeout_seconds": timeout.map(|t| t.as_secs_f64()),
                }),
            ),
        }
    }
}

/// Block until `names` have settled, any one with `any` else all, or
/// `timeout` passes, reading their durable status from the store; each
/// settled one is reported through `inspect`. While it waits it moves what
/// a wait can: a turn whose engine stopped is recovered, a dependent whose
/// prerequisites settled is started, a parked branch whose children settled
/// is woken, each when this process can.
pub(crate) fn wait_for(
    yard: &Yard,
    names: &[String],
    any: bool,
    timeout: Option<Duration>,
    inspect: impl Fn(&str) -> Result<Inspection, Error>,
) -> Result<Waited, Error> {
    let store = yard.store();
    let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
    let statuses = |store: &Store| -> Result<Vec<BranchStatus>, Error> {
        names
            .iter()
            .map(|name| store.read(name).map(|r| r.info.status))
            .collect()
    };
    loop {
        let now = statuses(&store)?;
        let pending: Vec<String> = names
            .iter()
            .zip(&now)
            .filter(|(_, status)| crate::wake::unsettled(status))
            .map(|(name, _)| name.clone())
            .collect();
        let done = match any {
            true => pending.len() < names.len() || names.is_empty(),
            false => pending.is_empty(),
        };
        let late = deadline.is_some_and(|d| Instant::now() >= d);
        if done || late {
            let settled = names
                .iter()
                .filter(|name| !pending.contains(name))
                .map(|name| inspect(name))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Waited {
                settled,
                pending,
                timed_out: !done,
            });
        }
        for (name, status) in names.iter().zip(&now) {
            match status {
                BranchStatus::Running => recover::settle(yard, name)?,
                BranchStatus::Waiting => {
                    graph::advance(yard, std::slice::from_ref(name), None)?;
                }
                BranchStatus::WaitingOnChildren => {
                    crate::wake::look(yard, name, None)?;
                }
                _ => {}
            }
        }
        let left = deadline.map_or(SETTLE_EVERY, |d| {
            d.saturating_duration_since(Instant::now())
                .min(SETTLE_EVERY)
        });
        store.wait(left, || Ok((statuses(&store)? != now).then_some(())))?;
    }
}

/// The context of `token`'s running turn in this process.
pub(crate) fn local_by_token(yard: &Yard, token: &str) -> Result<Local, Error> {
    let contexts = lock(&yard.hub.contexts);
    let found = contexts
        .iter()
        .find(|(_, context)| same_token(&context.token, token));
    match found {
        Some((branch, context)) => Ok(Local {
            yard: yard.clone(),
            branch: branch.clone(),
            options: context.options.clone(),
            cost: Some(context.cost.clone()),
        }),
        None => Err(Error::Denied(
            "this delegation token is not valid; tokens are issued for a running turn and \
             revoked when it ends"
                .into(),
        )),
    }
}

/// `token`'s branch: through this process when it runs the turn, else
/// through the broker.
pub(crate) fn as_branch(yard: &Yard, token: &str) -> Result<Delegate, Error> {
    match local_by_token(yard, token) {
        Ok(local) => Ok(Delegate {
            branch: local.branch.clone(),
            via: Via::Local(Box::new(local)),
        }),
        Err(_) => Delegate::connect(&yard.root, token),
    }
}

pub(crate) fn trusted(yard: &Yard, branch: &str, options: TaskOptions) -> Result<Delegate, Error> {
    let record = yard.store().read(branch)?;
    let options = TaskOptions {
        budget: effective_budget(&record, &options.budget),
        policy: effective_policy(&record, &options.policy),
        ..options
    };
    Ok(Delegate {
        branch: branch.to_owned(),
        via: Via::Local(Box::new(Local {
            yard: yard.clone(),
            branch: branch.to_owned(),
            options,
            cost: None,
        })),
    })
}

/// The caller's budget, narrowed by limits a parent imposed.
pub(crate) fn effective_budget(record: &Record, budget: &Budget) -> Budget {
    let Some(limits) = record.grant.as_ref().and_then(|g| g.limits.as_ref()) else {
        return budget.clone();
    };
    fn min<T: PartialOrd>(a: Option<T>, b: Option<T>) -> Option<T> {
        match (a, b) {
            (Some(a), Some(b)) => Some(if b < a { b } else { a }),
            (a, b) => a.or(b),
        }
    }
    Budget {
        max_usd: min(budget.max_usd, limits.max_usd),
        max_turns: min(budget.max_turns, limits.max_turns),
        max_duration: min(
            budget.max_duration,
            limits.max_duration_ms.map(Duration::from_millis),
        ),
        // Stall detection is not part of a delegation envelope: a parent
        // narrows cost, turns and duration, but a stall window is the
        // caller's own choice for this turn.
        stall_after: budget.stall_after,
        stall_action: budget.stall_action,
    }
}

/// The caller's policy with the denials a parent imposed, and those the
/// branch was started with, put first.
pub(crate) fn effective_policy(record: &Record, policy: &Policy) -> Policy {
    let given = record.grant.as_ref().map_or(&[][..], |g| g.deny.as_slice());
    let deny = run::with_denials(given, &record.deny);
    match deny.is_empty() {
        true => policy.clone(),
        false => narrowed(policy, &deny),
    }
}

/// `policy` with deny rules for `deny` ahead of its own. Only ever less
/// permissive: a request either matches a new deny rule or is decided as
/// before.
fn narrowed(policy: &Policy, deny: &[String]) -> Policy {
    let mut rules: Vec<Rule> = deny.iter().map(Rule::deny).collect();
    rules.extend(policy.rules.iter().cloned());
    Policy {
        rules,
        fallback: policy.fallback.clone(),
        preset: policy.preset,
    }
}

/// Whether a child can run turns without its parent asking again, and so
/// holds its whole limit: running, waiting for its prerequisites (it
/// starts on its own when they settle), blocked (a graph proposal reopens
/// it), with a plan awaiting approval (approving it starts a turn), or
/// waiting on its own children (it is woken when they settle; see
/// `crate::wake`). Every other status is settled: the child holds only
/// what it spent until it is sent something again, and a discarded one
/// never is.
pub(crate) fn is_live(status: &BranchStatus) -> bool {
    matches!(
        status,
        BranchStatus::Running
            | BranchStatus::Waiting
            | BranchStatus::Blocked { .. }
            | BranchStatus::AwaitingPlanApproval
            | BranchStatus::WaitingOnChildren
    )
}

/// How many of `record`'s children are live ([`is_live`]), not counting
/// `except`: what its envelope's `max_children` bounds.
fn live_children(store: &Store, record: &Record, except: Option<&str>) -> usize {
    record
        .info
        .children
        .iter()
        .filter(|child| Some(child.as_str()) != except)
        .filter_map(|child| store.read(child).ok())
        .filter(|child| is_live(&child.info.status))
        .count()
}

/// What `record`'s children hold of its budget; see [`Held`].
pub(crate) fn reserved(store: &Store, record: &Record) -> f64 {
    held(store, record).total()
}

/// What a branch's children hold of its budget, by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Held {
    /// Held by live children ([`is_live`]): each one's limit, or what its
    /// subtree spent if that is more.
    pub live_usd: f64,
    /// How many live children hold it.
    pub live: u32,
    /// Spent by settled children's subtrees (what a live grandchild of
    /// one holds included), and by removed children's, from the ledger.
    pub settled_usd: f64,
}

impl Held {
    pub fn total(&self) -> f64 {
        self.live_usd + self.settled_usd
    }
}

pub(crate) fn held(store: &Store, record: &Record) -> Held {
    let mut held = Held {
        settled_usd: record.removed_spent(),
        ..Held::default()
    };
    for child in record
        .info
        .children
        .iter()
        .filter_map(|child| store.read(child).ok())
    {
        match is_live(&child.info.status) {
            true => {
                held.live += 1;
                held.live_usd += holds(store, &child);
            }
            false => held.settled_usd += holds(store, &child),
        }
    }
    held
}

/// What one child holds of its parent's budget. A live child holds its
/// limit, or its subtree's spend if that is more, since it may spend up to
/// its limit without asking. A settled one holds what it spent, and what
/// its own children hold.
fn holds(store: &Store, child: &Record) -> f64 {
    let spent = subtree_spent(store, child);
    match is_live(&child.info.status) {
        true => limit_usd(child).map_or(spent, |limit| limit.max(spent)),
        false => (child.info.cost_usd.unwrap_or(0.0) + reserved(store, child)).max(spent),
    }
}

/// The cost limit a delegated child's turns run under.
fn limit_usd(record: &Record) -> Option<f64> {
    record
        .grant
        .as_ref()
        .and_then(|g| g.limits.as_ref())
        .and_then(|l| l.max_usd)
}

/// Reported costs of `record` and its descendants, removed ones included
/// (from each parent's ledger); unreported counts as 0.
fn subtree_spent(store: &Store, record: &Record) -> f64 {
    record.subtree_spent(&mut |name| store.read(name).ok())
}

/// Whether any direct child of `name` is currently running a turn. Read
/// fresh from the store, not from a turn's own (possibly stale) snapshot: a
/// child spawned during the running turn itself is exactly the case a
/// delegation wait needs to exclude. A branch delegating to a child keeps
/// its own turn `Running` for as long as it waits on it (spawn, send or
/// `wait_subtree` all block the calling turn), so checking direct children
/// is enough, without walking the whole subtree.
pub(crate) fn any_child_running(store: &Store, name: &str) -> bool {
    let Ok(record) = store.read(name) else {
        return false;
    };
    record.info.children.iter().any(|child| {
        store
            .read(child)
            .is_ok_and(|record| record.info.status == BranchStatus::Running)
    })
}

/// Every descendant of `name`, oldest first.
pub(crate) fn descendants(store: &Store, name: &str) -> Result<Vec<BranchInfo>, Error> {
    let mut found = Vec::new();
    let mut seen = BTreeSet::from([name.to_owned()]);
    let mut queue = store.read(name)?.info.children;
    while let Some(child) = queue.pop() {
        if !seen.insert(child.clone()) {
            continue;
        }
        let Ok(record) = store.read(&child) else {
            continue;
        };
        queue.extend(record.info.children.iter().cloned());
        found.push(record);
    }
    found.sort_by(|a, b| (a.created_ms, &a.info.name).cmp(&(b.created_ms, &b.info.name)));
    Ok(found.into_iter().map(|record| record.info).collect())
}

/// Whether `descendant` is `ancestor` or below it in the delegation tree.
pub(crate) fn is_ancestor(store: &Store, ancestor: &str, descendant: &str) -> Result<bool, Error> {
    if ancestor == descendant {
        return Ok(true);
    }
    Ok(descendants(store, ancestor)?
        .iter()
        .any(|info| info.name == descendant))
}

/// Ask `name`'s running turn and every running turn below it to stop, on
/// behalf of `by`: a durable request per running turn, which the engine
/// running it observes.
///
/// A branch still `waiting` for its prerequisites never starts: it ends
/// `interrupted` at once, and what waits for it is blocked.
pub(crate) fn cancel_tree(yard: &Yard, name: &str, by: &str) -> Result<Vec<String>, Error> {
    let store = yard.store();
    let mut targets = vec![store.read(name)?.info];
    targets.extend(descendants(&store, name)?);
    let mut cancelled = Vec::new();
    for info in targets {
        if info.status == BranchStatus::WaitingOnChildren {
            // Parked: it runs no turn to stop, and is not woken.
            if let Ok(record) = store.read(&info.name) {
                let mut ended = record.clone();
                ended.info.status = BranchStatus::Interrupted;
                ended.parked = None;
                let event = RecordedEvent {
                    at_ms: branchyard_support::time::now_ms(),
                    activity: Activity::Status(BranchStatus::Interrupted),
                };
                if store
                    .graph()
                    .settle_if(&ended, &event, crate::wake::is_parked)?
                {
                    store.notify();
                    cancelled.push(info.name);
                }
            }
            continue;
        }
        if info.status == BranchStatus::Waiting {
            if let Ok(record) = store.read(&info.name) {
                if graph::cancel_unstarted(&store, &record, by)? {
                    graph::settled(yard, &info.name, None);
                    cancelled.push(info.name);
                }
            }
            continue;
        }
        if store.request_cancel(&info.name, by, true)? {
            cancelled.push(info.name);
        }
    }
    Ok(cancelled)
}

/// Every branch a wait for `name`'s subtree covers, besides `all` (its
/// descendants): what depends on a branch in the subtree, and their
/// descendants, transitively. Such a branch is started by a turn in the
/// subtree ending, so a process that waits for the subtree waits for it
/// too.
fn dependency_closure(
    store: &Store,
    name: &str,
    all: &[BranchInfo],
) -> Result<Vec<BranchInfo>, Error> {
    let mut seen: BTreeSet<String> = all.iter().map(|info| info.name.clone()).collect();
    seen.insert(name.to_owned());
    let mut queue: Vec<String> = seen.iter().cloned().collect();
    let mut extra = Vec::new();
    while let Some(next) = queue.pop() {
        for dependency in store.graph().dependents(&next)? {
            let found = dependency.dependent;
            if !seen.insert(found.clone()) {
                continue;
            }
            let Ok(record) = store.read(&found) else {
                continue;
            };
            let below = descendants(store, &found)?;
            for info in &below {
                if seen.insert(info.name.clone()) {
                    queue.push(info.name.clone());
                    extra.push(info.clone());
                }
            }
            queue.push(found);
            extra.push(record.info);
        }
    }
    Ok(extra)
}

/// Wait until no descendant of `name` is running a turn, wherever it runs.
/// Children on this process's threads are joined; a child another process
/// drives is waited for through its durable status, and one whose engine
/// stopped is recovered.
pub(crate) fn wait_subtree(yard: &Yard, name: &str) -> Result<Vec<BranchInfo>, Error> {
    let store = yard.store();
    loop {
        let all = descendants(&store, name)?;
        let mut watched = all.clone();
        watched.extend(dependency_closure(&store, name, &all)?);
        // The branch itself, while it waits on its children or runs the
        // turn that woke it here.
        let own = store.read(name)?;
        let woken_here = own.info.status == BranchStatus::Running
            && store
                .backend()
                .leases()?
                .iter()
                .any(|l| l.branch == name && l.owner.as_deref() == Some(&store.owner().id));
        if woken_here || own.info.status == BranchStatus::WaitingOnChildren {
            watched.push(own.info.clone());
        }
        let names: BTreeSet<String> = watched.iter().map(|info| info.name.clone()).collect();
        let handles: Vec<JoinHandle<()>> = {
            let mut running = lock(&yard.hub.running);
            let waiting: Vec<String> = running
                .keys()
                .filter(|key| names.contains(*key))
                .cloned()
                .collect();
            waiting
                .iter()
                .filter_map(|key| running.remove(key))
                .collect()
        };
        if !handles.is_empty() {
            for handle in handles {
                // A panic in a child's thread has already been reported by
                // the runtime; its record says `running` until recovered,
                // and the others go on.
                branchyard_support::join_reporting("child turn", handle);
            }
            continue;
        }
        let running: Vec<&BranchInfo> = watched
            .iter()
            .filter(|info| info.status == BranchStatus::Running)
            .collect();
        if running.is_empty() {
            // A dependent whose prerequisites settled while no engine
            // could start it, such as one whose prerequisite's engine
            // stopped: start it now if this process applied its graph.
            let waiting: Vec<String> = watched
                .iter()
                .filter(|info| info.status == BranchStatus::Waiting)
                .map(|info| info.name.clone())
                .collect();
            if !waiting.is_empty() && !graph::advance(yard, &waiting, None)?.is_empty() {
                continue;
            }
            // A parked branch whose children all settled: its next turn,
            // when this process ran the turn that parked it.
            let mut woke = false;
            for info in watched.iter().rev() {
                if info.status == BranchStatus::WaitingOnChildren {
                    woke |= crate::wake::look(yard, &info.name, None)?;
                }
            }
            if woke {
                continue;
            }
            return descendants(&store, name);
        }
        for info in running {
            recover::settle(yard, &info.name)?;
        }
        let watching = names.clone();
        store.wait(SETTLE_EVERY, || {
            let settled = watching.iter().all(|name| {
                store
                    .read(name)
                    .map_or(true, |record| record.info.status != BranchStatus::Running)
            });
            Ok(settled.then_some(()))
        })?;
    }
}

/// Run a turn on a thread of this process, joined by a wait for its
/// subtree.
pub(crate) fn start_turn(
    yard: &Yard,
    prepared: Prepared,
    prompt: String,
    options: TaskOptions,
) -> Result<(), Error> {
    let prompt = prepared.prompt(&prompt);
    let Prepared {
        record,
        lease,
        profile,
        command,
        mode,
        note,
    } = prepared;
    let name = record.info.name.clone();
    let thread_yard = yard.clone();
    let panic_yard = yard.clone();
    let panic_branch = name.clone();
    let started = branchyard_support::spawn_named(
        format!("by-{name}"),
        // A panic in the turn is otherwise only a line on stderr; put it in
        // the branch's own event log, where its readers look.
        move |panic| {
            let event = RecordedEvent {
                at_ms: branchyard_support::time::now_ms(),
                activity: Activity::Warning(format!(
                    "the turn's thread panicked: {}",
                    panic.message
                )),
            };
            branchyard_support::best_effort(
                "record a turn thread's panic",
                panic_yard.store().append(&panic_branch, &event, None),
            );
        },
        move || {
            // The outcome is the branch's status; errors are recorded there.
            branchyard_support::best_effort(
                "run the delegated turn",
                engine::execute(
                    Turn {
                        yard: &thread_yard,
                        record,
                        profile,
                        command,
                        mode,
                        prompt: &prompt,
                        options: &options,
                        fork_source: None,
                        note,
                        sandbox: Default::default(),
                    },
                    lease,
                ),
            );
        },
    );
    match started {
        Ok(handle) => {
            let previous = lock(&yard.hub.running).insert(name, handle);
            if let Some(previous) = previous {
                branchyard_support::join_reporting("previous turn", previous);
            }
            Ok(())
        }
        Err(error) => {
            let store = yard.store();
            if let Ok(mut record) = store.read(&name) {
                record.info.status = BranchStatus::Failed {
                    reason: format!("could not start a thread: {error}"),
                };
                branchyard_support::best_effort(
                    "mark a branch failed after its thread would not start",
                    store.write(&record),
                );
            }
            Err(Error::Io(error))
        }
    }
}

/// One edit of a proposal, with a spawn in its Rust form.
#[derive(Clone, Debug)]
enum Edit {
    Spawn(Box<Spawn>),
    Add(Dependency),
    Remove(DependencyRef),
}

impl Edit {
    fn from_graph(edit: &GraphEdit) -> Result<Edit, Error> {
        Ok(match edit {
            GraphEdit::Spawn(spec) => Edit::Spawn(Box::new(spec.to_spawn()?)),
            GraphEdit::AddDependency(d) => Edit::Add(d.clone()),
            GraphEdit::RemoveDependency(d) => Edit::Remove(d.clone()),
        })
    }
}

/// A child of a proposal, checked and ready to be created.
struct Planned {
    /// Its `waiting` record.
    record: Record,
    /// Its check came from its parent, not the request.
    check_inherited: bool,
    limits: Limits,
    seat: Option<String>,
    depends_on: Vec<String>,
    after: After,
}

/// `record`'s siblings that share its check and may still be integrated:
/// ready, not settled yet, or stopped with a candidate. Oldest first.
fn sharing_check(store: &Store, record: &Record) -> Vec<Record> {
    let (Some(check), Some(parent)) = (&record.check, &record.info.parent) else {
        return Vec::new();
    };
    let Ok(parent) = store.read(parent) else {
        return Vec::new();
    };
    parent
        .info
        .children
        .iter()
        .filter(|child| **child != record.info.name)
        .filter_map(|child| store.read(child).ok())
        .filter(|sibling| sibling.check.as_ref() == Some(check))
        .filter(|sibling| match &sibling.info.status {
            BranchStatus::Ready => true,
            BranchStatus::Interrupted | BranchStatus::BudgetExceeded { .. } => {
                sibling.info.candidate.is_some()
            }
            status => is_live(status),
        })
        .collect()
}

/// A failed check of an integration of `branches` that left out siblings
/// sharing it: say who they are and the integration that runs it on all
/// of them. Any other error is returned as it is.
fn with_shared_check(store: &Store, error: Error, branches: &[String]) -> Error {
    let Error::CheckFailed {
        output_tail,
        shared: None,
    } = error
    else {
        return error;
    };
    let mut shared: Option<SharedCheck> = None;
    for record in branches.iter().filter_map(|b| store.read(b).ok()) {
        let (Some(check), siblings) = (record.check.clone(), sharing_check(store, &record)) else {
            continue;
        };
        let into = shared.get_or_insert_with(|| SharedCheck {
            check,
            inherited_from: record
                .info
                .parent
                .clone()
                .filter(|_| record.check_inherited),
            siblings: Vec::new(),
            unsettled: Vec::new(),
            integrate_together: branches.to_vec(),
        });
        for sibling in siblings {
            let name = sibling.info.name;
            if branches.contains(&name) || into.siblings.contains(&name) {
                continue;
            }
            if is_live(&sibling.info.status) {
                into.unsettled.push(name.clone());
            }
            into.integrate_together.push(name.clone());
            into.siblings.push(name);
        }
    }
    Error::CheckFailed {
        output_tail,
        shared: shared.filter(|s| !s.siblings.is_empty()).map(Box::new),
    }
}

/// What became of an integrated branch, for its parent's event log.
fn merged_outcome(merged: &Merged) -> String {
    match (&merged.via, merged.already) {
        (Some(via), true) => format!("already contained in {}, via {via}", merged.target),
        _ => format!("merged into {} as {}", merged.target, merged.commit),
    }
}

/// What became of a spawned child, for its parent's event log.
fn spawn_outcome(spawned: &Spawned) -> String {
    match &spawned.status {
        BranchStatus::Waiting => format!("waiting for {}", spawned.depends_on.join(", ")),
        BranchStatus::Blocked { reason } => format!("blocked: {reason}"),
        _ => format!(
            "started on {}",
            profiles::label(&spawned.harness, &spawned.profile)
        ),
    }
}

/// The operations, as one branch, in the process that runs them.
pub(crate) struct Local {
    yard: Yard,
    branch: String,
    /// The acting branch's turn options, bounded; its children inherit
    /// their policy, observer and tools from these.
    options: TaskOptions,
    /// The acting branch's live spend, while its turn runs.
    cost: Option<Arc<Mutex<Option<f64>>>>,
}

impl Local {
    fn store(&self) -> Store {
        self.yard.store()
    }

    /// Fail unless this branch holds `capability`: every branch reaches
    /// itself, its storage and its parent's inbox; only one whose envelope
    /// allows children acts on descendants. See [`crate::operations`].
    fn require(&self, capability: Capability, what: &str) -> Result<(), Error> {
        if capability != Capability::Delegate {
            return Ok(());
        }
        let grant = self.store().read(&self.branch)?.grant;
        match grant.as_ref().is_some_and(Grant::can_spawn) {
            true => Ok(()),
            false => Err(Error::Denied(format!(
                "{} may not {what}: {}; it acts only on itself, the artifacts and scratch \
                 areas it may reach, and its parent's inbox",
                self.branch,
                match grant {
                    Some(_) => "its envelope's max_depth is 0, so it has no children",
                    None => "it was not given delegation",
                }
            ))),
        }
    }

    /// Fail unless `target` is a descendant, or this branch itself when
    /// `or_self`.
    fn require_descendant(&self, target: &str, or_self: bool) -> Result<(), Error> {
        if target == self.branch {
            return match or_self {
                true => Ok(()),
                false => Err(Error::Denied(format!(
                    "{target} cannot do this to itself; it acts only on its descendants"
                ))),
            };
        }
        self.require(Capability::Delegate, &format!("act on {target}"))?;
        let found = descendants(&self.store(), &self.branch)?
            .iter()
            .any(|info| info.name == target);
        match found {
            true => Ok(()),
            false => Err(Error::Denied(format!(
                "{target} is not a descendant of {}",
                self.branch
            ))),
        }
    }

    /// This branch's own spend: the record's, or the running turn's if more.
    fn own_spent(&self, record: &Record) -> f64 {
        record
            .info
            .cost_usd
            .unwrap_or(0.0)
            .max(self.live_cost().unwrap_or(0.0))
    }

    /// The running turn's spend as the engine last observed it.
    fn live_cost(&self) -> Option<f64> {
        self.cost.as_ref().and_then(|cost| *lock(cost))
    }

    /// Before settled child `name` runs again: it is about to hold its
    /// whole limit again ([`is_live`]), so it must fit in its parent's
    /// `max_children` and in what its parent has left. A limit that does
    /// not fit is narrowed to what is left: `Some((from, to, parent))`. A
    /// parent with nothing left, or no room for another live child,
    /// refuses the send.
    fn reserve_again(&self, name: &str) -> Result<Option<(f64, f64, String)>, Error> {
        let store = self.store();
        let child = store.read(name)?;
        if is_live(&child.info.status) {
            // Running already, or started by its parent's graph; the send
            // is refused or needs no new reservation.
            return Ok(None);
        }
        let Some(parent_name) = child.info.parent.clone() else {
            return Ok(None);
        };
        let parent = store.read(&parent_name)?;
        if let Some(grant) = &parent.grant {
            let live = live_children(&store, &parent, Some(name));
            if live >= grant.envelope.max_children as usize {
                return Err(Error::Denied(format!(
                    "{parent_name} already has {live} live children, its envelope's \
                     max_children; {name} can run again once one of them settles"
                )));
            }
        }
        let (Some(limit), Some(parent_limit)) = (
            limit_usd(&child),
            match parent_name == self.branch {
                true => self.options.budget.max_usd,
                false => limit_usd(&parent),
            },
        ) else {
            return Ok(None);
        };
        let parent_spent = match parent_name == self.branch {
            true => self.own_spent(&parent),
            false => parent.info.cost_usd.unwrap_or(0.0),
        };
        // `child` counts as settled in `reserved` now.
        let left = parent_limit - parent_spent - reserved(&store, &parent);
        let settled = holds(&store, &child);
        let live = limit.max(subtree_spent(&store, &child));
        if live - settled <= left + EPSILON_USD {
            return Ok(None);
        }
        if left <= EPSILON_USD {
            return Err(Error::Denied(format!(
                "{parent_name} has nothing left of its ${parent_limit:.4} for {name} to spend"
            )));
        }
        Ok(Some((limit, settled + left, parent_name)))
    }

    fn remaining(&self, record: &Record) -> Option<f64> {
        let limit = self.options.budget.max_usd?;
        Some(limit - self.own_spent(record) - reserved(&self.store(), record))
    }

    /// Record a delegation operation on this branch's event log.
    fn note<T>(
        &self,
        tool: &str,
        branch: &str,
        result: &Result<T, Error>,
        done: impl Fn(&T) -> String,
    ) {
        let Ok(mut recorder) =
            Recorder::open(&self.store(), &self.branch, self.options.observer.clone())
        else {
            return;
        };
        let (outcome, refused) = match result {
            Ok(value) => (done(value), false),
            Err(error) => (error.to_string(), true),
        };
        branchyard_support::best_effort(
            "record the activity",
            recorder.record(Activity::Delegation {
                tool: tool.to_owned(),
                branch: branch.to_owned(),
                outcome,
                refused,
            }),
        );
    }

    fn spawn(&self, request: &Spawn) -> Result<Spawned, Error> {
        let result = self
            .try_apply_graph(&[Edit::Spawn(Box::new(request.clone()))], None)
            .and_then(|applied| {
                applied
                    .spawned
                    .into_iter()
                    .next()
                    .ok_or_else(|| Error::State("the spawn created no child".into()))
            });
        let target = match &result {
            Ok(spawned) => spawned.name.clone(),
            Err(_) => request.name.clone().unwrap_or_default(),
        };
        self.note("spawn", &target, &result, spawn_outcome);
        result
    }

    fn apply_graph(&self, edits: &[GraphEdit], expected: u64) -> Result<GraphApplied, Error> {
        let result = edits
            .iter()
            .map(Edit::from_graph)
            .collect::<Result<Vec<_>, _>>()
            .and_then(|edits| self.try_apply_graph(&edits, Some(expected)));
        self.note("apply_graph", &self.branch, &result, |applied| {
            let spawned: Vec<String> = applied
                .spawned
                .iter()
                .map(|s| format!("{} ({})", s.name, spawn_outcome(s)))
                .collect();
            match spawned.is_empty() {
                true => format!("revision {}", applied.revision),
                false => format!("revision {}: {}", applied.revision, spawned.join(", ")),
            }
        });
        result
    }

    /// Validate a whole proposal, then commit it in one store transaction
    /// and start the children whose prerequisites have settled. Nothing is
    /// written unless every edit is valid. `expected` is checked here and
    /// again in the transaction; `None` (a plain spawn) skips it.
    fn try_apply_graph(
        &self,
        edits: &[Edit],
        expected: Option<u64>,
    ) -> Result<GraphApplied, Error> {
        let store = self.store();
        let _spawning = lock(&self.yard.hub.spawning);
        if edits.is_empty() {
            return Err(Error::Denied(
                "a graph proposal needs at least one edit".into(),
            ));
        }
        if edits.len() > MAX_EDITS {
            return Err(Error::Denied(format!(
                "a graph proposal may carry at most {MAX_EDITS} edits, not {}",
                edits.len()
            )));
        }
        let caller = store.read(&self.branch)?;
        let grant = caller
            .grant
            .clone()
            .ok_or_else(|| Error::Denied(format!("{} was not given delegation", self.branch)))?;
        let revision = store.graph().graph_revision(&self.branch)?;
        if let Some(expected) = expected.filter(|e| *e != revision) {
            return Err(Error::StaleRevision {
                branch: self.branch.clone(),
                expected,
                actual: revision,
            });
        }
        let spawns = edits.iter().any(|e| matches!(e, Edit::Spawn(_)));
        if spawns && !grant.can_spawn() {
            return Err(Error::Denied(format!(
                "{} may not create children: its envelope's max_depth is 0",
                self.branch
            )));
        }
        let children: BTreeMap<String, Record> = caller
            .info
            .children
            .iter()
            .filter_map(|child| store.read(child).ok().map(|r| (child.clone(), r)))
            .collect();
        let mut planned: Vec<Planned> = Vec::new();
        let mut taken: BTreeSet<String> = BTreeSet::new();
        for edit in edits {
            if let Edit::Spawn(request) = edit {
                let child = self.plan_child(&caller, &grant, request, &planned, &taken)?;
                taken.insert(child.record.info.name.clone());
                planned.push(child);
            }
        }
        let mut edges: BTreeMap<(String, String), After> = store
            .graph()
            .dependencies(&self.branch)?
            .into_iter()
            .map(|d| ((d.dependent, d.prerequisite), d.after))
            .collect();
        let endpoint = |name: &str| -> Result<(), Error> {
            if taken.contains(name) || children.contains_key(name) {
                return Ok(());
            }
            let below = store.read(name).is_ok()
                && is_ancestor(&store, &self.branch, name).unwrap_or(false);
            Err(Error::Denied(match below {
                true => format!(
                    "{name} is not a child of {}; a dependency joins two of its children",
                    self.branch
                ),
                false => format!(
                    "{name} is not a child of {}, and a branch acts only on its own children's \
                     dependencies",
                    self.branch
                ),
            }))
        };
        let unstarted = |name: &str| -> Result<(), Error> {
            match children.get(name) {
                Some(child) if !graph::unstarted(&child.info.status) => Err(Error::Denied(
                    format!("{name} has already started, so its dependencies can no longer change"),
                )),
                _ => Ok(()),
            }
        };
        let mut add = Vec::new();
        let mut remove = Vec::new();
        let mut touched = BTreeSet::new();
        let mut add_edge = |dependency: Dependency,
                            edges: &mut BTreeMap<(String, String), After>|
         -> Result<(), Error> {
            endpoint(&dependency.dependent)?;
            endpoint(&dependency.prerequisite)?;
            if dependency.dependent == dependency.prerequisite {
                return Err(Error::Denied(format!(
                    "{} cannot depend on itself",
                    dependency.dependent
                )));
            }
            let key = (
                dependency.dependent.clone(),
                dependency.prerequisite.clone(),
            );
            if edges.insert(key, dependency.after).is_some() {
                return Err(Error::Denied(format!(
                    "{} already depends on {}",
                    dependency.dependent, dependency.prerequisite
                )));
            }
            add.push(dependency);
            Ok(())
        };
        for child in &planned {
            for prerequisite in &child.depends_on {
                add_edge(
                    Dependency {
                        dependent: child.record.info.name.clone(),
                        prerequisite: prerequisite.clone(),
                        after: child.after,
                    },
                    &mut edges,
                )?;
            }
        }
        for edit in edits {
            match edit {
                Edit::Spawn(_) => {}
                Edit::Add(dependency) => {
                    endpoint(&dependency.dependent)?;
                    endpoint(&dependency.prerequisite)?;
                    if dependency.dependent != dependency.prerequisite {
                        unstarted(&dependency.dependent)?;
                    }
                    add_edge(dependency.clone(), &mut edges)?;
                    touched.insert(dependency.dependent.clone());
                }
                Edit::Remove(dependency) => {
                    endpoint(&dependency.dependent)?;
                    endpoint(&dependency.prerequisite)?;
                    unstarted(&dependency.dependent)?;
                    let key = (
                        dependency.dependent.clone(),
                        dependency.prerequisite.clone(),
                    );
                    if edges.remove(&key).is_none() {
                        return Err(Error::Denied(format!(
                            "{} does not depend on {}",
                            dependency.dependent, dependency.prerequisite
                        )));
                    }
                    remove.push(dependency.clone());
                    touched.insert(dependency.dependent.clone());
                }
            }
        }
        let pairs: BTreeSet<(String, String)> = edges.keys().cloned().collect();
        if let Some(cycle) = graph::find_cycle(&pairs) {
            return Err(Error::Denied(format!(
                "the proposal would make a dependency cycle: {}",
                cycle.join(" waits for ")
            )));
        }
        // Children that start now begin from this branch's current work,
        // as a spawn always has; later ones from its branch as it is then.
        if planned
            .iter()
            .any(|p| p.depends_on.is_empty() && p.record.start_base.is_none())
        {
            self.current_work(&caller, "snapshot before delegating")?;
        }
        let revision = store.graph().commit_graph(&GraphCommit {
            parent: self.branch.clone(),
            expected,
            create: planned.iter().map(|p| p.record.clone()).collect(),
            add,
            remove,
        })?;
        lock(&self.yard.hub.graph_options).insert(self.branch.clone(), self.child_options());
        let mut consider: Vec<String> =
            planned.iter().map(|p| p.record.info.name.clone()).collect();
        consider.extend(touched);
        let started = graph::advance(&self.yard, &consider, Some(&self.options))?;
        let spawned = planned
            .into_iter()
            .map(|child| {
                let name = child.record.info.name.clone();
                let info = started
                    .iter()
                    .find(|r| r.info.name == name)
                    .map(|r| r.info.clone())
                    .or_else(|| store.read(&name).ok().map(|r| r.info))
                    .unwrap_or(child.record.info);
                Spawned {
                    name: info.name,
                    git_branch: info.git_branch,
                    harness: info.harness,
                    profile: info.profile,
                    base: info.base,
                    depth: info.depth,
                    status: info.status,
                    budget: ChildBudget {
                        max_usd: child.limits.max_usd,
                        max_turns: child.limits.max_turns,
                        max_minutes: child.limits.max_duration_ms.map(|ms| ms as f64 / 60_000.0),
                    },
                    seat: child.seat,
                    depends_on: child.depends_on,
                    check: child.record.check.clone(),
                    check_inherited: child.check_inherited,
                }
            })
            .collect();
        Ok(GraphApplied {
            branch: self.branch.clone(),
            revision,
            spawned,
            dependencies: edges
                .into_iter()
                .map(|((dependent, prerequisite), after)| Dependency {
                    dependent,
                    prerequisite,
                    after,
                })
                .collect(),
        })
    }

    /// Check one child of a proposal against this branch's envelope and
    /// budget, counting the children planned before it, and build its
    /// `waiting` record. Writes nothing.
    fn plan_child(
        &self,
        caller: &Record,
        grant: &Grant,
        request: &Spawn,
        planned: &[Planned],
        taken: &BTreeSet<String>,
    ) -> Result<Planned, Error> {
        let store = self.store();
        // A removed, discarded or otherwise settled child holds no slot.
        let live = live_children(&store, caller, None) + planned.len();
        if live >= grant.envelope.max_children as usize {
            return Err(Error::Denied(format!(
                "{} already has {live} live children (running, waiting, blocked, awaiting \
                 plan approval or waiting on its children), its envelope's max_children; settled children do not count, \
                 so wait for one to settle, or `by discard` or `by rm` one, to free its slot",
                self.branch
            )));
        }
        if request.prompt.trim().is_empty() {
            return Err(Error::Denied("a child needs a prompt".into()));
        }
        let seated = self.seat(caller, grant, request, planned)?;
        let filled;
        let request = match &seated {
            Some((name, seat, below)) => {
                filled = fill(request, name, seat, below)?;
                &filled
            }
            None => request,
        };
        let own = profiles::by_id(&caller.info.profile)
            .ok_or_else(|| Error::UnknownHarness(caller.info.profile.clone()))?;
        let profile = match &request.harness {
            Some(id) => harness::select(Some(id))?,
            None => own,
        };
        if !grant.envelope.allows(profile, own) {
            return Err(Error::Denied(format!(
                "{} may not delegate to {}; allowed: {}",
                self.branch,
                profile.label(),
                grant.envelope.allowed_text(own)
            )));
        }
        let envelope = grant.envelope.child(request, own)?;
        let planned_usd: f64 = planned.iter().filter_map(|p| p.limits.max_usd).sum();
        let limits = self.child_limits(caller, &request.budget, planned_usd)?;
        // The parent's own denials, those it was given, then the request's.
        let deny = run::with_denials(&run::with_denials(&grant.deny, &caller.deny), &request.deny);
        let child_grant = Grant {
            envelope,
            deny,
            limits: Some(limits.clone()),
            seats: seated.as_ref().map(|(_, _, below)| below.clone()),
        };
        if child_grant.can_spawn() {
            crate::projection::tools(&self.options)?;
        }
        // The same harness keeps the parent's executable override.
        let command = match profile.id == own.id {
            true => caller.command.clone(),
            false => None,
        };
        let launch = harness::command(profile, command.as_deref());
        harness::check_approvals(profile, self.options.unapproved_tools)?;
        harness::check_available(profile.harness, &launch)?;
        let seat = seated.as_ref().map(|(_, seat, _)| seat);
        let isolated =
            caller.home.is_some() || self.options.isolated || seat.is_some_and(|s| s.isolated);
        let mut provision = match seat.and_then(|s| s.provision.clone()) {
            Some(own) => Some(own),
            None => caller.provision.clone(),
        };
        // Connectors: what the request asks for, else its seat's, else the
        // parent's; always within the parent's grant.
        let parent_grant = caller
            .provision
            .as_ref()
            .map(|p| p.connectors.clone())
            .unwrap_or_default();
        let asked = request.connectors.clone().or_else(|| {
            seat.and_then(|s| s.provision.as_ref())
                .map(|p| p.connectors.clone())
        });
        let granted = branchyard_provision::connectors::narrow(asked.as_deref(), &parent_grant)
            .map_err(|why| Error::Denied(format!("{} may not grant that: {why}", self.branch)))?;
        match (&mut provision, granted.is_empty()) {
            (Some(spec), _) => spec.connectors = granted,
            (None, true) => {}
            (None, false) => {
                provision = Some(crate::Provisioning {
                    connectors: granted,
                    ..Default::default()
                })
            }
        }
        // Its network policy: its seat's (or what it inherited), within
        // the parent's.
        let network = branchyard_provision::network::narrow(
            provision.as_ref().and_then(|p| p.network.as_ref()),
            caller.provision.as_ref().and_then(|p| p.network.as_ref()),
        )
        .map_err(|why| Error::Denied(format!("{} may not grant that: {why}", self.branch)))?;
        match (&mut provision, network) {
            (Some(spec), network) => spec.network = network,
            (None, None) => {}
            (None, Some(network)) => {
                provision = Some(crate::Provisioning {
                    network: Some(network),
                    ..Default::default()
                })
            }
        }
        // Its models: its seat's (or what it inherited), within the
        // parent's; a child of a branch on the model gateway stays on it.
        let models = branchyard_provision::models::narrow(
            provision.as_ref().and_then(|p| p.models.as_ref()),
            caller.provision.as_ref().and_then(|p| p.models.as_ref()),
        )
        .map_err(|why| Error::Denied(format!("{} may not grant that: {why}", self.branch)))?;
        match (&mut provision, models) {
            (Some(spec), models) => spec.models = models,
            (None, None) => {}
            (None, Some(models)) => {
                provision = Some(crate::Provisioning {
                    models: Some(models),
                    ..Default::default()
                })
            }
        }
        // Its approvals: its seat's (or what it inherited), within the
        // parent's; only ever stricter (docs/effects.md).
        let approvals = branchyard_provision::approvals::narrow(
            provision.as_ref().and_then(|p| p.approvals.as_ref()),
            caller.provision.as_ref().and_then(|p| p.approvals.as_ref()),
        );
        match (&mut provision, approvals) {
            (Some(spec), approvals) => spec.approvals = approvals,
            (None, None) => {}
            (None, Some(approvals)) => {
                provision = Some(crate::Provisioning {
                    approvals: Some(approvals),
                    ..Default::default()
                })
            }
        }
        crate::provisioning::check(
            provision.as_ref(),
            isolated || crate::placement::sandboxed(caller.provider.as_ref()),
        )?;
        crate::egress::check(provision.as_ref(), caller.provider.as_ref())?;
        graph::check_bindings(&store, &self.branch, &request.bindings)?;
        let base = match &request.base {
            Some(rev) => Some(run::resolve_base(&self.yard, Some(rev))?),
            None => None,
        };
        // A seat's child is named after its parent and seat by default.
        let stem = match (&seated, &request.name) {
            (Some((seat, _, _)), None) => format!("{}-{seat}", self.branch),
            _ => request.prompt.clone(),
        };
        let name = names::plan_one(
            &store,
            &self.yard.root,
            request.name.as_deref(),
            &stem,
            taken,
        )?;
        let mut record = run::new_record(
            &store,
            NewBranch {
                name: &name,
                prompt: &request.prompt,
                profile,
                base: base.clone().unwrap_or_default(),
                parent: Some(self.branch.clone()),
                check: request.check.clone().or(caller.check.clone()),
                command,
                home: isolated.then(|| store.home(&name)),
                cost_baseline: None,
                provider: caller.provider.clone(),
                grant: Some(child_grant),
                depth: caller.info.depth + 1,
                // Its denials are in its grant.
                deny: Vec::new(),
                provision,
                workspace: caller.workspace.as_ref().map(|w| w.spec.clone()),
                // Resolved when it starts, from its parent as it is then
                // (`crate::graph`).
                seed: None,
                actor: caller.actor.clone(),
                task: None,
            },
        )?;
        record.info.status = BranchStatus::Waiting;
        record.check_inherited = request.check.is_none() && record.check.is_some();
        record.bindings = request.bindings.clone();
        record.start_base = base;
        if request.plan {
            crate::plan::check_profile(profile)?;
            record.plan = Some(crate::plan::PlanState {
                phase: crate::plan::PlanPhase::Planning,
                plan: None,
                round: 1,
            });
        }
        let mut depends_on = Vec::new();
        for prerequisite in &request.depends_on {
            if !depends_on.contains(prerequisite) {
                depends_on.push(prerequisite.clone());
            }
        }
        Ok(Planned {
            check_inherited: record.check_inherited,
            record,
            limits,
            seat: seated.map(|(name, _, _)| name),
            depends_on,
            after: request.after,
        })
    }

    /// The seat `request` fills, with the seats below it; `None` for a
    /// branch outside a rig that names none. A branch in a rig must name
    /// one its own seat delegates to, and may not fill it more often than
    /// the seat's instances allow, counting children planned alongside.
    fn seat(
        &self,
        caller: &Record,
        grant: &Grant,
        request: &Spawn,
        planned: &[Planned],
    ) -> Result<Option<(String, Seat, Seats)>, Error> {
        let seats = match (&grant.seats, &request.seat) {
            (None, None) => return Ok(None),
            (None, Some(seat)) => {
                return Err(Error::Denied(format!(
                    "{} is not in a rig, so it has no seat {seat} to fill; spawn without a seat",
                    self.branch
                )))
            }
            (Some(seats), None) => {
                return Err(Error::Denied(format!(
                    "{} fills seat {} of rig {}, so it spawns only by seat: one of {}",
                    self.branch,
                    seats.seat,
                    seats.rig,
                    listed(&seats.delegates_to)
                )))
            }
            (Some(seats), Some(_)) => seats,
        };
        let name = request.seat.clone().unwrap_or_default();
        let seat = match seats.table.get(&name) {
            Some(seat) if seats.delegates_to.contains(&name) => seat.clone(),
            _ => {
                return Err(Error::Denied(format!(
                    "seat {} of rig {} may spawn only {}, not {name}",
                    seats.seat,
                    seats.rig,
                    listed(&seats.delegates_to)
                )))
            }
        };
        let store = self.store();
        let filled = caller
            .info
            .children
            .iter()
            .filter_map(|child| store.read(child).ok())
            .filter(|child| {
                child
                    .grant
                    .as_ref()
                    .and_then(|g| g.seats.as_ref())
                    .is_some_and(|s| s.seat == name)
            })
            .count()
            + planned
                .iter()
                .filter(|p| p.seat.as_deref() == Some(name.as_str()))
                .count();
        if filled >= seat.instances as usize {
            return Err(Error::Denied(format!(
                "{} already has {filled} child{} in seat {name}, the seat's instances",
                self.branch,
                if filled == 1 { "" } else { "ren" }
            )));
        }
        let below = seats.below(&name);
        Ok(Some((name, seat, below)))
    }

    /// The child's limits, checked against what this branch has left.
    fn child_limits(
        &self,
        caller: &Record,
        asked: &Budget,
        planned_usd: f64,
    ) -> Result<Limits, Error> {
        let bounds = &self.options.budget;
        let max_usd = match (bounds.max_usd, asked.max_usd) {
            (_, Some(ask)) if !(ask.is_finite() && ask > 0.0) => {
                return Err(Error::Denied(format!(
                    "a child's {} must be a positive number, not {ask}",
                    crate::operations::limit_text("max_usd")
                )))
            }
            (Some(_), None) => {
                let remaining = (self.remaining(caller).unwrap_or(0.0) - planned_usd).max(0.0);
                return Err(Error::Denied(format!(
                    "{} has a cost limit, so a child needs one too, {}; ${remaining:.4} remains",
                    self.branch,
                    crate::operations::limit_text("max_usd")
                )));
            }
            (Some(_), Some(ask)) => {
                let remaining = self.remaining(caller).unwrap_or(0.0) - planned_usd;
                if ask > remaining + EPSILON_USD {
                    return Err(Error::Denied(format!(
                        "{} {ask} exceeds what {} has left, ${:.4}",
                        crate::operations::limit_text("max_usd"),
                        self.branch,
                        remaining.max(0.0)
                    )));
                }
                Some(ask)
            }
            (None, ask) => ask,
        };
        let max_turns = match (bounds.max_turns, asked.max_turns) {
            (Some(limit), Some(ask)) if ask > limit => {
                return Err(Error::Denied(format!(
                    "{} {ask} exceeds {}'s {limit}",
                    crate::operations::limit_text("max_turns"),
                    self.branch
                )))
            }
            (limit, ask) => ask.or(limit),
        };
        let max_duration = match (bounds.max_duration, asked.max_duration) {
            (Some(limit), Some(ask)) if ask > limit => {
                return Err(Error::Denied(format!(
                    "a {}s duration exceeds {}'s {}s",
                    ask.as_secs(),
                    self.branch,
                    limit.as_secs()
                )))
            }
            (limit, ask) => ask.or(limit),
        };
        Ok(Limits {
            max_usd,
            max_turns,
            max_duration_ms: max_duration.map(|d| d.as_millis().min(u64::MAX as u128) as u64),
        })
    }

    /// Commit this branch's uncommitted work to its git branch and return
    /// the branch head.
    fn current_work(&self, caller: &Record, why: &str) -> Result<String, Error> {
        let branch = names::validate(&caller.info.name)?;
        let _lock = git::lock();
        let workspace = self
            .yard
            .repo
            .workspace(&branch)
            .map_err(git::error)?
            .ok_or_else(|| {
                Error::State(format!(
                    "no worktree has {} checked out",
                    caller.info.git_branch
                ))
            })?;
        workspace
            .excluding(crate::workspace::excluded(caller))
            .snapshot(&format!("{}: {why}", caller.info.git_branch))
            .map_err(git::error)?;
        git::local_branch(&self.yard.root, &caller.info.git_branch)?
            .ok_or_else(|| Error::Git(format!("{} is missing", caller.info.git_branch)))
    }

    /// The options a child's turn runs with: the parent's policy, observer
    /// and tools. Its budget and denials come from its own record.
    fn child_options(&self) -> TaskOptions {
        graph::child_options(&self.options)
    }

    /// Run a turn on a thread of this process.
    fn start(&self, prepared: Prepared, prompt: String) -> Result<(), Error> {
        start_turn(&self.yard, prepared, prompt, self.child_options())
    }

    fn graph(&self, branch: &str) -> Result<Graph, Error> {
        self.require_descendant(branch, true)?;
        graph::show(&self.store(), branch)
    }

    fn inspect(&self, branch: &str) -> Result<Inspection, Error> {
        self.require_descendant(branch, true)?;
        let store = self.store();
        let record = store.read(branch)?;
        let own = branch == self.branch;
        let max_usd = match own {
            true => self.options.budget.max_usd,
            false => record
                .grant
                .as_ref()
                .and_then(|g| g.limits.as_ref())
                .and_then(|l| l.max_usd),
        };
        let held = held(&store, &record);
        let remaining_usd = match own {
            true => self.remaining(&record),
            false => {
                max_usd.map(|limit| limit - record.info.cost_usd.unwrap_or(0.0) - held.total())
            }
        };
        let cost_usd = match own {
            // Its running turn's spend, which the engine may not have
            // written yet.
            true => record
                .info
                .cost_usd
                .or(self.live_cost())
                .map(|_| self.own_spent(&record)),
            false => record.info.cost_usd,
        };
        let events = record::read(&store, branch)?;
        let info = record.info.clone();
        let seats = record.grant.as_ref().and_then(|g| g.seats.as_ref());
        let (seat, may_spawn) = match seats {
            Some(seats) => (Some(seats.seat.clone()), seats.delegates_to.clone()),
            None => (None, Vec::new()),
        };
        let check_shared_with = sharing_check(&store, &record)
            .into_iter()
            .map(|r| r.info.name)
            .collect();
        let allowed_harnesses = record
            .grant
            .as_ref()
            .map(|g| g.envelope.allowed(&info.profile))
            .unwrap_or_default();
        Ok(Inspection {
            subtree_cost_usd: subtree_spent(&store, &record).max(if own {
                self.own_spent(&record) + held.settled_usd
            } else {
                0.0
            }),
            name: info.name,
            status: info.status,
            harness: info.harness,
            profile: info.profile,
            parent: info.parent,
            children: info.children,
            depth: info.depth,
            turns: info.turns,
            candidate: info.candidate,
            cost_usd,
            max_usd,
            remaining_usd: remaining_usd.map(|r| r.max(0.0)),
            reserved_usd: held.live_usd,
            reserving_children: held.live,
            settled_children_usd: held.settled_usd,
            allowed_harnesses,
            check: record.check.clone(),
            check_inherited: record.check_inherited,
            check_shared_with,
            envelope: record.grant.map(|g| g.envelope),
            last_message: last_message(&events),
            seat,
            seats: may_spawn,
            stalled: info.stalled,
            graph_revision: store.graph().graph_revision(branch)?,
            depends_on: store.graph().prerequisites(branch)?,
            bindings: record.bindings,
        })
    }

    fn events(
        &self,
        branch: &str,
        cursor: Option<usize>,
        limit: usize,
    ) -> Result<EventPage, Error> {
        self.require_descendant(branch, true)?;
        let store = self.store();
        let total = store.backend().event_count(branch)? as usize;
        let limit = limit.clamp(1, EVENTS_MAX);
        let start = cursor.unwrap_or(total.saturating_sub(limit)).min(total);
        let page = record::since(&store, branch, start as u64, limit)?;
        Ok(EventPage {
            branch: branch.to_owned(),
            next_cursor: start + page.events.len(),
            total,
            events: page.events,
        })
    }

    fn send(&self, branch: &str, prompt: &str) -> Result<Sent, Error> {
        let result = self.try_send(branch, prompt);
        self.note("send", branch, &result, |_| "started".into());
        result
    }

    fn try_send(&self, branch: &str, prompt: &str) -> Result<Sent, Error> {
        self.require_descendant(branch, false)?;
        let _spawning = lock(&self.yard.hub.spawning);
        let narrowed = self.reserve_again(branch)?;
        let mut prepared = run::prepare_send(&self.yard, branch, &self.child_options(), true)?;
        if let Some((from, to, parent)) = narrowed {
            if let Some(limits) = prepared
                .record
                .grant
                .as_mut()
                .and_then(|g| g.limits.as_mut())
            {
                limits.max_usd = Some(to);
            }
            self.store()
                .write_fenced(&prepared.record, prepared.lease.fence())?;
            let note = format!(
                "its cost limit was narrowed from ${from:.4} to ${to:.4}, what {parent} had left \
                 for it"
            );
            prepared.note = Some(match prepared.note.take() {
                Some(earlier) => format!("{earlier}; {note}"),
                None => note,
            });
        }
        let info = prepared.record.info.clone();
        self.start(prepared, prompt.to_owned())?;
        Ok(Sent {
            name: info.name,
            status: info.status,
        })
    }

    fn answer_approval(
        &self,
        id: &str,
        allow: bool,
        reason: Option<&str>,
    ) -> Result<crate::effects::ApprovalAsk, Error> {
        let ask = self.yard.approval(id);
        let target = ask.as_ref().map(|a| a.branch.clone()).unwrap_or_default();
        let result = ask.and_then(|ask| {
            self.require_descendant(&ask.branch, false)?;
            self.yard
                .answer_approval(&ask.id, allow, &self.branch, "parent", reason)
        });
        self.note("answer_approval", &target, &result, |ask| {
            format!(
                "{} approval {}",
                if allow { "allowed" } else { "denied" },
                ask.id
            )
        });
        result
    }

    fn approve_plan(&self, branch: &str, edited: Option<&str>) -> Result<Sent, Error> {
        let result = self.try_approve_plan(branch, edited);
        self.note("approve_plan", branch, &result, |_| {
            "approved its plan; its turn started".into()
        });
        result
    }

    fn try_approve_plan(&self, branch: &str, edited: Option<&str>) -> Result<Sent, Error> {
        self.require_descendant(branch, false)?;
        let _spawning = lock(&self.yard.hub.spawning);
        let (prepared, prompt) = crate::plan::prepare_approval(
            &self.yard,
            branch,
            edited,
            &self.branch,
            &self.child_options(),
        )?;
        let info = prepared.record.info.clone();
        self.start(prepared, prompt)?;
        Ok(Sent {
            name: info.name,
            status: info.status,
        })
    }

    fn reject_plan(&self, branch: &str, reason: Option<&str>, replan: bool) -> Result<Sent, Error> {
        let result = self.try_reject_plan(branch, reason, replan);
        self.note("reject_plan", branch, &result, |_| match replan {
            true => "rejected its plan; it plans again".into(),
            false => "rejected its plan".into(),
        });
        result
    }

    fn try_reject_plan(
        &self,
        branch: &str,
        reason: Option<&str>,
        replan: bool,
    ) -> Result<Sent, Error> {
        self.require_descendant(branch, false)?;
        let _spawning = lock(&self.yard.hub.spawning);
        if !replan {
            let ended = crate::plan::reject(
                &self.yard,
                branch,
                reason,
                false,
                &self.branch,
                &self.child_options(),
            )?;
            return Ok(Sent {
                name: ended.info().name.clone(),
                status: ended.info().status.clone(),
            });
        }
        let (prepared, prompt) = crate::plan::prepare_replan(
            &self.yard,
            branch,
            reason,
            &self.branch,
            &self.child_options(),
        )?;
        let info = prepared.record.info.clone();
        self.start(prepared, prompt)?;
        Ok(Sent {
            name: info.name,
            status: info.status,
        })
    }

    fn integrate(&self, branch: &str) -> Result<Merged, Error> {
        let result = self
            .try_integrate(&[branch.to_owned()])
            .and_then(|mut all| {
                all.branches
                    .pop()
                    .ok_or_else(|| Error::State("the integration returned no branch".into()))
            });
        self.note("integrate", branch, &result, merged_outcome);
        result
    }

    fn integrate_all(&self, branches: &[String]) -> Result<MergedAll, Error> {
        let result = self.try_integrate(branches);
        self.note("integrate", &branches.join(", "), &result, |all| {
            all.branches
                .iter()
                .map(|m| format!("{}: {}", m.branch, merged_outcome(m)))
                .collect::<Vec<_>>()
                .join("; ")
        });
        result
    }

    fn try_integrate(&self, branches: &[String]) -> Result<MergedAll, Error> {
        let store = self.store();
        for branch in branches {
            self.require_descendant(branch, false)?;
            if store.read(branch)?.info.status == BranchStatus::Running {
                return Err(Error::Running(branch.to_owned()));
            }
        }
        let caller = store.read(&self.branch)?;
        self.current_work(
            &caller,
            &format!("snapshot before integrating {}", branches.join(", ")),
        )?;
        let merged = integrate::merge_many(&self.yard, branches, &caller.info.git_branch)
            .map_err(|error| with_shared_check(&store, error, branches))?;
        // A sibling waiting for these to be integrated may start now, and
        // any other child this branch now contains is merged too.
        for branch in branches {
            graph::settled(&self.yard, branch, Some(&self.options));
        }
        integrate::reconcile_children(&self.yard, &self.branch);
        Ok(merged)
    }

    fn steer(&self, branch: &str, text: &str) -> Result<Steer, Error> {
        let result = self.require_descendant(branch, false).and_then(|()| {
            let steer = crate::steer::request(&self.yard, branch, text, &self.branch)?;
            crate::steer::wait(&self.store(), branch, steer.id, STEER_WAIT)
        });
        self.note("steer", branch, &result, |s| match &s.state {
            SteerState::Refused { reason } => format!("steered input {} refused: {reason}", s.id),
            SteerState::Pending => format!("steered input {} queued", s.id),
            SteerState::Written => format!(
                "steered input {} written to the harness, not confirmed yet",
                s.id
            ),
            SteerState::Accepted => format!("steered input {} joined the running turn", s.id),
        });
        result
    }

    fn cancel(&self, branch: &str) -> Result<Cancelled, Error> {
        let result = self
            .require_descendant(branch, false)
            .and_then(|()| cancel_tree(&self.yard, branch, &self.branch))
            .and_then(|cancelled| {
                let status = self.store().read(branch)?.info.status;
                Ok(Cancelled::of(cancelled, branch, &status))
            });
        self.note("cancel", branch, &result, |c| {
            match (&c.note, c.cancelled.is_empty()) {
                (Some(note), _) => note.clone(),
                (None, true) => "nothing was running".into(),
                (None, false) => format!("asked {} to stop", c.cancelled.join(", ")),
            }
        });
        result
    }

    fn discard(&self, branch: &str, reason: Option<&str>) -> Result<Inspection, Error> {
        let result = self.require_descendant(branch, false).and_then(|()| {
            let reason = reason
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map_or_else(|| format!("discarded by {}", self.branch), str::to_owned);
            crate::ops::discard(&self.yard, branch, &reason)?;
            self.inspect(branch)
        });
        self.note("discard", branch, &result, |i| match &i.status {
            BranchStatus::Discarded { reason } => format!("discarded: {reason}"),
            _ => "discarded".into(),
        });
        result
    }

    fn children(&self) -> Result<Children, Error> {
        Ok(Children {
            branch: self.branch.clone(),
            descendants: descendants(&self.store(), &self.branch)?,
        })
    }

    fn wait_for(
        &self,
        mut names: Vec<String>,
        any: bool,
        timeout: Option<Duration>,
    ) -> Result<Waited, Error> {
        let store = self.store();
        if names.is_empty() {
            let children = store.read(&self.branch)?.info.children;
            names = children
                .iter()
                .filter(|child| {
                    store
                        .read(child)
                        .is_ok_and(|r| crate::wake::unsettled(&r.info.status))
                })
                .cloned()
                .collect();
            if names.is_empty() {
                names = children;
            }
        }
        for name in &names {
            self.require_descendant(name, true)?;
        }
        wait_for(&self.yard, &names, any, timeout, |name| self.inspect(name))
    }

    /// A relative path from a tool call, resolved against this branch's own
    /// worktree: the harness's working directory, whether the call reaches
    /// this process directly or through the broker from another.
    fn in_worktree(&self, path: &Path) -> Result<std::path::PathBuf, Error> {
        match path.is_absolute() {
            true => Ok(path.to_owned()),
            false => Ok(self.store().read(&self.branch)?.info.worktree.join(path)),
        }
    }

    /// Publish `path` as a new artifact of this branch; see
    /// `docs/storage.md`.
    fn publish_artifact(
        &self,
        path: &Path,
        name: Option<String>,
        media_type: Option<String>,
        labels: std::collections::BTreeMap<String, String>,
    ) -> Result<crate::ArtifactRef, Error> {
        let path = self.in_worktree(path)?;
        crate::storage::publish(&self.yard, &self.branch, &path, name, media_type, labels)
    }

    fn list_artifacts(&self) -> Result<Vec<crate::ArtifactRef>, Error> {
        crate::storage::list(&self.yard, &self.branch)
    }

    fn get_artifact(&self, id: &str, out: &Path) -> Result<crate::ArtifactRef, Error> {
        let out = self.in_worktree(out)?;
        crate::storage::get(&self.yard, &self.branch, id, &out)
    }

    fn share_artifact(&self, id: &str, to: &str) -> Result<(), Error> {
        crate::storage::share_artifact(&self.yard, &self.branch, id, to)
    }

    fn export_artifacts(
        &self,
        ids: &[String],
        out: &Path,
    ) -> Result<Vec<crate::BundleEntry>, Error> {
        let out = self.in_worktree(out)?;
        crate::bundle::export_artifacts(&self.yard, &self.branch, ids, &out)
    }

    fn import_artifacts(&self, path: &Path) -> Result<Vec<crate::ArtifactRef>, Error> {
        let path = self.in_worktree(path)?;
        crate::bundle::import_artifacts(&self.yard, &self.branch, &path)
    }

    fn create_scratch(&self, name: &str) -> Result<crate::ScratchArea, Error> {
        crate::storage::create_scratch(&self.yard, &self.branch, name)
    }

    fn list_scratch(&self) -> Result<Vec<crate::ScratchArea>, Error> {
        crate::storage::authorized_scratch(&self.yard, &self.branch)
    }

    fn share_scratch(&self, name: &str, to: &str) -> Result<(), Error> {
        crate::storage::share_scratch(&self.yard, &self.branch, name, to)
    }

    fn lock_scratch(&self, name: &str) -> Result<crate::ScratchLock, Error> {
        crate::storage::lock_scratch(&self.yard, &self.branch, name)
    }

    fn unlock_scratch(&self, name: &str) -> Result<(), Error> {
        crate::storage::unlock_scratch(&self.yard, &self.branch, name)
    }

    /// Send `kind` from this branch to its parent (`question`, `report` or
    /// `escalation`).
    fn send_to_parent(&self, kind: MessageKind, text: &str) -> Result<Message, Error> {
        let parent = self
            .store()
            .read(&self.branch)?
            .info
            .parent
            .ok_or_else(|| Error::Denied(format!("{} has no parent to {kind}", self.branch)))?;
        self.send_message(kind, &parent, text, None)
    }

    fn answer(&self, message_id: u64, text: &str) -> Result<Message, Error> {
        self.require(Capability::Delegate, "answer a descendant")?;
        let question = self
            .store()
            .backend()
            .message(message_id)?
            .ok_or(Error::UnknownMessage(message_id))?;
        self.send_message(MessageKind::Answer, &question.from, text, Some(message_id))
    }

    fn send_message(
        &self,
        kind: MessageKind,
        to: &str,
        text: &str,
        in_reply_to: Option<u64>,
    ) -> Result<Message, Error> {
        let result = self.try_send_message(kind, to, text, in_reply_to);
        self.note("message", to, &result, |m| {
            format!("sent {} #{}", m.kind, m.id)
        });
        result
    }

    fn try_send_message(
        &self,
        kind: MessageKind,
        to: &str,
        text: &str,
        in_reply_to: Option<u64>,
    ) -> Result<Message, Error> {
        if text.trim().is_empty() {
            return Err(Error::Denied(format!("a {kind} needs text")));
        }
        let store = self.store();
        inbox::authorize(&store, &self.branch, kind, to)?;
        if let Some(question_id) = in_reply_to {
            let question = store
                .backend()
                .message(question_id)?
                .ok_or(Error::UnknownMessage(question_id))?;
            if question.to != self.branch {
                return Err(Error::Denied(format!(
                    "message #{question_id} was not sent to {}",
                    self.branch
                )));
            }
        }
        let message = store.backend().send_message(&Message {
            id: 0,
            from: self.branch.clone(),
            to: to.to_owned(),
            kind,
            text: text.to_owned(),
            in_reply_to,
            at_ms: 0,
            delivered: false,
        })?;
        // `authorize` above refused `to == self.branch`, so these are two
        // distinct logs.
        for branch in [self.branch.as_str(), to] {
            if let Ok(mut recorder) = Recorder::open(&store, branch, self.options.observer.clone())
            {
                branchyard_support::best_effort(
                    "record the activity",
                    recorder.record(Activity::Message(message.clone())),
                );
            }
        }
        // The one call site a delivery hook (by default, steering `to`'s
        // running turn) reaches for a message just sent, before it falls
        // back to waiting for `to`'s next turn to start.
        inbox::try_deliver_now(&self.yard, &store, to, &message);
        Ok(message)
    }

    fn inbox(&self) -> Result<Inbox, Error> {
        Ok(Inbox {
            branch: self.branch.clone(),
            messages: self.store().backend().inbox(&self.branch)?,
        })
    }
}

/// Names for messages: `a, b` or `none`.
fn listed(names: &[String]) -> String {
    match names.is_empty() {
        true => "none".into(),
        false => names.join(", "),
    }
}

/// `request` with what `seat` fixes filled in. The request may narrow the
/// seat's limits, envelope and denials, and may not change its harness,
/// check or delegation harnesses.
fn fill(request: &Spawn, name: &str, seat: &Seat, below: &Seats) -> Result<Spawn, Error> {
    let fixed = |what: &str| {
        Err(Error::Denied(format!(
            "seat {name} fixes the child's {what}; spawn it without one"
        )))
    };
    if let Some(asked) = &request.harness {
        if harness::select(Some(asked))?.id != harness::select(Some(&seat.harness))?.id {
            return fixed("harness");
        }
    }
    if request.check.is_some() {
        return fixed("check");
    }
    if request.harnesses.is_some() {
        return fixed("delegation harnesses");
    }
    let limit = seat.budget.to_budget()?;
    let asked = &request.budget;
    fn narrower<T: PartialOrd + Copy + fmt::Debug>(
        what: &str,
        seat: &str,
        asked: Option<T>,
        limit: Option<T>,
    ) -> Result<Option<T>, Error> {
        match (asked, limit) {
            (Some(a), Some(l)) if a > l => Err(Error::Denied(format!(
                "{what} {a:?} exceeds seat {seat}'s {l:?}"
            ))),
            (a, l) => Ok(a.or(l)),
        }
    }
    let budget = Budget {
        max_usd: narrower("max_usd", name, asked.max_usd, limit.max_usd)?,
        max_turns: narrower("max_turns", name, asked.max_turns, limit.max_turns)?,
        max_duration: narrower(
            "a duration of",
            name,
            asked.max_duration,
            limit.max_duration,
        )?,
        stall_after: asked.stall_after,
        stall_action: asked.stall_action,
    };
    let envelope = below.envelope();
    let mut deny = seat.deny.clone();
    for pattern in &request.deny {
        if !deny.contains(pattern) {
            deny.push(pattern.clone());
        }
    }
    // The seat's bindings, and any the request adds; a request may not
    // change the access the seat gives an area.
    let mut bindings = seat.bindings.clone();
    for binding in &request.bindings {
        match bindings.iter().find(|b| b.scratch == binding.scratch) {
            Some(fixed) if fixed.access != binding.access => {
                return Err(Error::Denied(format!(
                    "seat {name} binds scratch area {} {}; spawn it without another access",
                    binding.scratch, fixed.access
                )))
            }
            Some(_) => {}
            None => bindings.push(binding.clone()),
        }
    }
    Ok(Spawn {
        prompt: request.prompt.clone(),
        harness: Some(seat.harness.clone()),
        name: request.name.clone(),
        base: request.base.clone(),
        budget,
        check: seat.check.clone(),
        max_depth: Some(
            request
                .max_depth
                .map_or(envelope.max_depth, |d| d.min(envelope.max_depth)),
        ),
        max_children: Some(
            request
                .max_children
                .map_or(envelope.max_children, |c| c.min(envelope.max_children)),
        ),
        harnesses: Some(envelope.harnesses),
        deny,
        seat: Some(name.to_owned()),
        depends_on: request.depends_on.clone(),
        after: request.after,
        bindings,
        connectors: request.connectors.clone(),
        plan: request.plan,
    })
}

/// The harness's last message since the branch's last prompt: its text
/// after its last tool call, permission request or steered input, or the
/// last text before one when nothing came after. Text the harness wrote
/// earlier in the turn is not run together with it; Claude Code's
/// separate text blocks of one message come apart by a blank line, as its
/// driver writes them. Longer than [`LAST_MESSAGE_MAX`] characters, it
/// keeps its beginning and its end, with the cut marked between. Also
/// used to build a reincarnation's handoff brief.
pub(crate) fn last_message(events: &[RecordedEvent]) -> String {
    let start = events
        .iter()
        .rposition(|e| matches!(e.activity, Activity::Prompt(_)))
        .map_or(0, |i| i + 1);
    let mut messages = vec![String::new()];
    for event in &events[start..] {
        match &event.activity {
            Activity::Harness(Event::MessageDelta { text, .. }) => {
                if let Some(last) = messages.last_mut() {
                    last.push_str(text);
                }
            }
            Activity::Harness(Event::ToolStarted { .. } | Event::PermissionRequested { .. })
            | Activity::Steered { .. } => {
                if messages.last().is_some_and(|m| !m.trim().is_empty()) {
                    messages.push(String::new());
                }
            }
            _ => {}
        }
    }
    let last = messages
        .iter()
        .rev()
        .find(|m| !m.trim().is_empty())
        .map_or("", |m| m.trim());
    elide(last, LAST_MESSAGE_MAX)
}

/// `text` in at most `max` characters: whole when it fits, else its start
/// and its end with an ellipsis line between.
fn elide(text: &str, max: usize) -> String {
    const CUT: &str = "\n…\n";
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let keep = max.saturating_sub(CUT.chars().count());
    let head: String = text.chars().take(keep / 2).collect();
    let tail: String = text.chars().skip(count - (keep - keep / 2)).collect();
    format!("{head}{CUT}{tail}")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyGraphArgs {
    edits: Vec<GraphEdit>,
    expected_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetArgs {
    branch: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    branches: Option<Vec<String>>,
    any: Option<bool>,
    timeout_seconds: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegrateArgs {
    branch: Option<String>,
    branches: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsArgs {
    branch: Option<String>,
    cursor: Option<usize>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    branch: String,
    prompt: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovePlanArgs {
    branch: String,
    #[serde(default)]
    edited: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerApprovalArgs {
    id: String,
    allow: bool,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RejectPlanArgs {
    branch: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    replan: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SteerArgs {
    branch: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscardArgs {
    branch: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxArgs {
    #[serde(default)]
    unread: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishArtifactArgs {
    path: String,
    name: Option<String>,
    media_type: Option<String>,
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextArgs {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskArgs {
    text: String,
    /// Block for up to this many seconds for an answer; `None` or `0`
    /// returns as soon as the question is sent.
    #[serde(default)]
    wait_seconds: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetArtifactArgs {
    id: String,
    out: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareArgs {
    id: String,
    to: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameArgs {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareScratchArgs {
    name: String,
    to: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerArgs {
    message_id: u64,
    text: String,
}

fn parse<T: DeserializeOwned>(tool: &str, arguments: Value) -> Result<T, Error> {
    let arguments = match arguments {
        Value::Null => json!({}),
        other => other,
    };
    serde_json::from_value(arguments)
        .map_err(|e| Error::Denied(format!("invalid arguments for {tool}: {e}")))
}

fn required(tool: &str, branch: Option<String>) -> Result<String, Error> {
    branch.ok_or_else(|| Error::Denied(format!("{tool} needs a branch")))
}

fn to_json<T: Serialize>(value: &T) -> Result<Value, Error> {
    serde_json::to_value(value).map_err(|e| Error::State(format!("encode a result: {e}")))
}

/// One operation with its tool's JSON arguments. Every surface ends here or
/// in the typed methods it calls.
pub(crate) fn dispatch(local: &Local, tool: &str, arguments: Value) -> Result<Value, Error> {
    let operation = crate::operations::by_tool(tool)
        .ok_or_else(|| Error::Denied(format!("no delegation tool named {tool}")))?;
    local.require(operation.capability, operation.name)?;
    match tool {
        "spawn" => {
            let spec: SpawnSpec = parse(tool, arguments)?;
            to_json(&local.spawn(&spec.to_spawn()?)?)
        }
        "apply_graph" => {
            let args: ApplyGraphArgs = parse(tool, arguments)?;
            to_json(&local.apply_graph(&args.edits, args.expected_revision)?)
        }
        "graph" => {
            let args: TargetArgs = parse(tool, arguments)?;
            let branch = args.branch.unwrap_or_else(|| local.branch.clone());
            to_json(&local.graph(&branch)?)
        }
        "inspect" => {
            let args: TargetArgs = parse(tool, arguments)?;
            let branch = args.branch.unwrap_or_else(|| local.branch.clone());
            to_json(&local.inspect(&branch)?)
        }
        "events" => {
            let args: EventsArgs = parse(tool, arguments)?;
            let branch = args.branch.unwrap_or_else(|| local.branch.clone());
            to_json(&local.events(&branch, args.cursor, args.limit.unwrap_or(50))?)
        }
        "send" => {
            let args: SendArgs = parse(tool, arguments)?;
            to_json(&local.send(&args.branch, &args.prompt)?)
        }
        "approve_plan" => {
            let args: ApprovePlanArgs = parse(tool, arguments)?;
            to_json(&local.approve_plan(&args.branch, args.edited.as_deref())?)
        }
        "answer_approval" => {
            let args: AnswerApprovalArgs = parse(tool, arguments)?;
            to_json(&local.answer_approval(&args.id, args.allow, args.reason.as_deref())?)
        }
        "reject_plan" => {
            let args: RejectPlanArgs = parse(tool, arguments)?;
            to_json(&local.reject_plan(&args.branch, args.reason.as_deref(), args.replan)?)
        }
        "propose_integration" | "integrate" => {
            let args: IntegrateArgs = parse(tool, arguments)?;
            match (args.branch, args.branches) {
                (Some(branch), None) => to_json(&local.integrate(&branch)?),
                (None, Some(branches)) => to_json(&local.integrate_all(&branches)?),
                _ => Err(Error::Denied(format!(
                    "{tool} needs either branch or branches (an array, integrated together)"
                ))),
            }
        }
        "steer" => {
            let args: SteerArgs = parse(tool, arguments)?;
            to_json(&local.steer(&args.branch, &args.text)?)
        }
        "cancel" => {
            let args: TargetArgs = parse(tool, arguments)?;
            to_json(&local.cancel(&required(tool, args.branch)?)?)
        }
        "discard" => {
            let args: DiscardArgs = parse(tool, arguments)?;
            to_json(&local.discard(&args.branch, args.reason.as_deref())?)
        }
        "children" => {
            let _: NoArgs = parse(tool, arguments)?;
            to_json(&local.children()?)
        }
        "wait" => {
            let args: WaitArgs = parse(tool, arguments)?;
            let timeout = match args.timeout_seconds {
                None => None,
                Some(secs) if secs.is_finite() && secs >= 0.0 => {
                    Some(Duration::try_from_secs_f64(secs).unwrap_or(Duration::MAX))
                }
                Some(secs) => {
                    return Err(Error::Denied(format!(
                        "timeout_seconds must be a number of seconds, not {secs}"
                    )))
                }
            };
            to_json(&local.wait_for(
                args.branches.unwrap_or_default(),
                args.any.unwrap_or(false),
                timeout,
            )?)
        }
        "publish_artifact" => {
            let args: PublishArtifactArgs = parse(tool, arguments)?;
            to_json(&local.publish_artifact(
                Path::new(&args.path),
                args.name,
                args.media_type,
                args.labels,
            )?)
        }
        "list_artifacts" => {
            let _: NoArgs = parse(tool, arguments)?;
            to_json(&local.list_artifacts()?)
        }
        "get_artifact" => {
            let args: GetArtifactArgs = parse(tool, arguments)?;
            to_json(&local.get_artifact(&args.id, Path::new(&args.out))?)
        }
        "share_artifact" => {
            let args: ShareArgs = parse(tool, arguments)?;
            local.share_artifact(&args.id, &args.to)?;
            Ok(Value::Bool(true))
        }
        "create_scratch" => {
            let args: NameArgs = parse(tool, arguments)?;
            to_json(&local.create_scratch(&args.name)?)
        }
        "list_scratch" => {
            let _: NoArgs = parse(tool, arguments)?;
            to_json(&local.list_scratch()?)
        }
        "share_scratch" => {
            let args: ShareScratchArgs = parse(tool, arguments)?;
            local.share_scratch(&args.name, &args.to)?;
            Ok(Value::Bool(true))
        }
        "lock_scratch" => {
            let args: NameArgs = parse(tool, arguments)?;
            to_json(&local.lock_scratch(&args.name)?)
        }
        "unlock_scratch" => {
            let args: NameArgs = parse(tool, arguments)?;
            local.unlock_scratch(&args.name)?;
            Ok(Value::Bool(true))
        }
        "ask" => {
            let args: AskArgs = parse(tool, arguments)?;
            let message = local.send_to_parent(MessageKind::Question, &args.text)?;
            let answer = match args.wait_seconds.filter(|s| *s > 0.0) {
                Some(secs) => inbox::wait_for_answer(
                    &local.store(),
                    message.id,
                    Duration::try_from_secs_f64(secs).unwrap_or(Duration::MAX),
                )?,
                None => None,
            };
            to_json(&Asked { message, answer })
        }
        "report" => {
            let args: TextArgs = parse(tool, arguments)?;
            to_json(&local.send_to_parent(MessageKind::Report, &args.text)?)
        }
        "escalate" => {
            let args: TextArgs = parse(tool, arguments)?;
            to_json(&local.send_to_parent(MessageKind::Escalation, &args.text)?)
        }
        "answer" => {
            let args: AnswerArgs = parse(tool, arguments)?;
            to_json(&local.answer(args.message_id, &args.text)?)
        }
        "inbox" => {
            let args: InboxArgs = parse(tool, arguments)?;
            let mut inbox = local.inbox()?;
            if args.unread {
                inbox.messages.retain(|m| !m.delivered);
            }
            to_json(&inbox)
        }
        other => Err(Error::Denied(format!("no delegation tool named {other}"))),
    }
}

#[allow(clippy::let_underscore_must_use)] // tests: a panic is the failure report
#[cfg(test)]
mod tests {
    use super::*;

    /// One rule for width and budget: a parked parent is live, and a
    /// discarded child is settled like any other.
    #[test]
    fn parked_branches_are_live_and_discarded_ones_settled() {
        assert!(is_live(&BranchStatus::WaitingOnChildren));
        assert!(is_live(&BranchStatus::Running));
        assert!(!is_live(&BranchStatus::Discarded { reason: "x".into() }));
        assert!(!is_live(&BranchStatus::Ready));
    }

    /// A failed shared check says what to wait for, and that its own check
    /// lets a child land alone; the detail carries the same.
    #[test]
    fn a_shared_check_names_the_siblings_still_running() {
        let error = Error::CheckFailed {
            output_tail: "2 failed".into(),
            shared: Some(Box::new(SharedCheck {
                check: vec!["make".into(), "test".into()],
                inherited_from: None,
                siblings: vec!["b".into(), "c".into()],
                unsettled: vec!["c".into()],
                integrate_together: vec!["a".into(), "b".into(), "c".into()],
            })),
        };
        let text = error.to_string();
        for needed in [
            "check failed:\n2 failed\n",
            "Siblings b, c share the same check",
            "`by integrate a b c`, once c settles (`by wait c`)",
            "needs a check of its own (`by spawn --check`)",
        ] {
            assert!(text.contains(needed), "{needed:?} missing from {text}");
        }
        let detail = error.detail().unwrap();
        assert_eq!(detail["unsettled"], json!(["c"]));
        assert!(detail.get("inherited_from").is_none());
        let plain = Error::CheckFailed {
            output_tail: "x".into(),
            shared: None,
        };
        assert_eq!(
            (plain.to_string(), plain.detail()),
            ("check failed:\nx".into(), None)
        );
    }
    use crate::{PermissionDecision, PermissionKey, PermissionRequest};
    use std::fs;
    use std::path::PathBuf;

    struct Temp(PathBuf);

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_store() -> (Temp, Store) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "branchyard-delegation-unit-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let store = Store::open(&dir).unwrap();
        (Temp(dir), store)
    }

    fn record(name: &str, children: &[&str], cost: Option<f64>, limit: Option<f64>) -> Record {
        Record {
            info: BranchInfo {
                name: name.into(),
                git_branch: format!("by/{name}"),
                worktree: PathBuf::from("/w"),
                prompt: "p".into(),
                harness: "gemini-cli".into(),
                profile: "gemini-cli-acp".into(),
                session: None,
                parent: None,
                children: children.iter().map(|c| (*c).to_owned()).collect(),
                depth: 0,
                base: "b".into(),
                candidate: None,
                status: BranchStatus::Ready,
                turns: 1,
                cost_usd: cost,
                created_at: 0,
                stalled: false,
                superseded_by: None,
            },
            created_ms: 0,
            check: None,
            check_inherited: false,
            command: None,
            home: None,
            cost_baseline: None,
            provider: None,
            provision: None,
            grant: Some(Grant {
                envelope: Envelope::default(),
                deny: Vec::new(),
                limits: limit.map(|max_usd| Limits {
                    max_usd: Some(max_usd),
                    ..Limits::default()
                }),
                seats: None,
            }),
            bindings: Vec::new(),
            start_base: None,
            checkpoint: None,
            context: None,
            workspace: None,
            parked: None,
            wakes: 0,
            lost: None,
            sandbox_seed: None,
            actor: None,
            plan: None,
            goal: None,
            deny: Vec::new(),
            removed: Vec::new(),
        }
    }

    /// A record in a rig, with its own seat and its seat's `escalates_to`.
    fn seated(name: &str, parent: Option<&str>, seat: &str, escalates_to: &[&str]) -> Record {
        let mut r = record(name, &[], None, None);
        r.info.parent = parent.map(str::to_owned);
        r.grant.as_mut().unwrap().seats = Some(Seats {
            rig: "r".into(),
            seat: seat.into(),
            delegates_to: Vec::new(),
            escalates_to: escalates_to.iter().map(|s| (*s).to_owned()).collect(),
            table: std::collections::BTreeMap::new(),
        });
        r
    }

    /// The last message is the harness's final one, not every text of
    /// the turn run together; a long one keeps its start and its end.
    #[test]
    fn the_last_message_is_the_final_one_and_long_ones_keep_both_ends() {
        let at = |activity: Activity| RecordedEvent { at_ms: 0, activity };
        let text = |text: &str| {
            at(Activity::Harness(Event::MessageDelta {
                turn: 1,
                text: text.into(),
            }))
        };
        let tool = at(Activity::Harness(Event::ToolStarted {
            turn: 1,
            call_id: "t".into(),
            name: "Bash".into(),
        }));
        let events = vec![
            at(Activity::Prompt("earlier".into())),
            text("Old turn."),
            at(Activity::Prompt("do it".into())),
            text("Let me look."),
            tool.clone(),
            text("Found it"),
            text(", fixing."),
            tool,
            text("Done: all four modules pass."),
            text("\n\nNothing else changed."),
            at(Activity::Harness(Event::TurnEnded {
                turn: 1,
                outcome: crate::TurnOutcome::Completed,
            })),
        ];
        assert_eq!(
            last_message(&events),
            "Done: all four modules pass.\n\nNothing else changed."
        );
        // A turn that ends on a tool call keeps the text before it.
        assert_eq!(last_message(&events[..7]), "Found it, fixing.");
        assert_eq!(last_message(&events[..3]), "");

        let long = format!("BEGIN{}END", "x".repeat(2 * LAST_MESSAGE_MAX));
        let cut = last_message(&[text(&long)]);
        assert_eq!(cut.chars().count(), LAST_MESSAGE_MAX);
        assert!(cut.starts_with("BEGIN") && cut.ends_with("END"), "{cut}");
        assert!(cut.contains("\n…\n"));
    }

    #[test]
    fn escalation_reaches_the_parent_always_and_further_up_only_when_the_seat_allows() {
        let (_temp, store) = temp_store();
        let root = seated("root", None, "root", &[]);
        store.write(&root).unwrap();
        let mid = seated("mid", Some("root"), "mid", &[]);
        store.write(&mid).unwrap();
        let mut leaf = seated("leaf", Some("mid"), "leaf", &[]);
        store.write(&leaf).unwrap();
        store.add_child("root", "mid").unwrap();
        store.add_child("mid", "leaf").unwrap();

        // Always allowed: escalate to the direct parent.
        crate::inbox::authorize(&store, "leaf", MessageKind::Escalation, "mid").unwrap();
        // Not yet allowed: nothing names root in leaf's escalates_to.
        assert!(matches!(
            crate::inbox::authorize(&store, "leaf", MessageKind::Escalation, "root"),
            Err(Error::Denied(_))
        ));

        leaf.grant
            .as_mut()
            .unwrap()
            .seats
            .as_mut()
            .unwrap()
            .escalates_to = vec!["root".into()];
        store.write(&leaf).unwrap();
        crate::inbox::authorize(&store, "leaf", MessageKind::Escalation, "root").unwrap();

        // A question or report never reaches beyond the parent, whatever
        // escalates_to says.
        assert!(matches!(
            crate::inbox::authorize(&store, "leaf", MessageKind::Question, "root"),
            Err(Error::Denied(_))
        ));
        // A parent may always answer a further descendant, not only its
        // direct child.
        crate::inbox::authorize(&store, "root", MessageKind::Answer, "leaf").unwrap();
    }

    #[test]
    fn a_live_child_holds_its_limit_and_a_settled_one_what_it_spent() {
        let (_temp, store) = temp_store();
        // root -> a (running, limit 0.5, spent 0.1) -> g (spent 0.3)
        //      -> b (ready, limit 0.2, spent 0.1) -> h (running, limit
        //           0.6, spent 0.4)
        //      -> c (ready, no limit, spent 0.05)
        //      -> d (merged, limit 0.9, spent 0.25)
        let with = |mut r: Record, status: BranchStatus, parent: &str| {
            r.info.status = status;
            r.info.parent = Some(parent.to_owned());
            r
        };
        let merged = BranchStatus::Merged {
            target: "by/root".into(),
            commit: "c".into(),
        };
        for r in [
            record("root", &["a", "b", "c", "d", "gone"], Some(0.2), None),
            with(
                record("a", &["g"], Some(0.1), Some(0.5)),
                BranchStatus::Running,
                "root",
            ),
            with(record("g", &[], Some(0.3), None), BranchStatus::Ready, "a"),
            with(
                record("b", &["h"], Some(0.1), Some(0.2)),
                BranchStatus::Ready,
                "root",
            ),
            with(
                record("h", &[], Some(0.4), Some(0.6)),
                BranchStatus::Running,
                "b",
            ),
            with(
                record("c", &[], Some(0.05), None),
                BranchStatus::Ready,
                "root",
            ),
            with(record("d", &[], Some(0.25), Some(0.9)), merged, "root"),
        ] {
            store.write(&r).unwrap();
        }
        let root = store.read("root").unwrap();
        // a holds its whole limit, 0.5. b is settled but its child h runs:
        // b's own 0.1 and h's limit 0.6. c and d hold what they spent.
        let held = held(&store, &root);
        assert_eq!(held.live, 1);
        assert!((held.live_usd - 0.5).abs() < 1e-9, "{held:?}");
        assert!(
            (held.settled_usd - (0.7 + 0.05 + 0.25)).abs() < 1e-9,
            "{held:?}"
        );
        assert!((reserved(&store, &root) - 1.5).abs() < 1e-9);
        let spent = subtree_spent(&store, &root);
        assert!((spent - 1.4).abs() < 1e-9, "{spent}");
        let names: Vec<String> = descendants(&store, "root")
            .unwrap()
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names, ["a", "b", "c", "d", "g", "h"]);

        // Removing a child keeps its spend in its parent's ledger: the
        // subtree cost and what root has left do not change.
        store.delete("d").unwrap();
        let root = store.read("root").unwrap();
        assert_eq!(root.removed.len(), 1);
        assert_eq!(root.removed[0].name, "d");
        assert!((subtree_spent(&store, &root) - 1.4).abs() < 1e-9);
        assert!((reserved(&store, &root) - 1.5).abs() < 1e-9);
        // A stale write of root keeps the ledger, as it keeps children.
        store.write(&record("root", &[], Some(0.2), None)).unwrap();
        assert_eq!(store.read("root").unwrap().removed.len(), 1);
        // Removing a subtree's root keeps all it spent: b and its h.
        store.delete("b").unwrap();
        let root = store.read("root").unwrap();
        assert!(
            (root.removed[1].spent_usd - 0.5).abs() < 1e-9,
            "{:?}",
            root.removed
        );
    }

    #[test]
    fn children_survive_a_stale_write_of_their_parent() {
        let (_temp, store) = temp_store();
        let stale = record("root", &[], None, None);
        store.write(&stale).unwrap();
        store.add_child("root", "kid").unwrap();
        store.write(&stale).unwrap();
        assert_eq!(store.read("root").unwrap().info.children, ["kid"]);
    }

    #[test]
    fn a_running_child_is_found_even_when_added_after_the_parent_was_read() {
        let (_temp, store) = temp_store();
        let root = record("root", &[], None, None);
        store.write(&root).unwrap();
        assert!(!any_child_running(&store, "root"), "no children at all yet");
        let mut idle_child = record("idle", &[], None, None);
        idle_child.info.status = BranchStatus::Ready;
        store.write(&idle_child).unwrap();
        store.add_child("root", "idle").unwrap();
        assert!(
            !any_child_running(&store, "root"),
            "its only child is not running"
        );
        let mut busy_child = record("busy", &[], None, None);
        busy_child.info.status = BranchStatus::Running;
        store.write(&busy_child).unwrap();
        // Added after `root`'s own record was last read: a spawn during the
        // parent's own turn, which stall detection must still see.
        store.add_child("root", "busy").unwrap();
        assert!(any_child_running(&store, "root"));
    }

    #[test]
    fn imposed_limits_only_narrow_the_callers_budget() {
        let mut child = record("c", &[], None, Some(0.5));
        child
            .grant
            .as_mut()
            .unwrap()
            .limits
            .as_mut()
            .unwrap()
            .max_turns = Some(2);
        let wide = Budget::usd(3.0).turns(10).duration(Duration::from_secs(60));
        let narrowed = effective_budget(&child, &wide);
        assert_eq!(
            narrowed,
            Budget {
                max_usd: Some(0.5),
                max_turns: Some(2),
                max_duration: Some(Duration::from_secs(60)),
                ..Budget::default()
            }
        );
        let tighter = Budget::usd(0.1);
        assert_eq!(effective_budget(&child, &tighter).max_usd, Some(0.1));
        let root = record("r", &[], None, None);
        assert_eq!(effective_budget(&root, &wide), wide);
    }

    #[test]
    fn imposed_denials_come_before_every_rule_and_the_fallback() {
        let request = |tool: &str| PermissionRequest {
            key: PermissionKey("1".into()),
            tool: tool.into(),
            input: Value::Null,
        };
        let mut child = record("c", &[], None, None);
        child.grant.as_mut().unwrap().deny = vec!["Bash".into(), "mcp__*".into()];
        let parent = Policy::allow_all().allow("Bash");
        let policy = effective_policy(&child, &parent);
        for tool in ["Bash", "mcp__branchyard__spawn"] {
            assert!(
                matches!(
                    policy.decide("c", &request(tool)),
                    PermissionDecision::Deny { .. }
                ),
                "{tool}"
            );
        }
        assert_eq!(
            policy.decide("c", &request("Edit")),
            PermissionDecision::Allow
        );
        // Without denials the parent's policy is used as it is.
        let plain = record("p", &[], None, None);
        assert_eq!(
            effective_policy(&plain, &parent).decide("p", &request("Bash")),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn envelopes_shrink_from_parent_to_child() {
        let own = profiles::by_id("gemini-cli-acp").unwrap();
        let parent = Envelope {
            max_depth: 3,
            max_children: 5,
            harnesses: vec!["gemini-cli".into(), "qwen-code-acp".into()],
            max_wakes: 3,
        };
        assert!(parent.allows(profiles::by_id("qwen-code-acp").unwrap(), own));
        assert!(!parent.allows(profiles::by_id("goose-acp").unwrap(), own));
        let child = parent.child(&Spawn::default(), own).unwrap();
        assert_eq!((child.max_depth, child.max_children), (2, 5));
        assert_eq!(child.harnesses, parent.harnesses);
        let asked = Spawn {
            max_depth: Some(9),
            max_children: Some(1),
            harnesses: Some(vec!["qwen-code".into()]),
            ..Spawn::default()
        };
        let child = parent.child(&asked, own).unwrap();
        assert_eq!(
            child,
            Envelope {
                max_depth: 2,
                max_children: 1,
                harnesses: vec!["qwen-code".into()],
                max_wakes: 3,
            }
        );
        let wider = Spawn {
            harnesses: Some(vec!["goose".into()]),
            ..Spawn::default()
        };
        assert!(matches!(parent.child(&wider, own), Err(Error::Denied(_))));
        // An empty list means the parent's own profile only.
        let own_only = Envelope::default();
        assert!(own_only.allows(own, own));
        assert!(!own_only.allows(profiles::by_id("qwen-code-acp").unwrap(), own));
    }
}
