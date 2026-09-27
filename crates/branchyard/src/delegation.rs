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

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use branchyard_harness::profiles::{self, Profile};
use branchyard_harness::SessionMode;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::broker::Remote;
use crate::engine::{self, Turn};
use crate::projection::{lock, same_token, ENV_BRANCH, ENV_ROOT, ENV_TOKEN};
use crate::record::{self, Recorder};
use crate::recover;
use crate::run::{self, NewBranch, Prepared};
use crate::seats::{Seat, Seats};
use crate::state::{Record, Store};
use crate::{
    git, harness, inbox, names, ops, Activity, BranchInfo, BranchStatus, Budget, CandidateInfo,
    Error, Event, Merged, Message, MessageKind, Policy, RecordedEvent, Rule, Steer, SteerState,
    TaskOptions, Yard,
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
}

impl Default for Envelope {
    /// Children only, at most four, on the parent's own profile.
    fn default() -> Self {
        Envelope {
            max_depth: 1,
            max_children: 4,
            harnesses: Vec::new(),
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

    fn allowed_text(&self, own: &Profile) -> String {
        match self.harnesses.is_empty() {
            true => own.id.to_owned(),
            false => self.harnesses.join(", "),
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
        })
    }
}

/// Limits a parent put on a delegated child. They bound every turn of the
/// child, whoever sends it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Limits {
    pub max_usd: Option<f64>,
    pub max_turns: Option<u32>,
    pub max_duration_ms: Option<u64>,
}

/// A branch's delegation record.
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
        let mut args = json!({"prompt": self.prompt});
        let mut set = |key: &str, value: Value| {
            if !value.is_null() {
                args[key] = value;
            }
        };
        set("harness", json!(self.harness));
        set("name", json!(self.name));
        set("base", json!(self.base));
        set("check", json!(self.check));
        set("max_depth", json!(self.max_depth));
        set("max_children", json!(self.max_children));
        set("harnesses", json!(self.harnesses));
        set("seat", json!(self.seat));
        if !self.deny.is_empty() {
            set("deny", json!(self.deny));
        }
        let budget = ChildBudget::from(&self.budget);
        if budget != ChildBudget::default() {
            set("budget", json!(budget));
        }
        args
    }
}

/// A child's limits as the tools show them.
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
    fn to_budget(&self) -> Result<Budget, Error> {
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
}

/// A descendant whose next turn was started.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sent {
    pub name: String,
    pub status: BranchStatus,
}

/// The branches a cancel asked to stop.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cancelled {
    pub cancelled: Vec<String>,
}

/// A branch's subtree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Children {
    pub branch: String,
    /// Every descendant, oldest first.
    pub descendants: Vec<BranchInfo>,
}

/// A branch's own inbox: every message addressed to it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inbox {
    pub branch: String,
    pub messages: Vec<Message>,
}

/// A question sent, and its answer if one arrived within the wait.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Asked {
    pub message: Message,
    /// `None` when asked without `--wait`, or the wait passed with no
    /// answer yet; ask again, or `inbox` to check later.
    pub answer: Option<Message>,
}

/// A branch as a delegating parent sees it.
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
    /// The branch's own cost estimate; `None` when its harness reports none.
    pub cost_usd: Option<f64>,
    /// Its own and its descendants' reported costs. Unreported costs count
    /// as zero here.
    pub subtree_cost_usd: f64,
    /// The cost limit its turns run under, when there is one.
    pub max_usd: Option<f64>,
    /// What is left of `max_usd` after its own spend and its children's
    /// reservations: what it can still spend or grant.
    pub remaining_usd: Option<f64>,
    pub envelope: Option<Envelope>,
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
}

