//! Delegate coding work to agent harnesses on git branches, and merge only
//! validated results.
//!
//! This is the public Branchyard SDK in local mode: the engine runs
//! in-process, harnesses run as local processes, and every branch is a git
//! worktree of your repository. State lives in `.branchyard/` at the
//! repository root. Local mode provides no isolation beyond your operating
//! system user; see `docs/design.md` §4. [`TaskOptions::provider`] runs a
//! branch's harness in a Microsandbox microVM instead, with the worktree
//! mounted into it, or in an Agent Substrate actor, with the worktree
//! carried in and out as git bundles; see `docs/providers.md`.
//!
//! ```no_run
//! use branchyard::{Budget, Policy, Yard};
//!
//! let yard = Yard::open(".")?;
//! let branches = yard
//!     .task("Make the flaky parser test deterministic")
//!     .budget(Budget::usd(2.0))
//!     .policy(Policy::allow_all())
//!     .check(["cargo", "test"])
//!     .run_on(&["claude-code", "codex"])?;
//! for branch in &branches {
//!     println!("{}: {:?}", branch.info().name, branch.info().status);
//! }
//! yard.merge(&branches[0].info().name, "main")?;
//! # Ok::<(), branchyard::Error>(())
//! ```
//!
//! Vocabulary: a **task** is what you asked; a **branch** is one harness
//! working in its own worktree and session; a **fork** starts a new branch
//! from another branch's candidate and forks its conversation where the
//! harness supports it; a **candidate** is the exact commit a branch
//! proposes; a **merge** promotes a candidate only after checks pass
//! against the exact target revision.
//!
//! What the engine guarantees:
//!
//! - State is durable in `.branchyard/state.db` (SQLite, write-ahead log),
//!   written in transactions. Every harness event, permission decision,
//!   candidate snapshot, status change and warning is appended to the
//!   branch's event log before the observer sees it, and can be read back
//!   from a cursor ([`Branch::events_since`], [`Yard::events_since`]).
//!   Every permission request reaches the [`Policy`]; nothing runs with a
//!   permission bypass.
//! - Branch names are reserved in a transaction, so parallel branches never
//!   share a name.
//! - A turn runs under its branch's lease, with a fencing generation that
//!   every write of the turn checks: two engines, in one process or two,
//!   never drive one branch at once. Its steps are journaled. When an
//!   engine stops mid-turn, [`Yard::open`] recovers the branch: it kills the
//!   harness's process group if pid and start time still match, and sets a
//!   truthful status, [`BranchStatus::Interrupted`] when the turn's outcome
//!   is unknown. A submitted prompt is never submitted again. See
//!   `docs/durability.md`.
//! - Cancellation is durable: [`Yard::cancel`] records a request that the
//!   engine running the turn, in any process, observes. So is steering:
//!   [`Branch::steer`] queues input that engine delivers into the running
//!   turn, where the harness supports it.
//! - A turn over budget is interrupted and waited for, never abandoned; the
//!   harness's process group is torn down when each call returns, and
//!   descendants that outlived it are named in the event log.
//! - A merge moves the target only by compare-and-swap from the revision
//!   read when the merge started, after the branch's check passed on the
//!   exact merge commit (`branchyard_workspace`).
//!
//! What it does not guarantee:
//!
//! - Isolation. By default a harness runs with your environment and your
//!   `HOME`, so it uses your own harness login and can read what you can;
//!   every `CLAUDE*` variable except Claude Code configuration (provider,
//!   credentials, TLS identity, limits) is removed, so a child never runs
//!   under its parent session's identity. [`TaskOptions::isolated`]
//!   gives it a scrubbed environment and a private home instead, which
//!   usually means it is not logged in.
//! - Recovery of a turn whose engine runs on another host: its lease has to
//!   expire first, and its processes there are not killed.
//! - Resume and fork across working directories. Some harnesses keep
//!   sessions per directory (Claude Code keys them by project path), so a
//!   fork, which runs in a new worktree, may not find its parent's session.
//!   The engine reports that failure as the branch's status; it never
//!   substitutes a fresh session.
//! - Cost limits for harnesses that report no cumulative cost estimate.
//!
//! # Delegation
//!
//! With [`TaskOptions::delegation`], a harness can spawn, inspect, message,
//! integrate and cancel child branches within an [`Envelope`], through `by`
//! in its shell, the Python module, or Branchyard's MCP tools; SDK code does
//! the same through a [`Delegate`]. Children run on threads of the process
//! that runs their parent; a process must call [`Branch::wait_subtree`]
//! before it exits, or it abandons them to recovery, which ends them
//! `interrupted`. `docs/delegation.md` describes the
//! surfaces, the envelope and the authority model, which in local mode
//! stops honest mistakes, not a hostile harness.

mod adopt;
mod broker;
mod bundle;
mod checkpoint;
mod compare;
#[cfg(test)]
mod conformance;
pub mod connectors;
mod delegation;
mod egress;
mod engine;
mod environments;
mod fleet;
mod git;
mod goal;
mod graph;
mod harness;
mod inbox;
mod judge;
mod knowledge;
mod lock;
mod names;
mod ops;
#[cfg(feature = "postgres")]
mod pg;
mod placement;
mod plan;
mod policy;
mod proc;
mod projection;
mod provisioning;
mod pull_request;
mod record;
mod recover;
mod run;
mod seats;
mod snapshots;
mod spotlight;
mod sqlite;
mod state;
mod steer;
mod storage;
mod tarball;
mod workspace;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub use git::current_branch;
pub use lock::DirLock;
pub use placement::{HOME as SANDBOX_HOME, WORKSPACE as SANDBOX_WORKSPACE};

pub use adopt::{AdoptSpec, Adoption};
pub use branchyard_harness::{
    Event, NativeSession, PermissionDecision, PermissionKey, PermissionRequest, TurnOutcome, Usage,
};
pub use branchyard_provision::network::{
    narrow as network_narrow, Enforce as NetworkEnforce, HostRule as NetworkRule, Network,
};
pub use branchyard_provision::{
    Delivery, Effort, McpServerSpec, Provisioning, RemoteMcpSpec, RemoteMcpTransport, SecretFrom,
    SecretSource, Telemetry, Via,
};
use branchyard_workspace::Repository;
pub use bundle::BundleEntry;
pub use checkpoint::{
    entries as checkpoint_entries, recorded as recorded_checkpoints, CheckpointEntry, Checkpoints,
    Rewound,
};
pub use compare::{attempt as compare_attempt, diff_files, mark_unique, Attempt, AttemptCheck};
pub use delegation::{
    Asked, Cancelled, ChildBudget, Children, Delegate, Envelope, EventPage, Inbox, Inspection,
    Sent, Spawn, Spawned,
};
pub use egress::{EgressActivity, Enforcement as EgressEnforcement};
pub use environments::{
    EnvironmentBuild, EnvironmentInfo, EnvironmentInput, EnvironmentOrigin, EnvironmentSnapshot,
    EnvironmentState, EnvironmentUse, Pruned as EnvironmentsPruned,
    DEFAULT_INPUTS as ENVIRONMENT_DEFAULT_INPUTS, DEFAULT_KEEP as ENVIRONMENT_DEFAULT_KEEP,
    DEFAULT_MAX_AGE as ENVIRONMENT_DEFAULT_MAX_AGE,
};
pub use fleet::{
    classify, credit, fresh_seed as fleet_seed, harness_fault, recorded_route,
    stats as fleet_stats, BranchOutcome, CandidateStats, Classification, Excluded, Fleet,
    FleetActivity, FleetCandidate, FleetEntry, JudgeMark, JudgeSpec, OutcomeRecord, Route,
    RouteDecision, RouteOptions, RoutePick, Routed, TaskKind, DEFAULT_EXPLORATION,
};
pub use goal::{
    follow_up_prompt as goal_follow_up_prompt, from_events as goal_from_events,
    judge_prompt as goal_judge_prompt, parse_goal_verdict, Goal, GoalActivity, GoalCheck, GoalInfo,
    GoalVerdict, DEFAULT_ROUNDS as GOAL_DEFAULT_ROUNDS,
};
pub use graph::{
    Access, After, Binding, Dependency, DependencyRef, Graph, GraphApplied, GraphEdit, GraphNode,
    GraphProposal, SpawnSpec, MAX_EDITS,
};
pub use inbox::{DeliveryHook, SteerDelivery};
pub use judge::{
    deterministic_scores, harness_judge, parse_verdict, prompt as judge_prompt, HarnessJudge,
    Judge, JudgeOptions, JudgedBy, Judgement, Scored, Verdict,
};
pub use knowledge::{
    export as export_knowledge, parse_distilled, DistillTrigger, Distilled, KnowledgeActivity,
    KnowledgeEdit, KnowledgeEntry, KnowledgeScope, KnowledgeSettings, KnowledgeSource,
    KnowledgeStatus, NewKnowledge, DEFAULT_BUDGET_TOKENS as KNOWLEDGE_DEFAULT_BUDGET_TOKENS,
    TEXT_MAX as KNOWLEDGE_TEXT_MAX,
};
pub use plan::{
    approved_prompt, from_events as plan_from_events, parse_tasks as parse_plan_tasks,
    planning_prompt, read_only as read_only_policy, Plan, PlanActivity, PlanInfo, PlanPhase,
    PlanTask, READ_ONLY_TOOLS,
};
pub use policy::{PolicyPreset, PresetRules, EDIT_TOOLS, SHELL_TOOLS, WEB_TOOLS};
pub use projection::{ENV_BRANCH, ENV_BY, ENV_ROOT, ENV_TOKEN};
pub use pull_request::{
    slug, CheckRun, CiSummary, IssueLink, PullRequestActivity, PullRequestObservation,
    PullRequestRef, Pushed, ResolvedThread,
};
pub use seats::{Seat, Seats};
use serde::{Deserialize, Serialize};
pub use snapshots::{
    SandboxConsistency, SandboxEvent, SandboxKeep, SandboxOrigin, SandboxScope, SandboxSnapshot,
    SnapshotMethod,
};
pub use spotlight::{TryEntry, TryFile, TryState};
use std::collections::BTreeMap;
pub use storage::{ArtifactRef, ScratchArea, ScratchLock, DEFAULT_ARTIFACT_LIMIT};
pub use workspace::{
    check_glob as check_workspace_glob, RanIn, WorkspaceInfo, WorkspacePhase, WorkspaceReport,
    WorkspaceSpec, ENV_PORT, ENV_WORKTREE, OUTPUT_TAIL as WORKSPACE_OUTPUT_TAIL,
    PORT_RANGE as WORKSPACE_PORT_RANGE,
};

/// This process as the engine names a lease's holder: its host and boot,
/// its pid, and its start time. For leases kept outside the engine, such as
/// the server's claims on queued operations.
pub fn process_identity() -> (String, u32, String) {
    (
        proc::host().to_owned(),
        std::process::id(),
        proc::own_start().to_owned(),
    )
}

/// Whether the process `pid` that started at `start` on `host`, as
/// [`process_identity`] named it, is known to be gone: it ran on this host
/// and boot and is not running now. Never true for another host, or when
/// the start time is unknown.
pub fn process_gone(host: &str, pid: u32, start: &str) -> bool {
    !start.is_empty() && state::gone(host, pid, start)
}

/// A repository with Branchyard state. Cheap to clone; clones share state.
#[derive(Clone, Debug)]
pub struct Yard {
    root: PathBuf,
    repo: Repository,
    store: state::Store,
    /// Delegation contexts, running children and the broker, shared by
    /// clones.
    hub: Arc<projection::Hub>,
}

