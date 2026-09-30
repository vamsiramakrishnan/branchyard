//! Task graphs: dependencies between a branch's children, atomic graph
//! proposals, and typed scratch-area bindings.
//!
//! A branch's children may depend on one another. A dependent child is
//! created at once but starts its first turn only when every prerequisite
//! has settled successfully ([`After::Settled`]: `ready`, `no_changes` or
//! `merged`), or, with [`After::Integrated`], once the prerequisite was
//! integrated into the parent. A prerequisite that fails, is interrupted,
//! stops at a limit, is blocked or is removed leaves its dependent
//! [`BranchStatus::Blocked`] with the reason.
//!
//! Scheduling is durable: dependencies are rows in the store, and a
//! dependent is started by a compare-and-swap ([`GraphBackend::claim`])
//! that moves it from `waiting` to `running` and takes its lease in one
//! transaction. Whichever engine settles a prerequisite, in any process,
//! looks at its dependents when the turn ends ([`settled`]); a wait for a
//! subtree, and [`crate::Yard::resume_graph`], start any whose
//! prerequisites settled while no engine was there to start them.
//!
//! A proposal ([`GraphEdit`]s with the parent's expected graph revision) is
//! validated whole and committed in one store transaction
//! ([`GraphBackend::commit_graph`]): every spawn's record, the parent's
//! children, the dependency rows and the new revision, or none of them.
//!
//! Not guaranteed: a dependent started by an engine other than the one
//! that applied the proposal runs under that engine's permission policy
//! (the one its sibling's turn ran under), narrowed by its own recorded
//! denials; one started by [`crate::Yard::resume_graph`] runs under the
//! options passed there.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use branchyard_harness::profiles;
use branchyard_harness::SessionMode;
use serde::{Deserialize, Serialize};

use crate::delegation::{self, ChildBudget, Spawn, Spawned};
use crate::projection::lock;
use crate::record::Recorder;
use crate::run::{self, Prepared};
use crate::state::{now_ms, Fence, Lease, Owner, Record, Store, LEASE_TTL};
use crate::storage::Lineage;
use crate::{git, harness, Activity, BranchStatus, Error, RecordedEvent, TaskOptions, Yard};

/// Most edits one proposal may carry.
pub const MAX_EDITS: usize = 64;

/// When a prerequisite counts as done for its dependent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum After {
    /// Its last turn ended `ready` or `no_changes`, or it was merged.
    #[default]
    Settled,
    /// It was integrated into the parent (`merged` into the parent's git
    /// branch), or ended `no_changes`, leaving nothing to integrate.
    Integrated,
}

impl After {
    fn is_settled(&self) -> bool {
        *self == After::Settled
    }
}

impl fmt::Display for After {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            After::Settled => "settled",
            After::Integrated => "integrated",
        })
    }
}

/// `dependent` starts only after `prerequisite`; both are children of the
/// same parent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub dependent: String,
    pub prerequisite: String,
    #[serde(default, skip_serializing_if = "After::is_settled")]
    pub after: After,
}

/// A dependency to remove.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DependencyRef {
    pub dependent: String,
    pub prerequisite: String,
}

/// How a branch uses a scratch area it is bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Checked readable when the child is planned and when each turn
    /// starts.
    ReadOnly,
    /// The area's writer lock is taken for each of the branch's turns and
    /// released when the turn ends; a turn that cannot take it fails.
    ExclusiveWrite,
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Access::ReadOnly => "read_only",
            Access::ExclusiveWrite => "exclusive_write",
        })
    }
}

/// A scratch area a branch is bound to; see `docs/graph.md`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub scratch: String,
    pub access: Access,
}