/// Recorded events from `cursor` on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventPage {
    pub branch: String,
    pub events: Vec<RecordedEvent>,
    /// Pass back to continue after the last event returned.
    pub next_cursor: usize,
    /// Events recorded so far.
    pub total: usize,
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

    /// Merge a descendant's candidate into this branch's own git branch
    /// (`by/<name>`, never the user's branches), after the descendant's
    /// check passes on the exact merge. This branch's uncommitted work is
    /// committed first, and its worktree moves to the merge.
    pub fn integrate(&self, branch: &str) -> Result<Merged, Error> {
        match &self.via {
            Via::Local(local) => local.integrate(branch),
            Via::Remote(_) => self.typed("propose_integration", json!({ "branch": branch })),
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

    /// This branch's descendants.
    pub fn children(&self) -> Result<Children, Error> {
        match &self.via {
            Via::Local(local) => local.children(),
            Via::Remote(_) => self.typed("children", json!({})),
        }
    }

    /// Publish `path` as a new immutable artifact of this branch; see
    /// `docs/storage.md`.
    pub fn publish_artifact(
        &self,
        path: &Path,
        name: Option<String>,
        labels: std::collections::BTreeMap<String, String>,
    ) -> Result<crate::ArtifactRef, Error> {
        match &self.via {
            Via::Local(local) => local.publish_artifact(path, name, None, labels),
            Via::Remote(_) => self.typed(
                "publish_artifact",
                json!({"path": path, "name": name, "labels": labels}),
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
            if inspection.status != BranchStatus::Running {
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
                    let store = local.store();
                    store.wait(left, || {
                        Ok(
                            (store.read(branch)?.info.status != BranchStatus::Running)
                                .then_some(()),
                        )
                    })?;
                }
                Via::Remote(_) => std::thread::sleep(WAIT_POLL.min(left)),
            }
        }
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

/// The caller's policy with the denials a parent imposed put first.
pub(crate) fn effective_policy(record: &Record, policy: &Policy) -> Policy {
    match record.grant.as_ref().map(|g| &g.deny) {
        Some(deny) if !deny.is_empty() => narrowed(policy, deny),
        _ => policy.clone(),
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
    }
}

/// What `record`'s children hold of its budget: each child's limit, or its
/// subtree's spend if that is more.
pub(crate) fn reserved(store: &Store, record: &Record) -> f64 {
    record
        .info
        .children
        .iter()
        .filter_map(|child| store.read(child).ok())
        .map(|child| {
            let limit = child
                .grant
                .as_ref()
                .and_then(|g| g.limits.as_ref())
                .and_then(|l| l.max_usd);
            let spent = subtree_spent(store, &child, &mut BTreeSet::new());
            limit.map_or(spent, |limit| limit.max(spent))
        })
        .sum()
}

/// Reported costs of `record` and its descendants; unreported counts as 0.
fn subtree_spent(store: &Store, record: &Record, seen: &mut BTreeSet<String>) -> f64 {
    if !seen.insert(record.info.name.clone()) {
        return 0.0;
    }
    record.info.cost_usd.unwrap_or(0.0)
        + record
            .info
            .children
            .iter()
            .filter_map(|child| store.read(child).ok())
            .map(|child| subtree_spent(store, &child, seen))
            .sum::<f64>()
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
pub(crate) fn cancel_tree(store: &Store, name: &str, by: &str) -> Result<Vec<String>, Error> {
    let mut targets = vec![store.read(name)?.info];
    targets.extend(descendants(store, name)?);
    let mut cancelled = Vec::new();
    for info in targets {
        if store.request_cancel(&info.name, by, true)? {
            cancelled.push(info.name);
        }
    }
    Ok(cancelled)
}

/// Wait until no descendant of `name` is running a turn, wherever it runs.
/// Children on this process's threads are joined; a child another process
/// drives is waited for through its durable status, and one whose engine
/// stopped is recovered.
pub(crate) fn wait_subtree(yard: &Yard, name: &str) -> Result<Vec<BranchInfo>, Error> {
    let store = yard.store();
    loop {
        let all = descendants(&store, name)?;
        let names: BTreeSet<String> = all.iter().map(|info| info.name.clone()).collect();
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
                let _ = handle.join();
            }
            continue;
        }
        let running: Vec<&BranchInfo> = all
            .iter()
            .filter(|info| info.status == BranchStatus::Running)
            .collect();
        if running.is_empty() {
            return Ok(all);
        }
        for info in running {
            recover::settle(yard, &info.name)?;
        }
        store.wait(SETTLE_EVERY, || {
            let settled = descendants(&store, name)?
                .iter()
                .all(|info| info.status != BranchStatus::Running);
            Ok(settled.then_some(()))
        })?;
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
        let live = self.cost.as_ref().and_then(|cost| *lock(cost));
        record.info.cost_usd.unwrap_or(0.0).max(live.unwrap_or(0.0))
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
        let _ = recorder.record(Activity::Delegation {
            tool: tool.to_owned(),
            branch: branch.to_owned(),
            outcome,
            refused,
        });
    }

    fn spawn(&self, request: &Spawn) -> Result<Spawned, Error> {
        let result = self.try_spawn(request);
        let target = match &result {
            Ok(spawned) => spawned.name.clone(),
            Err(_) => request.name.clone().unwrap_or_default(),
        };
        self.note("spawn", &target, &result, |s| {
            format!("started on {}", s.profile)
        });
        result
    }

    fn try_spawn(&self, request: &Spawn) -> Result<Spawned, Error> {
        let store = self.store();
        let _spawning = lock(&self.yard.hub.spawning);
        let caller = store.read(&self.branch)?;
        let grant = caller
            .grant
            .clone()
            .ok_or_else(|| Error::Denied(format!("{} was not given delegation", self.branch)))?;
        if !grant.can_spawn() {
            return Err(Error::Denied(format!(
                "{} may not create children: its envelope's max_depth is 0",
                self.branch
            )));
        }
        let live = caller
            .info
            .children
            .iter()
            .filter(|child| store.read(child).is_ok())
            .count();
        if live >= grant.envelope.max_children as usize {
            return Err(Error::Denied(format!(
                "{} already has {live} children, its envelope's max_children",
                self.branch
            )));
        }
        if request.prompt.trim().is_empty() {
            return Err(Error::Denied("a child needs a prompt".into()));
        }
        let seated = self.seat(&caller, &grant, request)?;
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
                profile.id,
                grant.envelope.allowed_text(own)
            )));
        }
        let envelope = grant.envelope.child(request, own)?;
        let limits = self.child_limits(&caller, &request.budget)?;
        let mut deny = grant.deny.clone();
        for pattern in &request.deny {
            if !deny.contains(pattern) {
                deny.push(pattern.clone());
            }
        }
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
        let provision = match seat.and_then(|s| s.provision.clone()) {
            Some(own) => Some(own),
            None => caller.provision.clone(),
        };
        crate::provisioning::check(
            provision.as_ref(),
            isolated || crate::placement::sandboxed(caller.provider.as_ref()),
        )?;
        let base = match &request.base {
            Some(rev) => run::resolve_base(&self.yard, Some(rev))?,
            None => self.current_work(&caller, "snapshot before delegating")?,
        };
        // A seat's child is named after its parent and seat by default.
        let stem = match (&seated, &request.name) {
            (Some((seat, _, _)), None) => format!("{}-{seat}", self.branch),
            _ => request.prompt.clone(),
        };
        let name =
            names::reserve(&store, &self.yard.root, request.name.as_deref(), &stem, &[])?.remove(0);
        let record = run::create(
            &self.yard,
            NewBranch {
                name: &name,
                prompt: &request.prompt,
                profile,
                base,
                parent: Some(self.branch.clone()),
                check: request.check.clone().or(caller.check.clone()),
                command,
                home: isolated.then(|| store.home(&name)),
                cost_baseline: None,
                provider: caller.provider.clone(),
                grant: Some(child_grant),
                depth: caller.info.depth + 1,
                provision,
            },
        )
        .inspect_err(|_| store.release(&name))?;
        let (record, lease) = record;
        store.add_child(&self.branch, &name)?;
        let info = record.info.clone();
        self.start(
            Prepared {
                record,
                lease,
                profile,
                command: launch,
                mode: SessionMode::Fresh,
                note: None,
            },
            request.prompt.clone(),
        )?;
        Ok(Spawned {
            name: info.name,
            git_branch: info.git_branch,
            harness: info.harness,
            profile: info.profile,
            base: info.base,
            depth: info.depth,
            status: info.status,
            budget: ChildBudget {
                max_usd: limits.max_usd,
                max_turns: limits.max_turns,
                max_minutes: limits.max_duration_ms.map(|ms| ms as f64 / 60_000.0),
            },
            seat: seated.map(|(name, _, _)| name),
        })
    }

    /// The seat `request` fills, with the seats below it; `None` for a
    /// branch outside a rig that names none. A branch in a rig must name
    /// one its own seat delegates to, and may not fill it more often than
    /// the seat's instances allow.
    fn seat(
        &self,
        caller: &Record,
        grant: &Grant,
        request: &Spawn,
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
    fn child_limits(&self, caller: &Record, asked: &Budget) -> Result<Limits, Error> {
        let bounds = &self.options.budget;
        let max_usd = match (bounds.max_usd, asked.max_usd) {
            (_, Some(ask)) if !(ask.is_finite() && ask > 0.0) => {
                return Err(Error::Denied(format!(
                    "a child's max_usd must be a positive number, not {ask}"
                )))
            }
            (Some(_), None) => {
                let remaining = self.remaining(caller).unwrap_or(0.0).max(0.0);
                return Err(Error::Denied(format!(
                    "{} has a cost limit, so a child needs max_usd; ${remaining:.4} remains",
                    self.branch
                )));
            }
            (Some(_), Some(ask)) => {
                let remaining = self.remaining(caller).unwrap_or(0.0);
                if ask > remaining + EPSILON_USD {
                    return Err(Error::Denied(format!(
                        "max_usd {ask} exceeds what {} has left, ${:.4}",
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
                    "max_turns {ask} exceeds {}'s {limit}",
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
            .snapshot(&format!("{}: {why}", caller.info.git_branch))
            .map_err(git::error)?;
        git::local_branch(&self.yard.root, &caller.info.git_branch)?
            .ok_or_else(|| Error::Git(format!("{} is missing", caller.info.git_branch)))
    }

    /// The options a child's turn runs with: the parent's policy, observer
    /// and tools. Its budget and denials come from its own record.
    fn child_options(&self) -> TaskOptions {
        TaskOptions {
            policy: self.options.policy.clone(),
            observer: self.options.observer.clone(),
            delegation_cli: self.options.delegation_cli.clone(),
            delegation_server: self.options.delegation_server.clone(),
            ..TaskOptions::default()
        }
    }

    /// Run a turn on a thread of this process.
    fn start(&self, prepared: Prepared, prompt: String) -> Result<(), Error> {
        let Prepared {
            record,
            lease,
            profile,
            command,
            mode,
            note,
        } = prepared;
        let name = record.info.name.clone();
        let yard = self.yard.clone();
        let options = self.child_options();
        let started = std::thread::Builder::new()
            .name(format!("by-{name}"))
            .spawn(move || {
                // The outcome is the branch's status; errors are recorded there.
                let _ = engine::execute(
                    Turn {
                        yard: &yard,
                        record,
                        profile,
                        command,
                        mode,
                        prompt: &prompt,
                        options: &options,
                        fork_source: None,
                        note,
                    },
                    lease,
                );
            });
        match started {
            Ok(handle) => {
                let previous = lock(&self.yard.hub.running).insert(name, handle);
                if let Some(previous) = previous {
                    let _ = previous.join();
                }
                Ok(())
            }
            Err(error) => {
                let store = self.store();
                if let Ok(mut record) = store.read(&name) {
                    record.info.status = BranchStatus::Failed {
                        reason: format!("could not start a thread: {error}"),
                    };
                    let _ = store.write(&record);
                }
                Err(Error::Io(error))
            }
        }
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
        let remaining_usd = match own {
            true => self.remaining(&record),
            false => max_usd.map(|limit| {
                limit - record.info.cost_usd.unwrap_or(0.0) - reserved(&store, &record)
            }),
        };
        let events = record::read(&store, branch)?;
        let info = record.info.clone();
        let seats = record.grant.as_ref().and_then(|g| g.seats.as_ref());
        let (seat, may_spawn) = match seats {
            Some(seats) => (Some(seats.seat.clone()), seats.delegates_to.clone()),
            None => (None, Vec::new()),
        };
        Ok(Inspection {
            subtree_cost_usd: subtree_spent(&store, &record, &mut BTreeSet::new()),
            name: info.name,
            status: info.status,
            harness: info.harness,
            profile: info.profile,
            parent: info.parent,
            children: info.children,
            depth: info.depth,
            turns: info.turns,
            candidate: info.candidate,
            cost_usd: info.cost_usd,
            max_usd,
            remaining_usd: remaining_usd.map(|r| r.max(0.0)),
            envelope: record.grant.map(|g| g.envelope),
            last_message: last_message(&events),
            seat,
            seats: may_spawn,
            stalled: info.stalled,
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
        let prepared = run::prepare_send(&self.yard, branch, &self.child_options(), true)?;
        let info = prepared.record.info.clone();
        self.start(prepared, prompt.to_owned())?;
        Ok(Sent {
            name: info.name,
            status: info.status,
        })
    }

    fn integrate(&self, branch: &str) -> Result<Merged, Error> {
        let result = self.try_integrate(branch);
        self.note("integrate", branch, &result, |m| {
            format!("merged into {} as {}", m.target, m.commit)
        });
        result
    }

    fn try_integrate(&self, branch: &str) -> Result<Merged, Error> {
        self.require_descendant(branch, false)?;
        let store = self.store();
        let child = store.read(branch)?;
        if child.info.status == BranchStatus::Running {
            return Err(Error::Running(branch.to_owned()));
        }
        let caller = store.read(&self.branch)?;
        self.current_work(&caller, &format!("snapshot before integrating {branch}"))?;
        ops::merge(&self.yard, branch, &caller.info.git_branch)
    }

    fn steer(&self, branch: &str, text: &str) -> Result<Steer, Error> {
        let result = self.require_descendant(branch, false).and_then(|()| {
            let steer = crate::steer::request(&self.yard, branch, text, &self.branch)?;
            crate::steer::wait(&self.store(), branch, steer.id, STEER_WAIT)
        });
        self.note("steer", branch, &result, |s| match &s.state {
            SteerState::Refused { reason } => format!("steered input {} refused: {reason}", s.id),
            SteerState::Pending => format!("steered input {} queued", s.id),
            SteerState::Delivered | SteerState::Accepted => {
                format!("steered input {} delivered", s.id)
            }
        });
        result
    }

    fn cancel(&self, branch: &str) -> Result<Cancelled, Error> {
        let result = self
            .require_descendant(branch, false)
            .and_then(|()| cancel_tree(&self.store(), branch, &self.branch))
            .map(|cancelled| Cancelled { cancelled });
        self.note("cancel", branch, &result, |c| {
            match c.cancelled.is_empty() {
                true => "nothing was running".into(),
                false => format!("asked {} to stop", c.cancelled.join(", ")),
            }
        });
        result
    }

    fn children(&self) -> Result<Children, Error> {
        Ok(Children {
            branch: self.branch.clone(),
            descendants: descendants(&self.store(), &self.branch)?,
        })
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
                let _ = recorder.record(Activity::Message(message.clone()));
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
    })
}

/// The text of the last turn, at most [`LAST_MESSAGE_MAX`] characters.
/// The harness's text since the branch's last prompt, truncated from the
/// front. Also used to build a reincarnation's handoff brief.
pub(crate) fn last_message(events: &[RecordedEvent]) -> String {
    let start = events
        .iter()
        .rposition(|e| matches!(e.activity, Activity::Prompt(_)))
        .map_or(0, |i| i + 1);
    let text: String = events[start..]
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Harness(Event::MessageDelta { text, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let count = text.chars().count();
    match count > LAST_MESSAGE_MAX {
        true => text.chars().skip(count - LAST_MESSAGE_MAX).collect(),
        false => text,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    prompt: String,
    harness: Option<String>,
    name: Option<String>,
    base: Option<String>,
    budget: Option<ChildBudget>,
    check: Option<Vec<String>>,
    max_depth: Option<u32>,
    max_children: Option<u32>,
    harnesses: Option<Vec<String>>,
    #[serde(default)]
    deny: Vec<String>,
    seat: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetArgs {
    branch: Option<String>,
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
struct SteerArgs {
    branch: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

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
    match tool {
        "spawn" => {
            let args: SpawnArgs = parse(tool, arguments)?;
            let budget = args.budget.unwrap_or_default().to_budget()?;
            to_json(&local.spawn(&Spawn {
                prompt: args.prompt,
                harness: args.harness,
                name: args.name,
                base: args.base,
                budget,
                check: args.check,
                max_depth: args.max_depth,
                max_children: args.max_children,
                harnesses: args.harnesses,
                deny: args.deny,
                seat: args.seat,
            })?)
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
        "propose_integration" | "integrate" => {
            let args: TargetArgs = parse(tool, arguments)?;
            to_json(&local.integrate(&required(tool, args.branch)?)?)
        }
        "steer" => {
            let args: SteerArgs = parse(tool, arguments)?;
            to_json(&local.steer(&args.branch, &args.text)?)
        }
        "cancel" => {
            let args: TargetArgs = parse(tool, arguments)?;
            to_json(&local.cancel(&required(tool, args.branch)?)?)
        }
        "children" => {
            let _: NoArgs = parse(tool, arguments)?;
            to_json(&local.children()?)
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
            let _: NoArgs = parse(tool, arguments)?;
            to_json(&local.inbox()?)
        }
        other => Err(Error::Denied(format!("no delegation tool named {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn a_childs_reservation_is_its_limit_or_its_subtrees_spend() {
        let (_temp, store) = temp_store();
        // root -> a (limit 0.5, spent 0.1) -> g (spent 0.3)
        //      -> b (limit 0.2, spent 0.1) -> h (spent 0.4, over b's limit)
        //      -> c (no limit, spent 0.05)
        for r in [
            record("root", &["a", "b", "c", "gone"], Some(0.2), None),
            record("a", &["g"], Some(0.1), Some(0.5)),
            record("g", &[], Some(0.3), None),
            record("b", &["h"], Some(0.1), Some(0.2)),
            record("h", &[], Some(0.4), None),
            record("c", &[], Some(0.05), None),
        ] {
            store.write(&r).unwrap();
        }
        let root = store.read("root").unwrap();
        // a reserves 0.5; b's subtree spent 0.5 > its 0.2; c its 0.05.
        assert!((reserved(&store, &root) - 1.05).abs() < 1e-9);
        let spent = subtree_spent(&store, &root, &mut BTreeSet::new());
        assert!((spent - 1.15).abs() < 1e-9, "{spent}");
        let names: Vec<String> = descendants(&store, "root")
            .unwrap()
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names, ["a", "b", "c", "g", "h"]);
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
                harnesses: vec!["qwen-code".into()]
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