impl Yard {
    /// Open the git repository containing `path`, creating `.branchyard/`
    /// and excluding it from git through the repository's `info/exclude`.
    /// Never touches `.gitignore`. Imports state left by earlier versions,
    /// then recovers every branch whose engine stopped mid-turn, as
    /// [`Yard::recover`] does.
    pub fn open(path: impl AsRef<Path>) -> Result<Yard, Error> {
        ops::open(path.as_ref())
    }

    /// [`Yard::open`], with the repository's state in the PostgreSQL
    /// database at `url` (`postgres://user@host/db`) instead of
    /// `.branchyard/state.db`, under `scope`, a name that separates
    /// repositories sharing a database. Worktrees, private homes and
    /// delegation tokens stay in `.branchyard/`. The tables are created in
    /// the connection's `search_path` schema when missing. Nothing is
    /// imported from `state.db`, and `by` on the same repository without
    /// the database sees none of this state. Needs the `postgres` feature;
    /// see `docs/durability.md`.
    #[cfg(feature = "postgres")]
    pub fn open_postgres(path: impl AsRef<Path>, url: &str, scope: &str) -> Result<Yard, Error> {
        ops::open_with(path.as_ref(), |root| {
            state::Store::open_postgres(root, url, scope)
        })
    }

    /// Recover every branch whose turn's engine stopped: on this host, a
    /// process that is gone; anywhere, a lease that expired. Kills the
    /// turn's recorded harness process group when its pid and start time
    /// still match, settles the branch's status from its journal, records
    /// [`Activity::Recovered`], and never submits a prompt again. Returns
    /// what was recovered. [`Yard::open`] already does this; call it again
    /// to reconcile a long-lived yard, as the server does.
    pub fn recover(&self) -> Result<Vec<Recovery>, Error> {
        recover::all(self)
    }

    /// Start every `waiting` branch whose prerequisites have all settled,
    /// its first turn on a thread of this process under `options`' policy,
    /// observer and tools (its limits and denials are its own), and mark
    /// `blocked` every one a prerequisite failed. The engine that settles a
    /// prerequisite does this for its dependents; this is for a
    /// prerequisite whose engine stopped before it could, and is what a
    /// server does on its recovery interval. Returns the branches started;
    /// wait for them before this process exits, as for any child
    /// ([`Branch::wait_subtree`] on their parent). See `docs/graph.md`.
    pub fn resume_graph(&self, options: &TaskOptions) -> Result<Vec<String>, Error> {
        graph::resume(self, options)
    }

    /// `branch`'s graph: its children, the dependencies among them, and its
    /// graph revision.
    pub fn graph(&self, branch: &str) -> Result<Graph, Error> {
        graph::show(&self.store(), branch)
    }

    /// Ask `branch`'s running turn, and every running turn delegated below
    /// it, to stop; each ends `interrupted`. The request is durable and is
    /// observed by the engine running the turn in any process using this
    /// repository. Returns the branches that were running.
    pub fn cancel(&self, branch: &str) -> Result<Vec<String>, Error> {
        self.cancel_as(branch, "the SDK caller")
    }

    /// [`Yard::cancel`] on behalf of `by`, whom each cancelled branch's
    /// event log names.
    pub fn cancel_as(&self, branch: &str, by: &str) -> Result<Vec<String>, Error> {
        delegation::cancel_tree(self, branch, by)
    }

    /// Deliver `text` into `branch`'s running turn as input from `by`,
    /// whom the branch's event log names ([`Activity::Steered`]). The turn
    /// may run in this process or another using the repository: the input
    /// is queued durably, bound to that turn like a cancel, and the engine
    /// running it writes it to the harness within about 100 ms; it is never
    /// delivered to a later turn. The harness takes it into the turn in
    /// flight without ending or interrupting it, at a point its protocol
    /// defines (`docs/harness-integration.md`), and the turn ends once, as
    /// usual, with its budget and policy unchanged.
    ///
    /// Returns the queued [`Steer`]; [`Yard::wait_steer`] follows it.
    /// Fails with [`Error::Unsupported`] and the reason when the branch's
    /// profile cannot take input mid-turn (no silent interrupt), and with
    /// [`Error::NotRunning`] when no turn is running.
    pub fn steer_as(&self, branch: &str, text: &str, by: &str) -> Result<Steer, Error> {
        steer::request(self, branch, text, by)
    }

    /// What became of steered input `id` of `branch`. Input still pending
    /// when its turn has ended was never delivered, and is reported
    /// [`SteerState::Refused`].
    pub fn steer_state(&self, branch: &str, id: u64) -> Result<Steer, Error> {
        steer::state(&self.store(), branch, id)
    }

    /// [`Yard::steer_state`], waiting up to `timeout` for the input to
    /// leave [`SteerState::Pending`].
    pub fn wait_steer(&self, branch: &str, id: u64, timeout: Duration) -> Result<Steer, Error> {
        steer::wait(&self.store(), branch, id, timeout)
    }