impl Binding {
    /// Parse `NAME:read_only` or `NAME:exclusive_write` (also `ro`, `rw`).
    pub fn parse(text: &str) -> Result<Binding, String> {
        let (scratch, access) = text
            .rsplit_once(':')
            .ok_or_else(|| format!("{text:?}: expected NAME:read_only or NAME:exclusive_write"))?;
        let access = match access {
            "read_only" | "ro" => Access::ReadOnly,
            "exclusive_write" | "rw" => Access::ExclusiveWrite,
            other => {
                return Err(format!(
                    "{text:?}: access must be read_only or exclusive_write, not {other}"
                ))
            }
        };
        Ok(Binding {
            scratch: scratch.to_owned(),
            access,
        })
    }
}

/// A child to create, as the `spawn` tool and a graph proposal's `spawn`
/// edit take it. [`Spawn`] is the same in Rust.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SpawnSpec {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<ChildBudget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_children: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harnesses: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat: Option<String>,
    /// Siblings this child waits for: other children of the same parent,
    /// or other spawns in the same proposal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// When each of `depends_on` counts as done.
    #[serde(default, skip_serializing_if = "After::is_settled")]
    pub after: After,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Binding>,
    /// The child's connector grant, narrowed to its parent's; unset
    /// inherits the parent's (or its seat's). See `docs/connectors.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connectors: Option<Vec<crate::connectors::GrantEntry>>,
}

impl SpawnSpec {
    pub(crate) fn to_spawn(&self) -> Result<Spawn, Error> {
        Ok(Spawn {
            prompt: self.prompt.clone(),
            harness: self.harness.clone(),
            name: self.name.clone(),
            base: self.base.clone(),
            budget: self.budget.clone().unwrap_or_default().to_budget()?,
            check: self.check.clone(),
            max_depth: self.max_depth,
            max_children: self.max_children,
            harnesses: self.harnesses.clone(),
            deny: self.deny.clone(),
            seat: self.seat.clone(),
            depends_on: self.depends_on.clone(),
            after: self.after,
            bindings: self.bindings.clone(),
            connectors: self.connectors.clone(),
        })
    }

    pub(crate) fn from_spawn(spawn: &Spawn) -> SpawnSpec {
        let budget = ChildBudget::from(&spawn.budget);
        SpawnSpec {
            prompt: spawn.prompt.clone(),
            harness: spawn.harness.clone(),
            name: spawn.name.clone(),
            base: spawn.base.clone(),
            budget: (budget != ChildBudget::default()).then_some(budget),
            check: spawn.check.clone(),
            max_depth: spawn.max_depth,
            max_children: spawn.max_children,
            harnesses: spawn.harnesses.clone(),
            deny: spawn.deny.clone(),
            seat: spawn.seat.clone(),
            depends_on: spawn.depends_on.clone(),
            after: spawn.after,
            bindings: spawn.bindings.clone(),
            connectors: spawn.connectors.clone(),
        }
    }
}

/// One edit of a graph proposal, tagged by `kind`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
// A proposal holds at most `MAX_EDITS`, built by hand in SDK code: boxing
// the spawn would only make every caller write `Box::new`.
#[allow(clippy::large_enum_variant)]
pub enum GraphEdit {
    /// Create a child of the branch whose graph this is.
    Spawn(SpawnSpec),
    /// Make one child wait for another. The dependent must not have
    /// started yet.
    AddDependency(Dependency),
    /// Remove a dependency of a child that has not started yet.
    RemoveDependency(DependencyRef),
}

/// A proposal: edits and the graph revision they were made against, as
/// `by graph apply` reads it and `POST …/graph` takes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GraphProposal {
    pub expected_revision: u64,
    pub edits: Vec<GraphEdit>,
}

/// What a committed proposal did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GraphApplied {
    /// The branch whose graph changed.
    pub branch: String,
    /// Its graph revision after the proposal.
    pub revision: u64,
    /// The children the proposal created, in proposal order, as they were
    /// when the call returned: `running` if started, `waiting` for their
    /// prerequisites, or `blocked`.
    pub spawned: Vec<Spawned>,
    /// Every dependency among the branch's children after the proposal.
    pub dependencies: Vec<Dependency>,
}

