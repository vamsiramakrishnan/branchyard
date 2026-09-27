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
use crate::run::{self, NewBranch};
use crate::state::{Record, Store};
use crate::{
    git, harness, names, ops, Activity, BranchInfo, BranchStatus, Budget, CandidateInfo, Error,
    Event, Merged, Policy, RecordedEvent, Rule, TaskOptions, Yard,
};

/// Most events one `events` call returns.
const EVENTS_MAX: usize = 200;
/// Characters of the last message `inspect` returns.
const LAST_MESSAGE_MAX: usize = 4000;
/// Cost comparisons tolerate float rounding of this much.
const EPSILON_USD: f64 = 1e-9;
/// How often [`Delegate::wait`] looks.
const WAIT_POLL: Duration = Duration::from_millis(100);

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
}

impl Grant {
    pub fn root(envelope: Envelope) -> Grant {
        Grant {
            envelope,
            deny: Vec::new(),
            limits: None,
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

    /// Inspect `branch` until it is not running a turn, for up to
    /// `timeout`. Fails with [`Error::Running`] if it still is.
    pub fn wait(&self, branch: &str, timeout: Duration) -> Result<Inspection, Error> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            let inspection = self.inspect(branch)?;
            if inspection.status != BranchStatus::Running {
                return Ok(inspection);
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(Error::Running(branch.to_owned()));
            }
            std::thread::sleep(WAIT_POLL);
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

/// Ask `name`'s running turn and every running turn below it to stop, on
/// behalf of `by`.
pub(crate) fn cancel_tree(store: &Store, name: &str, by: &str) -> Result<Vec<String>, Error> {
    let mut targets = vec![store.read(name)?.info];
    targets.extend(descendants(store, name)?);
    let mut cancelled = Vec::new();
    for info in targets {
        if info.status == BranchStatus::Running {
            store.request_cancel(&info.name, by)?;
            cancelled.push(info.name);
        }
    }
    Ok(cancelled)
}

pub(crate) fn wait_subtree(yard: &Yard, name: &str) -> Result<Vec<BranchInfo>, Error> {
    let store = yard.store();
    loop {
        let names: BTreeSet<String> = descendants(&store, name)?
            .into_iter()
            .map(|info| info.name)
            .collect();
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
        if handles.is_empty() {
            return descendants(&store, name);
        }
        for handle in handles {
            // A panic in a child's thread has already been reported by the
            // runtime; its record says `running` and the others go on.
            let _ = handle.join();
        }
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
        harness::check_available(profile.harness, &launch)?;
        let base = match &request.base {
            Some(rev) => run::resolve_base(&self.yard, Some(rev))?,
            None => self.current_work(&caller, "snapshot before delegating")?,
        };
        let name = names::reserve(
            &store,
            &self.yard.root,
            request.name.as_deref(),
            &request.prompt,
            &[],
        )?
        .remove(0);
        let isolated = caller.home.is_some() || self.options.isolated;
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
                grant: Some(child_grant),
                depth: caller.info.depth + 1,
            },
        )
        .inspect_err(|_| store.release(&name))?;
        store.add_child(&self.branch, &name)?;
        let info = record.info.clone();
        self.start(
            record,
            profile,
            launch,
            SessionMode::Fresh,
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
        })
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
    fn start(
        &self,
        record: Record,
        profile: &'static Profile,
        command: Vec<String>,
        mode: SessionMode,
        prompt: String,
    ) -> Result<(), Error> {
        let name = record.info.name.clone();
        let yard = self.yard.clone();
        let options = self.child_options();
        let started = std::thread::Builder::new()
            .name(format!("by-{name}"))
            .spawn(move || {
                // The outcome is the branch's status; errors are recorded there.
                let _ = engine::execute(Turn {
                    yard: &yard,
                    record,
                    profile,
                    command,
                    mode,
                    prompt: &prompt,
                    options: &options,
                    fork_source: None,
                });
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
        })
    }

    fn events(
        &self,
        branch: &str,
        cursor: Option<usize>,
        limit: usize,
    ) -> Result<EventPage, Error> {
        self.require_descendant(branch, true)?;
        let events = record::read(&self.store(), branch)?;
        let total = events.len();
        let limit = limit.clamp(1, EVENTS_MAX);
        let start = cursor.unwrap_or(total.saturating_sub(limit)).min(total);
        let page: Vec<RecordedEvent> = events.into_iter().skip(start).take(limit).collect();
        Ok(EventPage {
            branch: branch.to_owned(),
            next_cursor: start + page.len(),
            total,
            events: page,
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
        self.start(
            prepared.record,
            prepared.profile,
            prepared.command,
            prepared.mode,
            prompt.to_owned(),
        )?;
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
}

/// The text of the last turn, at most [`LAST_MESSAGE_MAX`] characters.
fn last_message(events: &[RecordedEvent]) -> String {
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
struct NoArgs {}

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
        "cancel" => {
            let args: TargetArgs = parse(tool, arguments)?;
            to_json(&local.cancel(&required(tool, args.branch)?)?)
        }
        "children" => {
            let _: NoArgs = parse(tool, arguments)?;
            to_json(&local.children()?)
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
        let store = Store::new(&dir);
        store.create_dirs().unwrap();
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
            },
            created_ms: 0,
            check: None,
            command: None,
            home: None,
            cost_baseline: None,
            grant: Some(Grant {
                envelope: Envelope::default(),
                deny: Vec::new(),
                limits: limit.map(|max_usd| Limits {
                    max_usd: Some(max_usd),
                    ..Limits::default()
                }),
            }),
        }
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