    /// Give this yard's branches the connector gateway `gateway`: a
    /// branch with a connector grant gets a signed token for each turn and
    /// its granted packages; see `docs/connectors.md`. Replaces any gateway
    /// set before. Shared by every clone of this `Yard`.
    pub fn use_connectors(&self, gateway: connectors::Gateway) {
        *self
            .hub
            .connectors
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(gateway));
    }

    /// The connector gateway set with [`Yard::use_connectors`], if any.
    pub fn connectors(&self) -> Option<Arc<connectors::Gateway>> {
        self.hub
            .connectors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Record the gateway's new audit lines on the branches they name, as
    /// [`Activity::ConnectorCall`] events; see [`connectors::ingest`].
    /// Returns how many were recorded; none without a gateway.
    pub fn ingest_connector_audit(&self) -> Result<usize, Error> {
        connectors::ingest(self)
    }

    /// Try `hook` before a message waits for its recipient's next turn to
    /// start; see [`DeliveryHook`]. Replaces any hook set before. Shared by
    /// every clone of this `Yard`.
    pub fn set_delivery_hook(&self, hook: Arc<dyn DeliveryHook>) {
        *self
            .hub
            .delivery_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// Stop trying a hook set with [`Yard::set_delivery_hook`].
    pub fn clear_delivery_hook(&self) {
        *self
            .hub
            .delivery_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub(crate) fn delivery_hook(&self) -> Option<Arc<dyn DeliveryHook>> {
        self.hub
            .delivery_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Up to `limit` recorded events of every branch after feed position
    /// `cursor`, in the order they were recorded. Positions start at 1 and
    /// only grow; pass the page's `next_cursor` back to continue. Events
    /// of removed branches stay in the feed.
    pub fn events_since(&self, cursor: u64, limit: usize) -> Result<FeedPage, Error> {
        record::feed(&self.store(), cursor, limit)
    }

    /// [`Yard::events_since`], waiting up to `timeout` for an event when
    /// there is none yet. Wakes at once for events recorded in this
    /// process, and within 100 ms for another process's.
    pub fn wait_for_events(
        &self,
        cursor: u64,
        limit: usize,
        timeout: Duration,
    ) -> Result<FeedPage, Error> {
        record::wait_feed(&self.store(), cursor, limit, timeout)
    }

    /// The feed position of the last recorded event; 0 when there is none.
    pub fn events_head(&self) -> Result<u64, Error> {
        self.store().backend().head()
    }

    /// Repository root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The branch checked out at the repository root (the default merge
    /// target), or `None` when HEAD is detached.
    pub fn current_branch(&self) -> Result<Option<String>, Error> {
        git::current_branch(&self.root)
    }

    /// Start describing a task.
    pub fn task(&self, prompt: impl Into<String>) -> TaskBuilder {
        TaskBuilder {
            yard: self.clone(),
            prompt: prompt.into(),
            options: TaskOptions::default(),
        }
    }

    /// Every branch Branchyard knows, oldest first.
    pub fn branches(&self) -> Result<Vec<BranchInfo>, Error> {
        Ok(self
            .store()
            .list()?
            .into_iter()
            .map(|record| record.info)
            .collect())
    }

    /// Look up one branch by name.
    pub fn branch(&self, name: &str) -> Result<Branch, Error> {
        let record = self.store().read(name)?;
        Ok(Branch {
            yard: self.clone(),
            info: record.info,
        })
    }

    /// Merge `branch`'s candidate into the local branch `target`, running the
    /// branch's check. Refuses if `target` moved since the check started, if
    /// the check fails, or if the merge conflicts.
    pub fn merge(&self, branch: &str, target: &str) -> Result<Merged, Error> {
        let merged = ops::merge(self, branch, target)?;
        // The outcome store learns the merge; it never undoes one.
        let _ = fleet::observe(self, branch, None);
        // So may the knowledge store, as proposals a person reviews.
        knowledge::on_end(self, branch, DistillTrigger::Merged);
        Ok(merged)
    }

    /// Remove a branch's worktree and record; deletes the git branch unless
    /// it was merged. Its private home goes too unless a fork shares it,
    /// and in any case the credential files provisioning wrote there; see
    /// [`Yard::remove_with`] to keep those.
    pub fn remove(&self, branch: &str) -> Result<(), Error> {
        ops::remove(self, branch, &RemoveOptions::default()).map(|_| ())
    }

    /// [`Yard::remove`], with options.
    pub fn remove_with(&self, branch: &str, options: &RemoveOptions) -> Result<(), Error> {
        ops::remove(self, branch, options).map(|_| ())
    }

    /// [`Yard::remove_with`], returning what the branch's workspace
    /// teardown did, when it has one; see `docs/workspace.md`. A failed
    /// teardown does not stop the removal.
    pub fn remove_reporting(
        &self,
        branch: &str,
        options: &RemoveOptions,
    ) -> Result<Option<WorkspaceReport>, Error> {
        ops::remove(self, branch, options)
    }

    /// `branch`'s workspace: what it was created with, whether its setup
    /// completed, and its reserved port. See `docs/workspace.md`.
    pub fn workspace(&self, branch: &str) -> Result<WorkspaceInfo, Error> {
        let store = self.store();
        let record = store.read(branch)?;
        let port = store.ports().port(branch)?;
        Ok(WorkspaceInfo {
            branch: branch.to_owned(),
            worktree: record.info.worktree.clone(),
            spec: record.workspace.as_ref().map(|w| w.spec.clone()),
            ready: record.workspace.as_ref().is_some_and(|w| w.ready),
            copied: workspace::excluded(&record),
            port,
        })
    }

    /// The prepared environments under `.branchyard/environments/`, and
    /// the builds recorded as failed, newest built first. See
    /// `docs/environments.md`.
    pub fn environments(&self) -> Vec<EnvironmentInfo> {
        environments::list(&self.root)
    }

    /// The environment key `spec` has on this host for the repository's
    /// checkout at its root, as a branch created from it now would.
    pub fn environment_key(&self, spec: &WorkspaceSpec) -> String {
        environments::current_key(&self.root, spec)
    }

    /// Build `spec`'s environment on this host now, from `HEAD`, in a
    /// temporary worktree, replacing the key's environment only when setup
    /// succeeds; a failure is recorded and the last good build stays. The
    /// caller decides whether `spec`'s scripts may run, as for
    /// [`TaskOptions::workspace`].
    pub fn rebuild_environment(&self, spec: &WorkspaceSpec) -> Result<EnvironmentBuild, Error> {
        environments::rebuild(self, spec)
    }

    /// Remove environments beyond the newest `keep` of each recipe or unused
    /// for longer than `max_age`, and old failures; never the newest good
    /// one of a recipe, nor one a branch links into. With `only`, just those
    /// keys.
    pub fn prune_environments(
        &self,
        keep: usize,
        max_age: Duration,
        only: &[String],
    ) -> environments::Pruned {
        environments::prune(self, keep, max_age, only)
    }

    /// The variables `branch`'s scripts and harness get:
    /// `BRANCHYARD_BRANCH`, `BRANCHYARD_WORKTREE`, `BRANCHYARD_ROOT` and
    /// `BRANCHYARD_PORT`, reserving the branch's port now if it has none
    /// yet. For running a command in its worktree, as `by workspace run`
    /// does.
    pub fn workspace_env(&self, branch: &str) -> Result<Vec<(String, String)>, Error> {
        let store = self.store();
        let record = store.read(branch)?;
        let port = workspace::reserve_port(&store, &self.root, branch)?;
        Ok(workspace::variables(
            self,
            branch,
            &record.info.worktree,
            Some(port),
        ))
    }

    /// Append a workspace lifecycle report to `branch`'s event log, for a
    /// phase run outside the engine, such as `by workspace run`.
    pub fn record_workspace(&self, branch: &str, report: WorkspaceReport) -> Result<(), Error> {
        let store = self.store();
        store.read(branch)?;
        record::Recorder::open(&store, branch, None)?.record(Activity::Workspace(report))
    }

    /// Never run workspace setup or teardown commands on this yard (or its
    /// clones): a branch that needs its setup fails, saying why, and a
    /// teardown is skipped and recorded. Copying files still happens. A
    /// server does this unless its operator allowed a repository's
    /// scripts; see `docs/workspace.md`.
    pub fn deny_workspace_scripts(&self) {
        self.hub.deny_scripts();
    }

    /// Run every Microsandbox-provider branch of this yard (and its clones)
    /// through `provider` instead of the Microsandbox SDK: its capabilities
    /// decide what is kept, snapshotted and branched. For embedding a
    /// provider built elsewhere, and for tests with
    /// `branchyard_sandbox::fake::FakeProvider`.
    pub fn use_sandbox_provider(&self, provider: Arc<dyn branchyard_sandbox::SandboxProvider>) {
        *projection::lock(&self.hub.sandbox_provider) = Some(provider);
    }

    /// The named branches side by side: status, turns, cost, tokens, time,
    /// diff stats and the files only each one changed; with `run_checks`,
    /// each branch's check run on its exact candidate in a private
    /// worktree. See `docs/checkpoints.md`.
    pub fn compare(&self, branches: &[String], run_checks: bool) -> Result<Vec<Attempt>, Error> {
        compare::compare(self, branches, run_checks)
    }

    /// Make a branch of a harness session that already exists on this
    /// machine: a worktree at `spec.base` (with `spec.diff` applied), the
    /// session recorded as the branch's, settled `no_changes`, so its next
    /// turn resumes the session. Records [`Activity::Adopted`].
    pub fn adopt(&self, spec: AdoptSpec) -> Result<Branch, Error> {
        adopt::adopt(self, spec)
    }

    /// The branches one `by fan` started as `<name>-<harness>`.
    pub fn fan_branches(&self, name: &str) -> Result<Vec<String>, Error> {
        compare::fan(self, name)
    }

    /// The diff from branch `a`'s candidate (or base) to `b`'s.
    pub fn diff_between(&self, a: &str, b: &str) -> Result<String, Error> {
        compare::between(self, a, b)
    }

    /// What the router would choose for `prompt` under `fleet`, without
    /// running anything: the kind, the entry, the candidates picked (one,
    /// or `attempts`) and those excluded, and why. See `docs/fleet.md`.
    pub fn route(
        &self,
        prompt: &str,
        options: &TaskOptions,
        fleet: &Fleet,
        how: &RouteOptions,
        attempts: Option<u32>,
    ) -> Result<Route, Error> {
        fleet::route(self, prompt, options, fleet, how, attempts).map(|(route, _)| route)
    }

    /// Run `prompt` on the candidate the router picks from `fleet`, failing
    /// over to the next while its harness fails, when the entry (or `how`)
    /// asks for failover. `options.harness` is ignored.
    pub fn run_routed(
        &self,
        prompt: &str,
        options: &TaskOptions,
        fleet: &Fleet,
        how: &RouteOptions,
    ) -> Result<Routed, Error> {
        fleet::run_routed(self, prompt, options, fleet, how, false)
    }

    /// A fan of the entry's attempts (or `how.attempts`) on the candidates
    /// the router picks, each failing over as [`Yard::run_routed`] does.
    /// Named `<name>-<harness>`, then `-2`, `-3` for a harness picked again.
    pub fn fan_routed(
        &self,
        prompt: &str,
        options: &TaskOptions,
        fleet: &Fleet,
        how: &RouteOptions,
    ) -> Result<Routed, Error> {
        fleet::run_routed(self, prompt, options, fleet, how, true)
    }

    /// Run `prompt` on `options.harness` as [`TaskBuilder::run`] does,
    /// recording `kind` on the branch for the outcome store.
    pub fn run_with_kind(
        &self,
        prompt: &str,
        options: &TaskOptions,
        kind: TaskKind,
    ) -> Result<Branch, Error> {
        let branch = fleet::run_with_kind(self, prompt, options, kind)?;
        goal::pursue(self, branch, options)
    }

    /// When `branch`'s last turn failed because of its harness and it was
    /// routed with failover, start its task on the next candidate and
    /// return that branch; `None` otherwise. Routed runs and fans do this
    /// themselves; call it after a send.
    pub fn failover(&self, branch: &str, options: &TaskOptions) -> Result<Option<Branch>, Error> {
        fleet::failover(self, branch, options)
    }

    /// Judge attempts at one task: their checks, a deterministic score,
    /// optionally a judge's verdict, a ranking and a proposed pick. See
    /// `docs/fleet.md`.
    pub fn judge(&self, branches: &[String], options: &JudgeOptions) -> Result<Judgement, Error> {
        judge::judge(self, branches, options)
    }

    /// How this yard (and its clones) learns and uses repository
    /// knowledge; see `docs/knowledge.md`. Replaces the settings set
    /// before; without a call, [`KnowledgeSettings::default`].
    pub fn use_knowledge(&self, settings: KnowledgeSettings) {
        *projection::lock(&self.hub.knowledge) = Some(Arc::new(settings));
    }

    /// The settings [`Yard::use_knowledge`] set, or the defaults.
    pub fn knowledge_settings(&self) -> Arc<KnowledgeSettings> {
        projection::lock(&self.hub.knowledge)
            .clone()
            .unwrap_or_default()
    }

    /// The repository's knowledge entries, or those of `status`, by id.
    pub fn knowledge(&self, status: Option<KnowledgeStatus>) -> Result<Vec<KnowledgeEntry>, Error> {
        knowledge::list(self, status)
    }

    /// One knowledge entry.
    pub fn knowledge_entry(&self, id: u64) -> Result<KnowledgeEntry, Error> {
        knowledge::get(self, id)
    }

    /// Add an entry written by `by`: adopted, unless `new.propose`.
    pub fn add_knowledge(&self, new: &NewKnowledge, by: &str) -> Result<KnowledgeEntry, Error> {
        knowledge::add(self, new, by)
    }

    /// Adopt entry `id` as `by`: from now on, matching branches are given
    /// it.
    pub fn adopt_knowledge(&self, id: u64, by: &str) -> Result<KnowledgeEntry, Error> {
        knowledge::decide(self, id, KnowledgeStatus::Adopted, by, None)
    }

    /// Reject entry `id` as `by`, with an optional reason: it is not used,
    /// and the same text is not proposed again.
    pub fn reject_knowledge(
        &self,
        id: u64,
        by: &str,
        reason: Option<&str>,
    ) -> Result<KnowledgeEntry, Error> {
        knowledge::decide(self, id, KnowledgeStatus::Rejected, by, reason)
    }

    /// Change entry `id`'s text or scope; it keeps its status.
    pub fn edit_knowledge(
        &self,
        id: u64,
        change: &KnowledgeEdit,
        by: &str,
    ) -> Result<KnowledgeEntry, Error> {
        knowledge::edit(self, id, change, by)
    }

    /// Delete entry `id`, returning what it was.
    pub fn remove_knowledge(&self, id: u64) -> Result<KnowledgeEntry, Error> {
        knowledge::remove(self, id)
    }

    /// Propose knowledge from `branch` now: with `distiller`, its answer,
    /// else (or when its answer is refused) the deterministic extractor's.
    /// Nothing is adopted. Recorded on the branch.
    pub fn distill(
        &self,
        branch: &str,
        distiller: Option<Arc<dyn Judge>>,
    ) -> Result<Distilled, Error> {
        knowledge::distill(self, branch, distiller.as_ref(), "asked")
    }

    /// `branch`'s plan, when it was started with one; see
    /// `docs/plans-and-goals.md`.
    pub fn plan(&self, branch: &str) -> Result<PlanInfo, Error> {
        plan::info(self, branch)
    }

    /// Approve `branch`'s plan as `by`, or `edited` in its place, and run
    /// it as the next turn under `options` (its policy, observer, limits);
    /// then pursue the branch's goal, if it has one.
    pub fn approve_plan(
        &self,
        branch: &str,
        edited: Option<&str>,
        by: &str,
        options: &TaskOptions,
    ) -> Result<Branch, Error> {
        plan::approve(self, branch, edited, by, options)
    }

    /// Reject `branch`'s plan as `by`: the branch fails, or with `replan`,
    /// another read-only planning turn runs with `reason`.
    pub fn reject_plan(
        &self,
        branch: &str,
        reason: Option<&str>,
        replan: bool,
        by: &str,
        options: &TaskOptions,
    ) -> Result<Branch, Error> {
        plan::reject(self, branch, reason, replan, by, options)
    }

    /// `branch`'s goal and its latest verdict, when it has one.
    pub fn goal(&self, branch: &str) -> Result<Option<GoalInfo>, Error> {
        goal::info(self, branch)
    }

    /// Finished branches' outcomes, or those of `kind`, oldest first; they
    /// outlive their branches.
    pub fn outcomes(&self, kind: Option<TaskKind>) -> Result<Vec<OutcomeRecord>, Error> {
        self.store().outcomes().outcomes(kind)
    }

    /// Apply `branch`'s candidate diff to this checkout, which must be
    /// clean, recording what it changed under `.branchyard/try/` so
    /// [`Yard::try_off`] restores it exactly. A try of another branch is
    /// turned off first. Refused, with nothing applied, when the diff does
    /// not apply. See `docs/checkpoints.md`.
    pub fn try_on(&self, branch: &str) -> Result<TryState, Error> {
        spotlight::on(self, branch)
    }

    /// Restore the checkout to what it held before [`Yard::try_on`].
    /// Refused when a tried file changed since, or `HEAD` moved, unless
    /// `force`. `None` when nothing was tried.
    pub fn try_off(&self, force: bool) -> Result<Option<TryState>, Error> {
        spotlight::off(self, force)
    }

    /// The try in effect, if any.
    pub fn try_status(&self) -> Result<Option<TryState>, Error> {
        spotlight::status(self)
    }

    /// The try recorded, read without taking the try lock or recovering a
    /// half-done one: a glance for a dashboard that refreshes often, which
    /// must not contend with a `try_*` in progress.
    pub fn try_recorded(&self) -> Result<Option<TryState>, Error> {
        spotlight::load(self)
    }

    /// Roll back a try a stopped process left half-applied or
    /// half-restored; says what was done. Every `try_*` call does this
    /// first.
    pub fn try_recover(&self) -> Result<Option<String>, Error> {
        spotlight::recover(self)
    }

    /// Known harness profiles, whether their executable is on `PATH`, and
    /// their live qualification status.
    pub fn harnesses(&self) -> Vec<HarnessInfo> {
        harness::list()
    }

    /// Publish the file at `path` as a new immutable artifact of `branch`,
    /// content-addressed by the blake3 digest of its bytes. See
    /// [`Branch::publish`] and `docs/storage.md`.
    pub fn publish_artifact(
        &self,
        branch: &str,
        path: impl AsRef<Path>,
        name: Option<String>,
        media_type: Option<String>,
        labels: BTreeMap<String, String>,
    ) -> Result<ArtifactRef, Error> {
        storage::publish(self, branch, path.as_ref(), name, media_type, labels)
    }

    /// Every artifact `reader` may read: what it published, what its
    /// ancestors or descendants published, and what was explicitly shared
    /// to it with [`Yard::share_artifact`].
    pub fn artifacts(&self, reader: &str) -> Result<Vec<ArtifactRef>, Error> {
        storage::list(self, reader)
    }

    /// Copy artifact `id`'s bytes to `out` for `reader`, checked against
    /// its recorded digest, and return its provenance.
    pub fn read_artifact(
        &self,
        reader: &str,
        id: &str,
        out: impl AsRef<Path>,
    ) -> Result<ArtifactRef, Error> {
        storage::get(self, reader, id, out.as_ref())
    }

    /// Share artifact `id` (published, or already shared, to `actor`) with
    /// `to`: the explicit grant a sibling of the publisher needs.
    pub fn share_artifact(&self, actor: &str, id: &str, to: &str) -> Result<(), Error> {
        storage::share_artifact(self, actor, id, to)
    }

    /// Export every artifact in `ids` that `reader` may read into a
    /// portable, deterministic tar bundle at `out`: the same artifacts
    /// always produce byte-identical bytes, and each member is checked
    /// against its own recorded digest before being written. See
    /// [`Yard::import_artifacts`] and `docs/storage.md` "Portable
    /// bundles".
    pub fn export_artifacts(
        &self,
        reader: &str,
        ids: &[String],
        out: impl AsRef<Path>,
    ) -> Result<Vec<BundleEntry>, Error> {
        bundle::export_artifacts(self, reader, ids, out.as_ref())
    }

    /// Import a bundle written by [`Yard::export_artifacts`], publishing
    /// each member as a new artifact owned by `branch`. Every member is
    /// verified against the bundle's index and its own content digest; a
    /// tampered, missing or unindexed extra member refuses the whole
    /// import, before anything is published. Each imported artifact's
    /// labels record its original provenance (`bundle.origin_id`,
    /// `bundle.origin_publisher`, `bundle.origin_created_at`).
    pub fn import_artifacts(
        &self,
        branch: &str,
        path: impl AsRef<Path>,
    ) -> Result<Vec<ArtifactRef>, Error> {
        bundle::import_artifacts(self, branch, path.as_ref())
    }

    /// Create scratch area `name`, a shared directory owned by `owner`,
    /// visible to its authorized branches at [`Yard::scratch_path`]. See
    /// `docs/storage.md`.
    pub fn create_scratch(&self, owner: &str, name: &str) -> Result<ScratchArea, Error> {
        storage::create_scratch(self, owner, name)
    }

    /// Every scratch area `reader` may reach.
    pub fn scratch_areas(&self, reader: &str) -> Result<Vec<ScratchArea>, Error> {
        storage::authorized_scratch(self, reader)
    }

    /// Share scratch area `name` (owned, or already shared, to `actor`)
    /// with `to`.
    pub fn share_scratch(&self, actor: &str, name: &str, to: &str) -> Result<(), Error> {
        storage::share_scratch(self, actor, name, to)
    }

    /// Where scratch area `name` lives on disk in local mode.
    pub fn scratch_path(&self, name: &str) -> PathBuf {
        storage::scratch_dir(&self.store(), name)
    }

    /// Acquire scratch area `name`'s writer lock for `branch`. Refused with
    /// [`Error::Running`] while another branch's turn holds it.
    pub fn lock_scratch(&self, branch: &str, name: &str) -> Result<ScratchLock, Error> {
        storage::lock_scratch(self, branch, name)
    }

    /// Release scratch area `name`'s lock if `branch` holds it.
    pub fn unlock_scratch(&self, branch: &str, name: &str) -> Result<(), Error> {
        storage::unlock_scratch(self, branch, name)
    }

    /// Scratch area `name`'s writer lock, if one is held.
    pub fn scratch_lock_state(&self, name: &str) -> Result<Option<ScratchLock>, Error> {
        storage::scratch_lock_state(self, name)
    }
}

/// The profile a harness or profile ID selects, from the built-in
/// registry, without looking for its executable.
pub fn harness_profile(id: &str) -> Result<HarnessProfile, Error> {
    let profile = harness::select(Some(id))?;
    Ok(HarnessProfile {
        harness: profile.harness.to_owned(),
        profile: profile.id.to_owned(),
        tool_approvals: profile.driver().capabilities().tool_approvals,
    })
}

/// A harness profile as [`harness_profile`] resolves it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessProfile {
    pub harness: String,
    pub profile: String,
    /// Whether its driver routes tool permission requests to the policy;
    /// without it, a task needs [`TaskOptions::unapproved_tools`].
    pub tool_approvals: bool,
}

impl Yard {
    /// Act as the branch whose running turn was issued `token`. The token
    /// is the authority and names the branch: it is issued when a
    /// delegating turn starts and revoked when it ends. Works in the
    /// process that runs the turn and, through its broker, in any other.
    /// Fails with [`Error::Denied`] for any other token.
    pub fn as_branch(&self, token: &str) -> Result<Delegate, Error> {
        delegation::as_branch(self, token)
    }

    fn store(&self) -> state::Store {
        self.store.clone()
    }
}

/// How [`Yard::remove_with`] removes a branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoveOptions {
    /// Keep the credential files provisioning wrote in the branch's
    /// private home (API keys, auth files, MCP configurations), recorded in
    /// the home's `.branchyard/provisioned.json`. Off by default: they are
    /// removed. It matters only when the home stays because a fork shares
    /// it; a home no branch uses is deleted whole.
    pub keep_credentials: bool,
}

/// Options shared by tasks, sends and forks.
#[derive(Clone, Default)]
pub struct TaskOptions {
    /// Harness ID (such as `claude-code`) or profile ID. Defaults to
    /// `claude-code`. A send keeps its branch's harness; a fork may name
    /// another only with a fresh session.
    pub harness: Option<String>,
    /// Branch name; defaults to a slug of the prompt, made unique.
    pub name: Option<String>,
    /// Base revision; defaults to `HEAD`.
    pub base: Option<String>,
    /// Limits for the branch. Cost and turns count across the branch's
    /// run and sends; the duration applies to each call.
    pub budget: Budget,
    pub policy: Policy,
    /// Check run in the merge worktree before promotion. Stored with the
    /// branch; a send or fork without one keeps the branch's.
    pub check: Option<Vec<String>>,
    pub observer: Option<Observer>,
    /// Run the harness with a scrubbed environment (no `ANTHROPIC*`,
    /// `CLAUDE*`, `OPENAI*` or `CODEX*` variables) and a private `HOME`
    /// under `.branchyard/homes/`. A harness then usually has no login.
    /// Off by default: local mode runs harnesses with your own login. A
    /// send keeps its branch's environment, since the session lives in it;
    /// a forked session shares its parent's private home.
    pub isolated: bool,
    /// Executable and fixed arguments replacing the profile's own, such as
    /// an absolute path to a harness installed elsewhere. The profile's
    /// driver still appends its protocol arguments. Stored with the branch
    /// for later sends and forks.
    pub command: Option<Vec<String>>,
    /// Let the harness create and coordinate child branches within this
    /// envelope, through Branchyard's MCP tools. Stored with the branch; a
    /// send without one keeps the branch's. A delegated child's envelope
    /// is fixed by its parent and not changed here.
    pub delegation: Option<Envelope>,
    /// The `by` executable a delegating harness gets: its directory goes
    /// first on the harness's `PATH`, its path in [`ENV_BY`], and `by mcp`
    /// is the harness's MCP server. Defaults to the running executable when
    /// it is `by`, else `by` beside it, else on `PATH`. Without one, the
    /// harness gets only the MCP server and the Python module cannot work.
    /// Children use their parent's.
    pub delegation_cli: Option<PathBuf>,
    /// The command that starts Branchyard's MCP server, when it is not
    /// `by mcp`: for example `["/opt/branchyard/bin/branchyard-mcp"]`. The
    /// engine appends `--root <root> --branch <name>` and passes the token
    /// in [`ENV_TOKEN`]. Children use their parent's.
    pub delegation_server: Option<Vec<String>>,
    /// Run a profile whose driver cannot route tool permission requests
    /// to Branchyard, such as the Antigravity, Pi and Amp profiles. Its
    /// tools then run under the harness's own configuration, and
    /// [`TaskOptions::policy`] never sees them. Off by default: such a
    /// profile is refused, for every run, send, fork and delegated spawn.
    pub unapproved_tools: bool,
    /// Where the harness runs. `None` runs a new branch as a local process
    /// and keeps a branch's own provider for its sends and forks. Stored
    /// with the branch.
    pub provider: Option<Provider>,
    /// What to prepare in the harness's home and environment before each
    /// turn: secrets, MCP servers, instructions, model, reasoning effort,
    /// telemetry. See `docs/provisioning.md`. Stored with the branch, with
    /// where each secret comes from but never its value; a send or fork
    /// without one keeps the branch's, and a delegated child inherits its
    /// parent's. Secrets need a home private to the branch:
    /// [`TaskOptions::isolated`] or a sandbox provider.
    pub provision: Option<Provisioning>,
    /// Make the branch the root of a rig: the seat it occupies and the
    /// seats below it, which its harness then spawns by name (see
    /// `docs/rigs.md`). Needs [`TaskOptions::delegation`], which still
    /// bounds every child. Stored with the branch; a send without one keeps
    /// the branch's.
    pub seats: Option<Seats>,
    /// Prepare each new branch's worktree before its first turn (copy
    /// untracked files, run setup) and clean up when it is removed
    /// (teardown), with `BRANCHYARD_PORT` reserved for it. Read only when a
    /// branch is created (run, fan, fork, reincarnate); stored with it, and
    /// a fork, reincarnation or delegated child without one takes its
    /// parent's. Whether a repository's scripts may run is the caller's
    /// decision: `by` asks you to trust them. See `docs/workspace.md`.
    pub workspace: Option<WorkspaceSpec>,
    /// Who a new branch acts for at the connector gateway: its tokens'
    /// `sub` and `by_tenant`. Read only when a branch is created (run, fan,
    /// fork, reincarnate); a fork without one keeps its parent's, and a
    /// delegated child always has its parent's. `None`: the yard's gateway
    /// default (`local:<user>` locally). A server sets it to the request's
    /// principal. See `docs/connectors.md`.
    pub actor: Option<connectors::Actor>,
    /// A W3C `traceparent` for this call's turns: each turn's harness gets
    /// it as `TRACEPARENT`, so what it calls (a connector SDK putting it in
    /// the gateway call's `_meta`, a tool exporting its own spans) joins
    /// the caller's trace. Not stored with the branch. A server sets it to
    /// its operation's span; see `docs/observability.md`.
    pub trace_parent: Option<String>,
    /// Plan first: a new branch's first turn runs read-only with a planning
    /// prompt and the branch then waits, `awaiting_plan_approval`, for
    /// [`Yard::approve_plan`] or [`Yard::reject_plan`]. Read only when a
    /// branch is created by `run`, `run_on` or a routed run; see
    /// `docs/plans-and-goals.md`.
    pub plan: bool,
    /// A goal a judge verifies when a new branch's turn ends ready: unmet,
    /// it gets follow-up turns with what is missing. Read only when a
    /// branch is created by `run`, `run_on` or a routed run; stored with
    /// it, except a custom judge.
    pub goal: Option<Goal>,
}

/// The variable a turn's harness gets [`TaskOptions::trace_parent`] in.
pub const ENV_TRACEPARENT: &str = "TRACEPARENT";

/// Where a branch's harness runs.
///
/// Serialized as an object tagged by `kind`, such as `{"kind": "local"}` or
/// `{"kind": "microsandbox", "image": "...", ...}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// One per branch, built once per command: the size does not matter, and
// boxing would change how every caller constructs a provider.
#[allow(clippy::large_enum_variant)]
pub enum Provider {
    /// A local process as your user; no isolation. The default.
    Local,
    /// A Microsandbox microVM per turn, booted from an image that has the
    /// harness installed. Needs a build with the `microsandbox` feature and
    /// a Linux host with KVM.
    Microsandbox(SandboxOptions),
    /// An Agent Substrate actor per turn, from a template that runs the
    /// Branchyard bridge and has the harness installed. Unqualified: see
    /// `docs/substrate.md`.
    Substrate(SubstrateOptions),
}

/// A sandboxed harness's image, limits and credentials.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SandboxOptions {
    /// OCI image reference. The harness executable must be installed in it
    /// and on its `PATH`, or named by an absolute guest path in
    /// [`TaskOptions::command`].
    pub image: String,
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
    /// Variables copied by name from this process into the sandbox, such as
    /// `ANTHROPIC_API_KEY`. Nothing else is: not your login, not your
    /// `HOME`. Names are stored with the branch; values are not.
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// What happens to the branch's microVM when a turn ends: destroyed
    /// (the default), or paused and resumed by the next turn. Needs
    /// [`SandboxOptions::live_branch`]; see `docs/sandbox-snapshots.md`.
    #[serde(default, skip_serializing_if = "SandboxKeep::is_destroy")]
    pub keep: SandboxKeep,
    /// With a kept sandbox, how many checkpoints keep a provider snapshot
    /// as well (the newest; older ones are released). Unset: 3. 0: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<u32>,
    /// At most this many kept (paused) sandboxes of this provider in the
    /// repository; parking one more destroys the least recently used.
    /// Unset: 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_paused: Option<u32>,
    /// Opt in to the Microsandbox SDK's pause, resume, live branching and
    /// full-memory snapshots, which are declared only then: unqualified
    /// until they pass on a KVM host.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub live_branch: bool,
}