/// A child in [`Graph`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GraphNode {
    pub name: String,
    pub status: BranchStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Binding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat: Option<String>,
}

/// A branch's graph: its children and the dependencies among them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Graph {
    pub branch: String,
    /// Bumped by every committed proposal and every spawn.
    pub revision: u64,
    /// Direct children, oldest first.
    pub children: Vec<GraphNode>,
    pub dependencies: Vec<Dependency>,
}

/// One proposal, validated, for [`GraphBackend::commit_graph`].
#[derive(Clone, Debug)]
pub(crate) struct GraphCommit {
    pub parent: String,
    /// The revision the proposal was made against; `None` for a plain
    /// spawn, which is checked under the process's spawn lock instead.
    pub expected: Option<u64>,
    /// New children's records, `waiting`; each name must be free.
    pub create: Vec<Record>,
    pub add: Vec<Dependency>,
    pub remove: Vec<DependencyRef>,
}

/// Durable graph state: dependency rows and each parent's revision, and
/// the compare-and-swap moves of a `waiting` branch. Implemented for
/// [`crate::sqlite::Sqlite`] and [`crate::pg::Postgres`] beside their
/// [`crate::state::Backend`], sharing its connection; kept separate so
/// this feature does not enlarge that trait.
pub(crate) trait GraphBackend: Send + Sync + fmt::Debug {
    /// `parent`'s graph revision; 0 before its first proposal or spawn.
    fn graph_revision(&self, parent: &str) -> Result<u64, Error>;
    /// Every dependency among `parent`'s children, ordered by dependent
    /// then prerequisite.
    fn dependencies(&self, parent: &str) -> Result<Vec<Dependency>, Error>;
    /// What `dependent` waits for.
    fn prerequisites(&self, dependent: &str) -> Result<Vec<Dependency>, Error>;
    /// What waits for `prerequisite`.
    fn dependents(&self, prerequisite: &str) -> Result<Vec<Dependency>, Error>;
    /// In one transaction: check the revision ([`Error::StaleRevision`]),
    /// create each record as a child of the parent (a taken name is
    /// [`Error::BranchExists`]), check that every dependent an edge edit
    /// touches is still `waiting` or `blocked` (reopening a `blocked` one
    /// to `waiting`), add and remove the edges (a duplicate or missing one
    /// is [`Error::Denied`]), and bump the revision. Returns the new
    /// revision. Any failure changes nothing.
    fn commit_graph(&self, commit: &GraphCommit) -> Result<u64, Error>;
    /// Write `record` and take its lease for its first turn, only if the
    /// stored record is still `waiting` and no lease is held: the one
    /// start of a dependent, whichever engine gets there first. `None`
    /// when it is not.
    fn claim(&self, record: &Record, owner: &Owner, ttl: Duration) -> Result<Option<Fence>, Error>;
    /// Write `record` and append `event`, only if the stored record is
    /// still `waiting` and no lease is held. False when it is not.
    fn settle_waiting(&self, record: &Record, event: &RecordedEvent) -> Result<bool, Error>;
}

/// Whether `status` is waiting for prerequisites or blocked by one:
/// dependency edits may still change it.
pub(crate) fn unstarted(status: &BranchStatus) -> bool {
    matches!(status, BranchStatus::Waiting | BranchStatus::Blocked { .. })
}