/// Where an Agent Substrate cluster is and how a harness runs in it.
///
/// Each turn creates an actor from [`SubstrateOptions::template`], copies the
/// worktree into it at [`SubstrateOptions::workdir`] and the branch's private
/// home to [`SubstrateOptions::home`], runs the harness there through the
/// bridge, copies both back and deletes the actor. See `docs/substrate.md`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SubstrateOptions {
    /// The `Control` API, as `https://host:port`, or `http://host:port` on
    /// loopback or with [`SubstrateOptions::insecure`].
    pub endpoint: String,
    /// The router URL of an actor's bridge, with `{atespace}` and `{actor}`
    /// in place of the names: `https://` or `wss://`, or `http://` or
    /// `ws://` on loopback or with [`SubstrateOptions::insecure`].
    pub router: String,
    /// The atespace actors are created in. Empty means `default`.
    #[serde(default)]
    pub atespace: String,
    /// An actor template that runs `branchyard-bridge` with the public half
    /// of [`SubstrateOptions::key`], with git and the harness installed.
    pub template: String,
    /// The host's bridge signing key, from `branchyard-bridge keygen`. The
    /// path is stored with the branch; the key is not.
    pub key: PathBuf,
    /// Where the worktree is placed in the actor. Empty means `/workspace`.
    #[serde(default)]
    pub workdir: String,
    /// The harness's `HOME` in the actor. Empty means `/branchyard/home`.
    #[serde(default)]
    pub home: String,
    /// Variables copied by name from this process into the actor, such as
    /// `ANTHROPIC_API_KEY`. Names are stored with the branch; values are
    /// not.
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// PEM certificate authorities for a TLS `Control` API, and for the
    /// router unless [`SubstrateOptions::router_ca`] is set. Unset trusts
    /// the public roots bundled at build time.
    #[serde(default)]
    pub ca: Option<PathBuf>,
    /// A PEM client certificate and key for the `Control` API (mutual TLS).
    #[serde(default)]
    pub client_cert: Option<PathBuf>,
    #[serde(default)]
    pub client_key: Option<PathBuf>,
    /// PEM certificate authorities for a TLS router.
    #[serde(default)]
    pub router_ca: Option<PathBuf>,
    /// Allow a `Control` endpoint or router in the clear to a host other
    /// than loopback, sending credentials and code unencrypted.
    #[serde(default)]
    pub insecure: bool,
    /// What happens to the branch's actor when a turn ends: deleted (the
    /// default), or paused (`PauseActor`) and resumed by the next turn. See
    /// `docs/sandbox-snapshots.md`.
    #[serde(default, skip_serializing_if = "SandboxKeep::is_destroy")]
    pub keep: SandboxKeep,
    /// With a kept actor, how many checkpoints keep a tag as well (the
    /// newest; older tags are deleted). Unset: 3. 0: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<u32>,
    /// At most this many kept actors in the repository for this cluster;
    /// parking one more deletes the least recently used. Unset: 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_paused: Option<u32>,
}