/// Every dependency cycle among `edges`, as one named cycle, if any.
pub(crate) fn find_cycle(edges: &BTreeSet<(String, String)>) -> Option<Vec<String>> {
    let mut next: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (dependent, prerequisite) in edges {
        next.entry(dependent).or_default().push(prerequisite);
    }
    // Depth-first search with colors: 1 on the stack, 2 done.
    let mut color: BTreeMap<&str, u8> = BTreeMap::new();
    fn visit<'a>(
        node: &'a str,
        next: &BTreeMap<&'a str, Vec<&'a str>>,
        color: &mut BTreeMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        color.insert(node, 1);
        stack.push(node);
        for &to in next.get(node).map(Vec::as_slice).unwrap_or_default() {
            match color.get(to).copied().unwrap_or(0) {
                1 => {
                    let start = stack.iter().position(|n| *n == to).unwrap_or(0);
                    let mut cycle: Vec<String> =
                        stack[start..].iter().map(|n| (*n).to_owned()).collect();
                    cycle.push(to.to_owned());
                    return Some(cycle);
                }
                0 => {
                    if let Some(cycle) = visit(to, next, color, stack) {
                        return Some(cycle);
                    }
                }
                _ => {}
            }
        }
        stack.pop();
        color.insert(node, 2);
        None
    }
    let nodes: Vec<&str> = next.keys().copied().collect();
    for node in nodes {
        if color.get(node).copied().unwrap_or(0) == 0 {
            if let Some(cycle) = visit(node, &next, &mut color, &mut Vec::new()) {
                return Some(cycle);
            }
        }
    }
    None
}

/// What a `waiting` branch's prerequisites say.
#[derive(Debug, PartialEq)]
enum Verdict {
    Start,
    Wait,
    Blocked(String),
}

fn verdict(store: &Store, record: &Record) -> Result<Verdict, Error> {
    let target = match &record.info.parent {
        Some(parent) => store
            .backend()
            .read(parent)?
            .map(|p| p.info.git_branch)
            .unwrap_or_default(),
        None => String::new(),
    };
    let mut wait = false;
    for dependency in store.graph().prerequisites(&record.info.name)? {
        let name = &dependency.prerequisite;
        let Some(prerequisite) = store.backend().read(name)? else {
            return Ok(Verdict::Blocked(format!(
                "its prerequisite {name} was removed"
            )));
        };
        let blocked = |why: String| Ok(Verdict::Blocked(format!("its prerequisite {name} {why}")));
        match (&prerequisite.info.status, dependency.after) {
            (BranchStatus::Ready | BranchStatus::NoChanges, After::Settled)
            | (BranchStatus::Merged { .. }, After::Settled)
            | (BranchStatus::NoChanges, After::Integrated) => {}
            (BranchStatus::Merged { target: into, .. }, After::Integrated) if *into == target => {}
            (BranchStatus::Ready | BranchStatus::Merged { .. }, After::Integrated) => wait = true,
            (BranchStatus::Running | BranchStatus::Waiting, _) => wait = true,
            (BranchStatus::Failed { reason }, _) => return blocked(format!("failed: {reason}")),
            (BranchStatus::Interrupted, _) => return blocked("was interrupted".into()),
            (BranchStatus::BudgetExceeded { limit }, _) => {
                return blocked(format!("stopped at its {limit} limit"))
            }
            (BranchStatus::Blocked { .. }, _) => return blocked("is blocked".into()),
        }
    }
    Ok(match wait {
        true => Verdict::Wait,
        false => Verdict::Start,
    })
}

/// The options a dependent's turn runs with: those the proposal was
/// applied with in this process, else `given`'s policy, observer and tools.
/// Its limits and denials come from its own record.
fn options_for(yard: &Yard, record: &Record, given: Option<&TaskOptions>) -> Option<TaskOptions> {
    let parent = record.info.parent.as_deref()?;
    if let Some(found) = lock(&yard.hub.graph_options).get(parent) {
        return Some(found.clone());
    }
    given.map(child_options)
}

/// A child's turn options from its parent's or a sibling's.
pub(crate) fn child_options(options: &TaskOptions) -> TaskOptions {
    TaskOptions {
        policy: options.policy.clone(),
        observer: options.observer.clone(),
        delegation_cli: options.delegation_cli.clone(),
        delegation_server: options.delegation_server.clone(),
        ..TaskOptions::default()
    }
}

/// Settle a `waiting` branch without running it: `blocked` with `reason`,
/// or `interrupted` when `cancelled`. False when it had left `waiting`.
pub(crate) fn settle_unstarted(
    store: &Store,
    record: &Record,
    status: BranchStatus,
) -> Result<bool, Error> {
    let mut record = record.clone();
    record.info.status = status;
    let event = RecordedEvent {
        at_ms: now_ms(),
        activity: Activity::Status(record.info.status.clone()),
    };
    let done = store.graph().settle_waiting(&record, &event)?;
    if done {
        store.notify();
    }
    Ok(done)
}

/// Look at each of `names` that is `waiting`: block it when a prerequisite
/// failed, start it when all have settled and options to run it are known,
/// else leave it. Returns the records of those started, as started. A
/// dependent that is blocked blocks its own dependents in turn.
pub(crate) fn advance(
    yard: &Yard,
    names: &[String],
    options: Option<&TaskOptions>,
) -> Result<Vec<Record>, Error> {
    let store = yard.store();
    let mut started = Vec::new();
    let mut queue: Vec<String> = names.to_vec();
    let mut seen = BTreeSet::new();
    let mut failed = None;
    while let Some(name) = queue.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(record) = store.backend().read(&name)? else {
            continue;
        };
        if record.info.status != BranchStatus::Waiting {
            continue;
        }
        match verdict(&store, &record)? {
            Verdict::Wait => {}
            Verdict::Blocked(reason) => {
                if settle_unstarted(&store, &record, BranchStatus::Blocked { reason })? {
                    queue.extend(dependents_of(&store, &name));
                }
            }
            Verdict::Start => {
                let Some(options) = options_for(yard, &record, options) else {
                    continue;
                };
                match start(yard, record, &options) {
                    Ok(Some(record)) => started.push(record),
                    Ok(None) => {}
                    Err(error) => failed = failed.or(Some(error)),
                }
            }
        }
    }
    match failed {
        Some(error) => Err(error),
        None => Ok(started),
    }
}

fn dependents_of(store: &Store, name: &str) -> Vec<String> {
    store
        .graph()
        .dependents(name)
        .map(|found| found.into_iter().map(|d| d.dependent).collect())
        .unwrap_or_default()
}

/// `name`'s turn or state changed: look at what waits for it. Errors are
/// not the finished turn's; they leave the dependents to the next look.
pub(crate) fn settled(yard: &Yard, name: &str, options: Option<&TaskOptions>) {
    let store = yard.store();
    let dependents = dependents_of(&store, name);
    if !dependents.is_empty() {
        let _ = advance(yard, &dependents, options);
    }
}

/// Start a `waiting` branch's first turn on a thread of this process, from
/// its explicit base or its parent's git branch as it is now. `None` if
/// another engine claimed it first.
fn start(yard: &Yard, mut record: Record, options: &TaskOptions) -> Result<Option<Record>, Error> {
    let store = yard.store();
    let profile = profiles::by_id(&record.info.profile)
        .ok_or_else(|| Error::UnknownHarness(record.info.profile.clone()))?;
    let command = harness::command(profile, record.command.as_deref());
    let parent = record.info.parent.clone().unwrap_or_default();
    let parent_record = store.backend().read(&parent)?;
    let base = match record.start_base.take() {
        Some(base) => base,
        None => {
            let head = parent_record
                .as_ref()
                .map(|p| git::local_branch(&yard.root, &p.info.git_branch))
                .transpose()?
                .flatten();
            match head {
                Some(head) => head,
                None => {
                    let reason = format!("its parent {parent}'s branch is missing");
                    settle_unstarted(&store, &record, BranchStatus::Blocked { reason })?;
                    return Ok(None);
                }
            }
        }
    };
    // A delegated child, rig seat or dependent on its parent's provider
    // starts from the parent's provider snapshot at its base, when there
    // is one; see `crate::snapshots`.
    record.sandbox_seed = parent_record
        .as_ref()
        .filter(|p| p.provider == record.provider)
        .and_then(|p| crate::snapshots::seed(p, None, &base));
    record.info.base = base;
    record.info.status = BranchStatus::Running;
    let Some(fence) = store.graph().claim(&record, store.owner(), LEASE_TTL)? else {
        return Ok(None);
    };
    let lease = Lease::new(store.clone(), fence);
    let (record, lease) = run::materialize(yard, record, lease)?;
    let started = record.clone();
    let prompt = record.info.prompt.clone();
    delegation::start_turn(
        yard,
        Prepared {
            record,
            lease,
            profile,
            command,
            mode: SessionMode::Fresh,
            note: None,
        },
        prompt,
        options.clone(),
    )?;
    Ok(Some(started))
}