impl SubstrateOptions {
    /// [`SubstrateOptions::atespace`], defaulted.
    pub fn atespace(&self) -> &str {
        match self.atespace.is_empty() {
            true => "default",
            false => &self.atespace,
        }
    }

    /// [`SubstrateOptions::workdir`], defaulted.
    pub fn workdir(&self) -> &str {
        match self.workdir.is_empty() {
            true => placement::WORKSPACE,
            false => &self.workdir,
        }
    }

    /// [`SubstrateOptions::home`], defaulted.
    pub fn home(&self) -> &str {
        match self.home.is_empty() {
            true => placement::HOME,
            false => &self.home,
        }
    }
}

/// Receives every activity as it is recorded, from any branch's thread.
pub type Observer = Arc<dyn Fn(&BranchEvent) + Send + Sync>;

/// Builds and runs a task.
pub struct TaskBuilder {
    yard: Yard,
    prompt: String,
    options: TaskOptions,
}

impl TaskBuilder {
    pub fn harness(mut self, harness: impl Into<String>) -> Self {
        self.options.harness = Some(harness.into());
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.options.name = Some(name.into());
        self
    }

    pub fn base(mut self, rev: impl Into<String>) -> Self {
        self.options.base = Some(rev.into());
        self
    }

    pub fn budget(mut self, budget: Budget) -> Self {
        self.options.budget = budget;
        self
    }

    pub fn policy(mut self, policy: Policy) -> Self {
        self.options.policy = policy;
        self
    }

    pub fn check<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options.check = Some(argv.into_iter().map(Into::into).collect());
        self
    }

    pub fn on_event(mut self, observer: impl Fn(&BranchEvent) + Send + Sync + 'static) -> Self {
        self.options.observer = Some(Arc::new(observer));
        self
    }

    /// See [`TaskOptions::isolated`].
    pub fn isolated(mut self, isolated: bool) -> Self {
        self.options.isolated = isolated;
        self
    }

    /// See [`TaskOptions::unapproved_tools`].
    pub fn unapproved_tools(mut self, allowed: bool) -> Self {
        self.options.unapproved_tools = allowed;
        self
    }

    /// See [`TaskOptions::provider`].
    pub fn provider(mut self, provider: Provider) -> Self {
        self.options.provider = Some(provider);
        self
    }

    /// See [`TaskOptions::command`].
    pub fn command<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options.command = Some(argv.into_iter().map(Into::into).collect());
        self
    }

    /// See [`TaskOptions::provision`].
    pub fn provision(mut self, provision: Provisioning) -> Self {
        self.options.provision = Some(provision);
        self
    }

    /// Let the harness delegate within `envelope`; see
    /// [`TaskOptions::delegation`].
    pub fn delegate(mut self, envelope: Envelope) -> Self {
        self.options.delegation = Some(envelope);
        self
    }

    /// Plan first; see [`TaskOptions::plan`].
    pub fn plan(mut self, plan: bool) -> Self {
        self.options.plan = plan;
        self
    }

    /// Verify `goal` with a judge; see [`TaskOptions::goal`].
    pub fn goal(mut self, goal: Goal) -> Self {
        self.options.goal = Some(goal);
        self
    }

    /// Replace every option at once.
    pub fn options(mut self, options: TaskOptions) -> Self {
        self.options = options;
        self
    }

    /// The branch names [`TaskBuilder::run`] (for an empty `harnesses`) or
    /// [`TaskBuilder::run_on`] would create now, in order. They agree unless
    /// another caller creates a branch in between.
    pub fn planned_names(&self, harnesses: &[&str]) -> Result<Vec<String>, Error> {
        run::planned_names(&self.yard, &self.prompt, &self.options, harnesses)
    }

    /// Create the branch, run the prompt as one turn, and snapshot the
    /// candidate. Returns once the turn ends; the branch's status says how.
    /// Errors mean nothing ran: an unknown or missing harness, an invalid
    /// name or base, or unwritable state.
    pub fn run(self) -> Result<Branch, Error> {
        let branch = run::run(&self.yard, &self.prompt, &self.options)?;
        goal::pursue(&self.yard, branch, &self.options)
    }

    /// Run the same task on several harnesses in parallel, one branch each,
    /// named `<name>-<harness>`. Every harness is resolved and found before
    /// any branch is created. A failure on one branch does not stop the
    /// others; it is recorded in that branch's status.
    pub fn run_on(self, harnesses: &[&str]) -> Result<Vec<Branch>, Error> {
        let branches = run::run_on(&self.yard, &self.prompt, &self.options, harnesses)?;
        goal::pursue_all(&self.yard, branches, &self.options)
    }
}

/// A branch: one harness session working in one worktree.
#[derive(Clone, Debug)]
pub struct Branch {
    yard: Yard,
    info: BranchInfo,
}

impl Branch {
    pub fn info(&self) -> &BranchInfo {
        &self.info
    }

    pub fn yard(&self) -> &Yard {
        &self.yard
    }

    /// Continue this branch's harness session with another prompt in a new
    /// harness process, then snapshot a new candidate.
    pub fn send(&self, prompt: &str, options: TaskOptions) -> Result<Branch, Error> {
        run::send(&self.yard, &self.info.name, prompt, &options)
    }

    /// [`Branch::send`], named for parity with [`Delegate::send_and_wait`]:
    /// outside a harness a send already runs the turn and returns once it
    /// settles, so this does exactly what `send` does.
    pub fn send_and_wait(&self, prompt: &str, options: TaskOptions) -> Result<Branch, Error> {
        self.send(prompt, options)
    }

    /// A new branch from this branch's candidate. The harness session is
    /// forked when the harness supports it; otherwise this fails unless
    /// `fresh_session` is true, in which case the new branch starts a fresh
    /// session on the forked code.
    pub fn fork(
        &self,
        prompt: &str,
        fresh_session: bool,
        options: TaskOptions,
    ) -> Result<Branch, Error> {
        run::fork(
            &self.yard,
            &self.info.name,
            prompt,
            fresh_session,
            None,
            &options,
        )
    }

    /// A new branch from this branch's checkpoint `turn` (0 is its base),
    /// leaving this branch as it is. The harness session is forked natively
    /// only when it ended at that checkpoint and the harness can fork;
    /// otherwise the new branch starts a fresh session whose first prompt
    /// begins with a generated summary of the turns that led there, and an
    /// [`Activity::ForkedAt`] on the new branch says which. See
    /// `docs/checkpoints.md`.
    pub fn fork_at(&self, turn: u32, prompt: &str, options: TaskOptions) -> Result<Branch, Error> {
        run::fork(
            &self.yard,
            &self.info.name,
            prompt,
            false,
            Some(turn),
            &options,
        )
    }

    /// Reset this branch, its worktree and its candidate to checkpoint
    /// `turn` (0 is its base). Refused while a turn runs, for a merged
    /// branch, and when the worktree holds changes no checkpoint has.
    /// Later checkpoints are kept, so rewinding to one of them undoes this.
    /// The next turn resumes the harness's own session only if it ended at
    /// that checkpoint; otherwise it starts fresh with a summary. Journaled:
    /// an engine that stops mid-rewind leaves it for recovery to finish.
    pub fn rewind(&self, turn: u32) -> Result<Rewound, Error> {
        checkpoint::rewind(&self.yard, &self.info.name, turn)
    }

    /// This branch's checkpoints, oldest first, and the one it is at.
    pub fn checkpoints(&self) -> Result<Checkpoints, Error> {
        checkpoint::list(&self.yard, &self.info.name)
    }

    /// A new branch from this branch's latest candidate, always with a
    /// fresh session, whose first prompt is a generated handoff brief: the
    /// original task, turns so far, its last message, the candidate's
    /// diffstat and why it was reincarnated. Works even when
    /// `options.harness` or `options.provision`'s model differs from this
    /// branch's own. This branch's record gets `superseded_by` set to the
    /// new branch's name, best-effort (informational; never blocks the new
    /// branch). See `docs/lifecycle.md`.
    pub fn reincarnate(&self, options: TaskOptions) -> Result<Branch, Error> {
        run::reincarnate(&self.yard, &self.info.name, &options)
    }

    /// Unified diff of the candidate against the branch's base; empty when
    /// there is no candidate.
    pub fn diff(&self) -> Result<String, Error> {
        let info = self.yard.store().read(&self.info.name)?.info;
        match &info.candidate {
            None => Ok(String::new()),
            Some(candidate) => git::diff(&self.yard.root, &info.base, &candidate.commit),
        }
    }

    /// Where this branch's harness runs; `None` is a local process.
    pub fn provider(&self) -> Result<Option<Provider>, Error> {
        Ok(self.yard.store().read(&self.info.name)?.provider)
    }

    /// Recorded activity, oldest first.
    pub fn events(&self) -> Result<Vec<RecordedEvent>, Error> {
        record::read(&self.yard.store(), &self.info.name)
    }

    /// Up to `limit` recorded events after the first `cursor`, oldest
    /// first. Events are numbered from 1 in each branch; pass the page's
    /// `next_cursor` back to continue.
    pub fn events_since(&self, cursor: u64, limit: usize) -> Result<Page, Error> {
        record::since(&self.yard.store(), &self.info.name, cursor, limit)
    }

    /// [`Branch::events_since`], waiting up to `timeout` for an event when
    /// there is none yet.
    pub fn wait_for_events(
        &self,
        cursor: u64,
        limit: usize,
        timeout: Duration,
    ) -> Result<Page, Error> {
        record::wait(&self.yard.store(), &self.info.name, cursor, limit, timeout)
    }

    /// Act as this branch with your own authority: the same operations its
    /// harness gets, bounded by the envelope it was given, with no token.
    /// A branch without delegation can still inspect itself but cannot
    /// spawn. `options` supplies the policy, observer and tools for its
    /// children's turns; their limits come from the envelope and from
    /// `options.budget`, which bounds this branch.
    pub fn delegate(&self, options: TaskOptions) -> Result<Delegate, Error> {
        delegation::trusted(&self.yard, &self.info.name, options)
    }

    /// Every branch this one delegated to, directly or through its
    /// children, oldest first.
    pub fn descendants(&self) -> Result<Vec<BranchInfo>, Error> {
        delegation::descendants(&self.yard.store(), &self.info.name)
    }

    /// Ask this branch's running turn, and every running turn delegated
    /// below it, to stop; see [`Yard::cancel`].
    pub fn cancel(&self) -> Result<Vec<String>, Error> {
        self.yard.cancel(&self.info.name)
    }

    /// Deliver `text` into this branch's running turn, in whichever process
    /// runs it, without interrupting it; see [`Yard::steer_as`]. The event
    /// log names the SDK caller as its sender.
    pub fn steer(&self, text: &str) -> Result<Steer, Error> {
        self.yard.steer_as(&self.info.name, text, "the SDK caller")
    }

    /// Publish the file at `path` as a new immutable artifact of this
    /// branch; see [`Yard::publish_artifact`].
    pub fn publish(
        &self,
        path: impl AsRef<Path>,
        name: Option<String>,
        labels: BTreeMap<String, String>,
    ) -> Result<ArtifactRef, Error> {
        self.yard
            .publish_artifact(&self.info.name, path, name, None, labels)
    }

    /// Every artifact this branch may read; see [`Yard::artifacts`].
    pub fn artifacts(&self) -> Result<Vec<ArtifactRef>, Error> {
        self.yard.artifacts(&self.info.name)
    }

    /// Copy artifact `id`'s bytes to `out` for this branch; see
    /// [`Yard::read_artifact`].
    pub fn read_artifact(&self, id: &str, out: impl AsRef<Path>) -> Result<ArtifactRef, Error> {
        self.yard.read_artifact(&self.info.name, id, out)
    }

    /// Export artifacts this branch may read into a portable bundle; see
    /// [`Yard::export_artifacts`].
    pub fn export_artifacts(
        &self,
        ids: &[String],
        out: impl AsRef<Path>,
    ) -> Result<Vec<BundleEntry>, Error> {
        self.yard.export_artifacts(&self.info.name, ids, out)
    }

    /// Import a bundle, owned by this branch; see
    /// [`Yard::import_artifacts`].
    pub fn import_artifacts(&self, path: impl AsRef<Path>) -> Result<Vec<ArtifactRef>, Error> {
        self.yard.import_artifacts(&self.info.name, path)
    }

    /// Wait until no descendant of this branch is running a turn, then
    /// return the descendants' records. Descendants on threads of this
    /// process are joined; one another process drives is waited for through
    /// its durable status, and one whose engine stopped is recovered first,
    /// as [`Yard::recover`] would.
    pub fn wait_subtree(&self) -> Result<Vec<BranchInfo>, Error> {
        delegation::wait_subtree(&self.yard, &self.info.name)
    }
}

/// A branch's durable record.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    /// The git branch, `by/<name>`.
    pub git_branch: String,
    pub worktree: PathBuf,
    pub prompt: String,
    pub harness: String,
    pub profile: String,
    /// Native harness session, once known.
    pub session: Option<String>,
    /// Branch this one was forked from, or that delegated it.
    pub parent: Option<String>,
    /// Branches this one delegated to, oldest first. Forks are not
    /// children.
    #[serde(default)]
    pub children: Vec<String>,
    /// Delegation depth: 0 for a branch you started, one more than its
    /// parent's for a delegated child.
    #[serde(default)]
    pub depth: u32,
    /// The commit the branch started from.
    pub base: String,
    pub candidate: Option<CandidateInfo>,
    pub status: BranchStatus,
    pub turns: u32,
    /// The harness's own cumulative cost estimate for this branch, when it
    /// reports one. A fork's excludes its parent's.
    pub cost_usd: Option<f64>,
    /// Seconds since the Unix epoch.
    pub created_at: u64,
    /// A running turn has had no harness activity for its
    /// [`Budget::stall_after`] window; see [`Activity::Stalled`]. Always
    /// `false` once the turn has ended.
    #[serde(default)]
    pub stalled: bool,
    /// The branch [`Branch::reincarnate`] started from this one's latest
    /// candidate, with a fresh session and a handoff brief.
    #[serde(default)]
    pub superseded_by: Option<String>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateInfo {
    pub commit: String,
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}

/// Serialized as an object tagged by `state`, such as
/// `{"state": "failed", "reason": "..."}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BranchStatus {
    Running,
    /// A delegated child created with prerequisites that have not all
    /// settled yet: it has no worktree and has run no turn. See
    /// `docs/graph.md`.
    Waiting,
    /// A child that will not start because a prerequisite failed, was
    /// interrupted, stopped at a limit, is blocked itself or was removed.
    /// Changing its dependencies with a graph proposal reopens it.
    Blocked {
        reason: String,
    },
    /// The last turn completed and produced a candidate.
    Ready,
    /// A planning turn proposed a plan, and the branch waits for it to be
    /// approved, edited or rejected before anything changes; see
    /// [`Yard::approve_plan`].
    AwaitingPlanApproval,
    /// The last turn completed without changing any file.
    NoChanges,
    Interrupted,
    BudgetExceeded {
        limit: String,
    },
    Failed {
        reason: String,
    },
    Merged {
        target: String,
        commit: String,
    },
}

/// Limits enforced by the engine. The cost limit uses the harness's own
/// cumulative estimate and cannot apply to harnesses that report none.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Budget {
    pub max_usd: Option<f64>,
    pub max_turns: Option<u32>,
    pub max_duration: Option<Duration>,
    /// No harness activity (a protocol event, including a permission
    /// answer) for this long marks the branch [`Activity::Stalled`]. Never
    /// while the turn is waiting on a permission answer (the engine is
    /// blocked delivering it, not polling) or on a running child
    /// (delegation wait; see `docs/lifecycle.md`). `None` disables stall
    /// detection, the default.
    pub stall_after: Option<Duration>,
    /// What a detected stall does. Ignored when [`Budget::stall_after`] is
    /// `None`.
    pub stall_action: StallAction,
}

impl Budget {
    pub fn usd(limit: f64) -> Self {
        Budget {
            max_usd: Some(limit),
            ..Budget::default()
        }
    }

    pub fn turns(mut self, limit: u32) -> Self {
        self.max_turns = Some(limit);
        self
    }

    pub fn duration(mut self, limit: Duration) -> Self {
        self.max_duration = Some(limit);
        self
    }

    /// Mark the branch stalled after this long without harness activity.
    pub fn stall_after(mut self, window: Duration) -> Self {
        self.stall_after = Some(window);
        self
    }

    /// What a stall does; see [`Budget::stall_after`].
    pub fn stall_action(mut self, action: StallAction) -> Self {
        self.stall_action = action;
        self
    }
}

/// What a detected stall does to the turn.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallAction {
    /// Record [`Activity::Stalled`] and keep running; the branch shows
    /// `stalled: true` until new activity or the turn ends.
    #[default]
    Notify,
    /// Record [`Activity::Stalled`], then interrupt the turn as
    /// [`BranchStatus::Interrupted`], the same way a cancel does.
    Interrupt,
}

/// Answers permission requests, one invocation at a time. Never a bypass:
/// every request reaches the policy.
#[derive(Clone)]
pub struct Policy {
    rules: Vec<Rule>,
    fallback: Fallback,
}

/// Answers `(branch, request)` for requests no rule decides.
pub type Asker = Arc<dyn Fn(&str, &PermissionRequest) -> PermissionDecision + Send + Sync>;

#[derive(Clone)]
enum Fallback {
    Allow,
    Deny,
    Ask(Asker),
}

/// Decides requests whose tool name matches `tool` (exact, or `*` suffix
/// wildcard such as `mcp__*`).
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub tool: String,
    pub allow: bool,
    /// When set, the rule also requires the request to be a shell command
    /// that runs exactly this `by` (or `by` by name) with a delegation
    /// subcommand; see [`Policy::allow_delegation_commands`].
    pub delegation_by: Option<PathBuf>,
}

impl Rule {
    pub(crate) fn deny(tool: impl Into<String>) -> Rule {
        Rule {
            tool: tool.into(),
            allow: false,
            delegation_by: None,
        }
    }
}

impl Default for Policy {
    /// Deny everything not allowed by a rule.
    fn default() -> Self {
        Policy::deny_all()
    }
}

impl Policy {
    pub fn allow_all() -> Self {
        Policy {
            rules: Vec::new(),
            fallback: Fallback::Allow,
        }
    }