/// `name`'s graph: its children and the dependencies among them.
pub(crate) fn show(store: &Store, name: &str) -> Result<Graph, Error> {
    let record = store.read(name)?;
    let dependencies = store.graph().dependencies(name)?;
    let mut children: Vec<Record> = record
        .info
        .children
        .iter()
        .filter_map(|child| store.read(child).ok())
        .collect();
    children.sort_by(|a, b| (a.created_ms, &a.info.name).cmp(&(b.created_ms, &b.info.name)));
    let children = children
        .into_iter()
        .map(|child| GraphNode {
            depends_on: dependencies
                .iter()
                .filter(|d| d.dependent == child.info.name)
                .map(|d| d.prerequisite.clone())
                .collect(),
            seat: child
                .grant
                .as_ref()
                .and_then(|g| g.seats.as_ref())
                .map(|s| s.seat.clone()),
            bindings: child.bindings,
            name: child.info.name,
            status: child.info.status,
        })
        .collect();
    Ok(Graph {
        branch: name.to_owned(),
        revision: store.graph().graph_revision(name)?,
        children,
        dependencies,
    })
}

/// Start every `waiting` branch in the repository whose prerequisites have
/// all settled, under `options`, and block those one of whose
/// prerequisites failed. Returns the branches started.
pub(crate) fn resume(yard: &Yard, options: &TaskOptions) -> Result<Vec<String>, Error> {
    let store = yard.store();
    let waiting: Vec<String> = store
        .list()?
        .into_iter()
        .filter(|r| r.info.status == BranchStatus::Waiting)
        .map(|r| r.info.name)
        .collect();
    Ok(advance(yard, &waiting, Some(options))?
        .into_iter()
        .map(|r| r.info.name)
        .collect())
}

/// Check `bindings` for a new child of `parent`: each names an existing
/// scratch area, once, that the child will be able to read, which without
/// a share of its own means the area belongs to `parent` or one of its
/// ancestors (see `docs/storage.md`).
pub(crate) fn check_bindings(
    store: &Store,
    parent: &str,
    bindings: &[Binding],
) -> Result<(), Error> {
    if bindings.is_empty() {
        return Ok(());
    }
    let lineage = Lineage::load(store)?;
    let me = lineage.id(parent)?;
    let mut seen = BTreeSet::new();
    for binding in bindings {
        let name = &binding.scratch;
        crate::storage::validate_scratch_name(name)?;
        if !seen.insert(name) {
            return Err(Error::Denied(format!(
                "scratch area {name} is bound twice; bind it once"
            )));
        }
        let row = store.storage().scratch(name)?.ok_or_else(|| {
            Error::Denied(format!(
                "cannot bind scratch area {name}: it does not exist"
            ))
        })?;
        let owned_above = row
            .owner_incarnation
            .is_some_and(|owner| lineage.descends(owner, me));
        if !owned_above {
            return Err(Error::Denied(format!(
                "cannot bind scratch area {name}: it belongs to {}, so a new child of {parent} \
                 could not read it; bind an area {parent} or one of its ancestors owns",
                row.area.owner_branch
            )));
        }
    }
    Ok(())
}