    pub fn deny_all() -> Self {
        Policy {
            rules: Vec::new(),
            fallback: Fallback::Deny,
        }
    }

    /// Ask `ask(branch, request)` for anything no rule decides.
    pub fn ask(
        ask: impl Fn(&str, &PermissionRequest) -> PermissionDecision + Send + Sync + 'static,
    ) -> Self {
        Policy {
            rules: Vec::new(),
            fallback: Fallback::Ask(Arc::new(ask)),
        }
    }

    pub fn allow(mut self, tool: impl Into<String>) -> Self {
        self.rules.push(Rule {
            tool: tool.into(),
            allow: true,
            delegation_by: None,
        });
        self
    }

    pub fn deny(mut self, tool: impl Into<String>) -> Self {
        self.rules.push(Rule::deny(tool));
        self
    }

    /// Allow exactly the harness's shell commands that run `by` with a
    /// delegation subcommand (`spawn`, `inspect`, `events`, `send`,
    /// `integrate`, `cancel`, `children`), and nothing else. The command
    /// must be a single simple command: plain or quoted words, no
    /// variables, substitutions, globs, redirections, pipes or command
    /// lists. Its program must be `by_path` itself or `by` by name, which
    /// a delegating harness finds first on its `PATH`. One `sh -c` or
    /// `bash -lc` wrapper, as Codex reports commands, is looked through.
    ///
    /// Opt-in, and ordered like any rule: an earlier deny rule, such as one
    /// a delegating parent imposed, still wins. The subcommands act within
    /// the branch's envelope, so allowing them grants no other authority.
    pub fn allow_delegation_commands(mut self, by_path: impl Into<PathBuf>) -> Self {
        self.rules.push(Rule {
            tool: "*".into(),
            allow: true,
            delegation_by: Some(by_path.into()),
        });
        self
    }

    /// Decide one request: the first matching rule, else the fallback.
    pub fn decide(&self, branch: &str, request: &PermissionRequest) -> PermissionDecision {
        self.decide_with_source(branch, request).0
    }

    /// [`Policy::decide`], also saying what decided.
    pub fn decide_with_source(
        &self,
        branch: &str,
        request: &PermissionRequest,
    ) -> (PermissionDecision, DecisionSource) {
        policy::decide(self, branch, request)
    }
}

/// What answered a permission request.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecisionSource {
    /// The first rule whose pattern matched the tool.
    Rule { pattern: String },
    /// The policy's fallback: [`Policy::allow_all`] or [`Policy::deny_all`].
    Default,
    /// The [`Policy::ask`] callback.
    Asked,
    /// The engine, which could not deliver the policy's answer and
    /// interrupted the turn instead.
    Engine,
}

/// Something that happened on a branch, as recorded and observed.
///
/// Serialized with the variant as the key in snake case, such as
/// `{"harness": {"type": "ready"}}` or `{"warning": "..."}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// A normalized event from the harness driver.
    Harness(Event),
    /// A prompt submitted as a turn.
    Prompt(String),
    /// The answer to the permission request just before it.
    Decision {
        tool: String,
        allowed: bool,
        /// The denial message sent to the harness.
        message: Option<String>,
        source: DecisionSource,
    },
    /// A new candidate commit.
    Snapshot(CandidateInfo),
    /// The branch's status changed.
    Status(BranchStatus),
    /// Something the engine noticed, such as descendants that outlived the
    /// harness.
    Warning(String),
    /// A delegation operation this branch asked for, recorded on the
    /// asking branch whether it was carried out or refused.
    Delegation {
        /// The tool: `spawn`, `send`, `propose_integration` or `cancel`.
        tool: String,
        /// The branch it acted on; for a refused spawn, the requested name
        /// or empty.
        branch: String,
        /// What happened, or why it was refused.
        outcome: String,
        refused: bool,
    },
    /// The harness's home and environment were prepared before it started;
    /// see [`TaskOptions::provision`]. Names only, never a secret's value.
    Provisioned {
        /// The authentication method the secrets chose, if any.
        auth: Option<String>,
        /// Files written or removed in the harness's home, relative to it.
        files: Vec<String>,
        /// Variables set for the harness.
        env: Vec<String>,
        /// How each secret the harness reads reaches it, and whether it is
        /// in the environment of the harness's tool commands.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        secrets: Vec<Delivery>,
        /// Secrets given that this harness does not read.
        unused_secrets: Vec<String>,
        /// Connectors granted for the turn, whose packages and index were
        /// placed in the home and whose gateway token was written there
        /// (never shown); see `docs/connectors.md`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        connectors: Vec<String>,
        /// The adopted knowledge entries given to the harness in its
        /// instructions this turn, most specific first; see
        /// `docs/knowledge.md`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        knowledge: Vec<u64>,
    },
    /// Input from `by` was written into the running turn; see
    /// [`Branch::steer`]. The harness's `steer_accepted` or
    /// `steer_rejected` event follows.
    Steered {
        /// The steer's ID, as [`Steer::id`].
        id: u64,
        by: String,
        text: String,
    },
    /// Recovery took over a turn whose engine stopped; see
    /// [`Yard::recover`].
    Recovered {
        /// What was known about the turn, and what recovery concluded.
        reason: String,
        /// Harness processes that were still running and were killed.
        killed: Vec<u32>,
    },
    /// No harness activity for the turn's [`Budget::stall_after`] window;
    /// recorded once per stall. See `docs/lifecycle.md`.
    Stalled {
        /// Milliseconds since the Unix epoch when activity was last seen.
        since_ms: u64,
    },
    /// Activity was observed after [`Activity::Stalled`]; the branch is no
    /// longer stalled.
    Resumed,
    /// A harness-to-harness message was sent or delivered; see
    /// [`crate::inbox`]. Recorded on both the sending and the receiving
    /// branch's event log.
    Message(Message),
    /// Inbox messages reached this branch's turn, and by which path; see
    /// `docs/delegation.md#delivery`. Each message is delivered once.
    MessagesDelivered { ids: Vec<u64>, via: DeliveredVia },
    /// A phase of the branch's workspace lifecycle ran: files copied into
    /// its new worktree, setup before its first turn, or teardown at its
    /// removal. See [`TaskOptions::workspace`].
    Workspace(WorkspaceReport),
    /// A turn ended and its worktree was recorded as a checkpoint ref; see
    /// `docs/checkpoints.md`.
    Checkpoint(Checkpoint),
    /// The branch was rewound to an earlier (or, after a rewind, a later)
    /// checkpoint by [`Branch::rewind`].
    Rewound {
        /// The checkpoint the branch was at, when known.
        from: Option<u32>,
        to: u32,
        /// The commit the branch and its worktree were reset to.
        commit: String,
        /// How the branch's next turn continues the conversation.
        session: SessionContinuity,
    },
    /// This branch was forked from another's checkpoint by
    /// [`Branch::fork_at`]; recorded on the new branch before its turn.
    ForkedAt {
        branch: String,
        turn: u32,
        commit: String,
        session: SessionContinuity,
    },
    /// A step towards a pull request: an issue linked, a check run, a push,
    /// a pull request opened or observed, feedback delivered. See
    /// [`PullRequestActivity`] and `docs/pull-requests.md`.
    PullRequest(Box<PullRequestActivity>),
    /// Where a turn's sandbox came from (fresh, resumed, branched from a
    /// provider snapshot) and what became of it: kept, destroyed, evicted,
    /// a snapshot released. See `docs/sandbox-snapshots.md`.
    Sandbox(Box<SandboxEvent>),
    /// Routing, failover and judging: the router's choice for this branch,
    /// its harness failing over to the next candidate, a judge's scratch
    /// branch, or a judge's score. See [`FleetActivity`] and
    /// `docs/fleet.md`.
    Fleet(Box<FleetActivity>),
    /// A call the harness made through the connector gateway, from the
    /// gateway's audit log: allowed, denied, or refused for want of
    /// confirmation. See `docs/connectors.md`.
    ConnectorCall(Box<connectors::ConnectorCall>),
    /// The turn's egress policy and how it was applied, and each
    /// destination the egress proxy allowed or denied. See
    /// [`EgressActivity`] and `docs/egress.md`.
    Egress(Box<EgressActivity>),
    /// The branch was made from a harness session that already existed on
    /// this machine (`by adopt`); its next turn resumes that session.
    Adopted(Box<Adoption>),
    /// Repository knowledge: the branch was distilled into proposed
    /// entries. See [`KnowledgeActivity`] and `docs/knowledge.md`.
    Knowledge(Box<KnowledgeActivity>),
    /// Plan approval: planning, a proposed plan, its approval, rejection or
    /// escalation. See [`PlanActivity`] and `docs/plans-and-goals.md`.
    Plan(Box<PlanActivity>),
    /// A goal and its judge's verdicts. See [`GoalActivity`].
    Goal(Box<GoalActivity>),
}

/// A turn's checkpoint: the branch's commit when the turn ended, kept as the
/// ref `refs/branchyard/<branch>/<incarnation>/turn-<N>`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// The turn's number, from 1; turn 0 is the branch's base and has no ref.
    pub turn: u32,
    pub commit: String,
    pub git_ref: String,
    /// The checkpoint the turn started from: `Some(0)` for the base, the
    /// rewound-to checkpoint after a rewind, `None` when not recorded.
    pub after: Option<u32>,
    /// The harness session when the turn ended.
    pub session: Option<String>,
    /// Against the branch's base.
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
    /// The provider snapshot taken with this checkpoint, when the branch
    /// kept its sandbox and the provider could; see
    /// `docs/sandbox-snapshots.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<Box<SandboxSnapshot>>,
}

/// How a rewound or forked-at branch's conversation continues. Serialized as
/// an object tagged by `mode`, such as `{"mode": "native", "session": "..."}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SessionContinuity {
    /// The harness's own session is resumed (a rewind) or forked (a fork):
    /// it ended exactly at that checkpoint.
    Native { session: String },
    /// A fresh session, whose first prompt carries a generated summary of
    /// these turns, because the native session could not continue from the
    /// checkpoint, for `reason`.
    Summary { turns: Vec<u32>, reason: String },
    /// A fresh session with no summary: nothing ran before the checkpoint
    /// (turn 0, the base).
    Fresh { reason: String },
}

impl SessionContinuity {
    /// One line for people: what the next turn continues.
    pub fn describe(&self) -> String {
        match self {
            SessionContinuity::Native { session } => {
                format!("continues the harness's own session {session}")
            }
            SessionContinuity::Summary { turns, reason } => {
                let turns = match (turns.first(), turns.last()) {
                    (Some(first), Some(last)) if first != last => {
                        format!("turns {}", checkpoint::turn_list(turns))
                    }
                    (Some(only), _) => format!("turn {only}"),
                    _ => "no earlier turns".to_owned(),
                };
                format!("starts a fresh session with a summary of {turns}: {reason}")
            }
            SessionContinuity::Fresh { reason } => {
                format!("starts a fresh session: {reason}")
            }
        }
    }

    /// Whether the harness's own session continues.
    pub fn native(&self) -> bool {
        matches!(self, SessionContinuity::Native { .. })
    }
}

/// How [`Activity::MessagesDelivered`] messages reached a turn. Serialized
/// as an object tagged by `path`, such as
/// `{"path": "steer", "steer": 3, "boundary": "codex_turn_steer"}`.
///
/// `boundary` names the protocol boundary the message actually landed at,
/// per harness (Straitjacket's relay names an equivalent boundary for its
/// own capsules; see `docs/comparison.md`): `"turn_start"` for every
/// profile's turn start, or a driver-specific name such as
/// `"claude_next_model_call"`, `"codex_turn_steer"`, `"pi_steer"` or
/// `"acp_session_steering"` for a steer, from
/// [`branchyard_harness::Driver::steer_boundary`]. Old rows recorded before
/// this field existed deserialize with `"not_recorded"`, never a guess.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "path", rename_all = "snake_case")]
pub enum DeliveredVia {
    /// Prepended to the prompt at the start of the turn.
    TurnStart {
        #[serde(default = "boundary_turn_start")]
        boundary: String,
    },
    /// Steered into the running turn as input `steer` ([`Steer::id`]).
    Steer {
        steer: u64,
        #[serde(default = "boundary_not_recorded")]
        boundary: String,
    },
}

fn boundary_turn_start() -> String {
    "turn_start".to_owned()
}

fn boundary_not_recorded() -> String {
    "not_recorded".to_owned()
}

/// Input for a branch's running turn, and what became of it; see
/// [`Branch::steer`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Steer {
    /// Unique in the repository's store.
    pub id: u64,
    pub branch: String,
    /// Who sent it, as the branch's event log names them.
    pub by: String,
    pub text: String,
    /// Milliseconds since the Unix epoch.
    pub requested_at_ms: u64,
    pub state: SteerState,
}

/// Where a [`Steer`] is. Serialized as an object tagged by `state`, such as
/// `{"state": "refused", "reason": "..."}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SteerState {
    /// Queued for the running turn; the engine running it writes it to the
    /// harness within about 100 ms, in whichever process it runs.
    Pending,
    /// Written to the harness, which has not yet confirmed it.
    Delivered,
    /// The harness took it into the running turn.
    Accepted,
    /// Never reached the model: the harness refused or dropped it, an
    /// interrupt cancelled it, or the turn ended first.
    Refused { reason: String },
}

/// What a message means, and so who it may go to: a `question` and a
/// `report` go to the sender's parent; an `escalation` goes to the parent
/// too, or further up an ancestor its rig seat's `escalates_to` names; an
/// `answer` goes from a branch to one of its own descendants, and normally
/// carries `in_reply_to` a question's id.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Question,
    Report,
    Escalation,
    Answer,
}

impl MessageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageKind::Question => "question",
            MessageKind::Report => "report",
            MessageKind::Escalation => "escalation",
            MessageKind::Answer => "answer",
        }
    }
}

impl fmt::Display for MessageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for MessageKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Error> {
        match value {
            "question" => Ok(MessageKind::Question),
            "report" => Ok(MessageKind::Report),
            "escalation" => Ok(MessageKind::Escalation),
            "answer" => Ok(MessageKind::Answer),
            other => Err(Error::State(format!("unknown message kind {other:?}"))),
        }
    }
}

/// One harness-to-harness message, durable in the store; see
/// `docs/delegation.md#inbox`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Assigned by the store when it is sent; counts from 1 across the
    /// whole repository.
    pub id: u64,
    pub from: String,
    pub to: String,
    pub kind: MessageKind,
    pub text: String,
    /// The question this answers, for an `answer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<u64>,
    /// Milliseconds since the Unix epoch, when it was sent.
    pub at_ms: u64,
    /// Whether it has been delivered to its recipient's turn (at the start
    /// of one, or through a running turn's delivery hook). `inbox --unread`
    /// is the messages for which this is `false`.
    #[serde(default)]
    pub delivered: bool,
}

/// Activity from a named branch.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchEvent {
    pub branch: String,
    pub activity: Activity,
}

/// Activity as recorded in `.branchyard/`, with its observation time.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedEvent {
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub activity: Activity,
}

/// A page of one branch's events; see [`Branch::events_since`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub events: Vec<RecordedEvent>,
    /// The number of the last event returned, or the cursor asked for when
    /// none were.
    pub next_cursor: u64,
}

/// A page of the repository's feed; see [`Yard::events_since`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeedPage {
    pub events: Vec<FeedEvent>,
    /// The position of the last event returned, or the cursor asked for
    /// when none were.
    pub next_cursor: u64,
}

/// One event in the repository's feed.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeedEvent {
    /// Position in the feed, from 1.
    pub position: u64,
    pub branch: String,
    pub event: RecordedEvent,
}

/// A branch [`Yard::recover`] took over.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recovery {
    pub branch: String,
    /// The status recovery settled on.
    pub status: BranchStatus,
    pub reason: String,
    /// Harness processes that were killed.
    pub killed: Vec<u32>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Merged {
    pub branch: String,
    pub target: String,
    pub previous: String,
    pub commit: String,
}

/// Known harness profiles, whether their executable is on `PATH`, and
/// their live qualification status, without opening a repository (what
/// [`Yard::harnesses`] returns).
pub fn harnesses() -> Vec<HarnessInfo> {
    harness::list()
}

/// Known harness profile and local availability.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInfo {
    pub harness: String,
    pub profile: String,
    pub default: bool,
    /// The executable was found on `PATH`.
    pub available: bool,
    /// Live qualification summary from `docs/qualification/`, such as
    /// "9/9 on 0.81.2".
    pub qualification: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    /// Not inside a git work tree.
    NotARepository(PathBuf),
    UnknownBranch(String),
    BranchExists(String),
    /// No message with this id in the branch's inbox.
    UnknownMessage(u64),
    /// No knowledge entry with this id.
    UnknownKnowledge(u64),
    /// The branch has no plan, or none awaiting approval.
    NoPlan(String),
    /// Not a usable branch name: lowercase `[a-z0-9._-]`, starting with a
    /// letter or digit, one path segment.
    InvalidName {
        name: String,
        reason: String,
    },
    UnknownHarness(String),
    /// The harness executable could not be found or started.
    HarnessUnavailable {
        harness: String,
        reason: String,
    },
    /// The requested operation needs a capability the harness lacks.
    Unsupported(String),
    /// The branch has no candidate to merge or fork.
    NoCandidate(String),
    /// `actual` is `None` when the target branch no longer exists.
    TargetMoved {
        expected: String,
        actual: Option<String>,
    },
    Conflict {
        files: Vec<String>,
    },
    CheckFailed {
        output_tail: String,
    },
    CheckTimedOut {
        timeout: Duration,
        output_tail: String,
    },
    /// The check command could not be started.
    CheckNotStarted(String),
    /// A worktree with the target checked out has uncommitted changes.
    DirtyTarget(PathBuf),
    /// The candidate is already contained in the target.
    AlreadyMerged {
        target: String,
    },
    /// The recorded candidate is not a valid commit descending from its base.
    InvalidCandidate(String),
    /// Refused by a delegation envelope or authority check.
    Denied(String),
    /// The branch is running a turn, and the operation needs it idle.
    Running(String),
    /// The branch is not running a turn, and the operation needs one, such
    /// as [`Branch::steer`].
    NotRunning(String),
    /// This engine lost the branch's lease to another, which recovered or
    /// took over the branch; its writes are refused.
    Fenced(String),
    /// A graph proposal was made against a revision of the branch's graph
    /// that is no longer current; read the graph and propose again.
    StaleRevision {
        branch: String,
        expected: u64,
        actual: u64,
    },
    /// An error the engine running a delegating turn returned through its
    /// broker, with the [`Error::kind`] it had there.
    Remote {
        kind: String,
        message: String,
    },
    Git(String),
    Harness(String),
    Io(std::io::Error),
    State(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotARepository(path) => {
                write!(f, "{} is not inside a git work tree", path.display())
            }
            Error::UnknownBranch(name) => write!(f, "no branch named {name}"),
            Error::BranchExists(name) => write!(f, "branch {name} already exists"),
            Error::UnknownMessage(id) => write!(f, "no message #{id} in this inbox"),
            Error::UnknownKnowledge(id) => write!(f, "no knowledge entry #{id}"),
            Error::NoPlan(why) => write!(f, "no plan: {why}"),
            Error::InvalidName { name, reason } => {
                write!(f, "{name:?} is not a usable branch name: {reason}")
            }
            Error::UnknownHarness(name) => write!(f, "no harness or profile named {name}"),
            Error::HarnessUnavailable { harness, reason } => {
                write!(f, "{harness} is unavailable: {reason}")
            }
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::NoCandidate(name) => write!(f, "branch {name} has no candidate"),
            Error::TargetMoved {
                expected,
                actual: Some(actual),
            } => write!(
                f,
                "target moved from {expected} to {actual}; re-run the check"
            ),
            Error::TargetMoved {
                expected,
                actual: None,
            } => write!(f, "target at {expected} no longer exists"),
            Error::Conflict { files } => write!(f, "merge conflicts in {}", files.join(", ")),
            Error::CheckFailed { output_tail } => write!(f, "check failed:\n{output_tail}"),
            Error::CheckTimedOut {
                timeout,
                output_tail,
            } => write!(f, "check timed out after {timeout:?}:\n{output_tail}"),
            Error::CheckNotStarted(reason) => write!(f, "check could not start: {reason}"),
            Error::DirtyTarget(worktree) => write!(
                f,
                "the target is checked out with uncommitted changes in {}",
                worktree.display()
            ),
            Error::AlreadyMerged { target } => {
                write!(f, "the candidate is already contained in {target}")
            }
            Error::InvalidCandidate(message) => write!(f, "invalid candidate: {message}"),
            Error::Denied(why) => write!(f, "denied: {why}"),
            Error::Running(name) => write!(f, "branch {name} is running a turn"),
            Error::NotRunning(name) => write!(f, "branch {name} is not running a turn"),
            Error::Fenced(why) => write!(f, "fenced: {why}"),
            Error::StaleRevision {
                branch,
                expected,
                actual,
            } => write!(
                f,
                "stale graph revision: {branch}'s graph is at revision {actual}, not \
                 {expected}; read it again and propose against the current revision"
            ),
            Error::Remote { message, .. } => f.write_str(message),
            Error::Git(message) => write!(f, "git: {message}"),
            Error::Harness(message) => write!(f, "harness: {message}"),
            Error::Io(error) => write!(f, "{error}"),
            Error::State(message) => write!(f, "state: {message}"),
        }
    }
}

impl Error {
    /// A stable name for the error's variant, as `by --json` and the broker
    /// report it: `denied`, `running`, `unknown_branch`, `no_candidate`,
    /// `conflict`, `check_failed`, `target_moved`, `unsupported`, and so on.
    pub fn kind(&self) -> &str {
        match self {
            Error::NotARepository(_) => "not_a_repository",
            Error::UnknownBranch(_) => "unknown_branch",
            Error::BranchExists(_) => "branch_exists",
            Error::UnknownMessage(_) => "unknown_message",
            Error::UnknownKnowledge(_) => "unknown_knowledge",
            Error::NoPlan(_) => "no_plan",
            Error::InvalidName { .. } => "invalid_name",
            Error::UnknownHarness(_) => "unknown_harness",
            Error::HarnessUnavailable { .. } => "harness_unavailable",
            Error::Unsupported(_) => "unsupported",
            Error::NoCandidate(_) => "no_candidate",
            Error::TargetMoved { .. } => "target_moved",
            Error::Conflict { .. } => "conflict",
            Error::CheckFailed { .. } => "check_failed",
            Error::CheckTimedOut { .. } => "check_timed_out",
            Error::CheckNotStarted(_) => "check_not_started",
            Error::DirtyTarget(_) => "dirty_target",
            Error::AlreadyMerged { .. } => "already_merged",
            Error::InvalidCandidate(_) => "invalid_candidate",
            Error::Denied(_) => "denied",
            Error::Running(_) => "running",
            Error::NotRunning(_) => "not_running",
            Error::Fenced(_) => "fenced",
            Error::StaleRevision { .. } => "stale_revision",
            Error::Remote { kind, .. } => kind,
            Error::Git(_) => "git",
            Error::Harness(_) => "harness",
            Error::Io(_) => "io",
            Error::State(_) => "state",
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}