/// Take what `record`'s bindings need for a turn: check each read-only
/// area is still readable and take each exclusive area's writer lock. On
/// failure, the locks taken are released and the reason returned.
pub(crate) fn bind(yard: &Yard, record: &Record) -> Result<(), String> {
    let name = &record.info.name;
    let mut taken = Vec::new();
    for binding in &record.bindings {
        let result = match binding.access {
            Access::ReadOnly => crate::storage::check_readable(yard, name, &binding.scratch),
            Access::ExclusiveWrite => {
                crate::storage::lock_scratch(yard, name, &binding.scratch).map(|_| ())
            }
        };
        match result {
            Ok(()) if binding.access == Access::ExclusiveWrite => taken.push(&binding.scratch),
            Ok(()) => {}
            Err(error) => {
                for scratch in taken {
                    let _ = crate::storage::unlock_scratch(yard, name, scratch);
                }
                return Err(format!(
                    "its {} binding to scratch area {} could not be honored: {error}",
                    binding.access, binding.scratch
                ));
            }
        }
    }
    Ok(())
}

/// Release the writer locks `record`'s exclusive bindings took.
pub(crate) fn unbind(yard: &Yard, record: &Record) {
    for binding in &record.bindings {
        if binding.access == Access::ExclusiveWrite {
            let _ = crate::storage::unlock_scratch(yard, &record.info.name, &binding.scratch);
        }
    }
}

/// Record on `name`'s log that it was cancelled before it started.
pub(crate) fn cancel_unstarted(store: &Store, record: &Record, by: &str) -> Result<bool, Error> {
    let done = settle_unstarted(store, record, BranchStatus::Interrupted)?;
    if done {
        if let Ok(mut recorder) = Recorder::open(store, &record.info.name, None) {
            let _ = recorder.record(Activity::Warning(format!(
                "cancelled by {by} before its prerequisites settled; it never ran"
            )));
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(list: &[(&str, &str)]) -> BTreeSet<(String, String)> {
        list.iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    #[test]
    fn cycles_are_found_and_named() {
        assert_eq!(find_cycle(&edges(&[("b", "a"), ("c", "b")])), None);
        let cycle = find_cycle(&edges(&[("a", "b"), ("b", "c"), ("c", "a")])).unwrap();
        assert_eq!(cycle.first(), cycle.last());
        assert_eq!(cycle.len(), 4);
        assert!(find_cycle(&edges(&[("a", "a")])).is_some());
    }

    #[test]
    fn edits_read_as_tagged_objects_and_refuse_unknown_fields() {
        let edits: Vec<GraphEdit> = serde_json::from_value(serde_json::json!([
            {"kind": "spawn", "prompt": "p", "name": "a", "depends_on": ["b"], "after": "integrated",
             "bindings": [{"scratch": "notes", "access": "exclusive_write"}]},
            {"kind": "add_dependency", "dependent": "a", "prerequisite": "c"},
            {"kind": "remove_dependency", "dependent": "a", "prerequisite": "d"},
        ]))
        .unwrap();
        match &edits[0] {
            GraphEdit::Spawn(spec) => {
                assert_eq!(spec.depends_on, ["b"]);
                assert_eq!(spec.after, After::Integrated);
                assert_eq!(spec.bindings[0].access, Access::ExclusiveWrite);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            serde_json::to_value(&edits[1]).unwrap(),
            serde_json::json!({"kind": "add_dependency", "dependent": "a", "prerequisite": "c"})
        );
        for bad in [
            serde_json::json!({"kind": "spawn", "prompt": "p", "colour": 1}),
            serde_json::json!({"kind": "add_dependency", "dependent": "a"}),
            serde_json::json!({"kind": "rename", "name": "a"}),
        ] {
            assert!(
                serde_json::from_value::<GraphEdit>(bad.clone()).is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            Binding::parse("notes:rw").unwrap(),
            Binding {
                scratch: "notes".into(),
                access: Access::ExclusiveWrite
            }
        );
        assert!(Binding::parse("notes").is_err());
    }
}
