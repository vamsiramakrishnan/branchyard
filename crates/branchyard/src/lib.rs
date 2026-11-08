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

mod access;
mod adopt;
mod assignment;
mod broker;
mod bundle;
pub mod bundles;
mod capabilities;
mod capacity;
mod changes;
mod checkpoint;
mod compare;
pub mod configure;
#[cfg(test)]
mod conformance;
pub mod connectors;
pub mod contract;
mod delegation;
pub mod doctor;
mod drift;
pub mod effects;
mod egress;
mod endpoint;
mod engine;
mod environments;
mod feedback;
mod fleet;
mod git;
pub mod glance;
mod goal;
mod graph;
pub mod guard;
pub mod handoff;
mod harness;
pub mod images;
mod inbox;
mod integrate;
pub mod inventory;
mod json_schema;
mod judge;
mod knowledge;
mod land;
mod lifecycle;
pub mod live_trace;
mod liveness;
mod lock;
pub mod manifest;
mod map;
mod map_input;
pub mod memory;
pub mod messaging;
pub mod models;
mod names;
mod native_options;
mod obligations;
pub mod operations;
mod ops;
#[cfg(feature = "postgres")]
mod pg;
mod placement;
mod plan;
mod policy;
mod pool;
mod proc;
mod projection;
mod provider_admission;
mod providers;
mod provisioning;
mod pull_request;
pub mod qualified;
mod record;
mod recover;
pub mod registry;
pub mod reported;
mod review;
mod run;
#[cfg(test)]
mod schema_parity;
pub mod scope;
mod seats;
pub mod services;
pub mod skill_activity;
pub mod skills;
mod snapshots;
mod spotlight;
mod sqlite;
pub mod stage;
pub mod startup;
mod state;
mod steer;
mod storage;
#[doc(hidden)]
pub mod store_codec;
mod store_trace;
mod tarball;
pub mod tasks;
mod timeline;
mod timers;
pub mod toolstore;
mod tooltree;
pub mod trace_export;
pub mod up;
mod wake;
mod workspace;
pub mod workspace_runtime;

use branchyard_support::best_effort;
use branchyard_support::LockExt as _;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The configuration file, `branchyard.toml` and the user file: its types,
/// strict parser, layering and loader. See `docs/setup.md#configuration`.
pub use branchyard_config as config;
/// Whether this build includes the Microsandbox SDK, and the command that
/// builds one that does.
pub use branchyard_microsandbox::{ENABLED as MICROSANDBOX_BUILT, REBUILD as MICROSANDBOX_REBUILD};
pub use git::current_branch;
pub use git::hold_writes as hold_git_writes;
pub use guard::Guard;
pub use lock::DirLock;
pub use native_options::{
    AgentSandboxNetwork, AgentSandboxOptions, AgentSandboxRuntime, ContainerNetwork,
    ContainerOptions, ContainerRuntime,
};
#[cfg(feature = "postgres")]
pub use pg::{
    postgres_config, postgres_open, set_connection_policy, ConnectionPolicy, PasswordSource,
};
pub use placement::{HOME as SANDBOX_HOME, WORKSPACE as SANDBOX_WORKSPACE};
pub use provider_admission::ProviderAdmission;
pub use providers::{HarnessLocation, Isolation, ProviderCompute, RunsOn};
pub use qualified::{qualified_pin, QualifiedPin};

pub use access::{AccessActivity, Ceiling, DelegationScope, NetworkScope};
pub use adopt::{AdoptSpec, Adoption};
pub use branchyard_harness::{
    Event, HarnessTask, NativeSession, PermissionDecision, PermissionKey, PermissionRequest,
    TargetKind, ToolTarget, TurnOutcome, Usage,
};
pub use branchyard_provision::network::{
    narrow as network_narrow, Enforce as NetworkEnforce, HostRule as NetworkRule, Network,
};
pub use branchyard_provision::{
    built_in, logins, Delivery, Effort, McpServerSpec, Provisioning, RemoteMcpSpec,
    RemoteMcpTransport, SecretFrom, SecretSource, SessionPatch, Telemetry, Via,
};
use branchyard_workspace::Repository;
pub use bundle::BundleEntry;
pub use capacity::AUTO as AUTO_HARNESS;
pub use changes::{
    check_review, review_prompt, ChangedFile, Changes, DiffLine, DiffSide, FileStatus, Hunk,
    LineKind, Review, ReviewComment, Reviews, SentComment, MAX_ADDRESSED_REVIEWS, MAX_DIFF_BYTES,
    MAX_FILES, MAX_FILE_LINES, MAX_LINE_CHARS, MAX_REVIEW_COMMENTS, MAX_REVIEW_TEXT,
    MAX_TOTAL_LINES,
};
pub use checkpoint::{
    entries as checkpoint_entries, recorded as recorded_checkpoints, CheckpointEntry, Checkpoints,
    Rewound,
};
pub use compare::{attempt as compare_attempt, diff_files, mark_unique, Attempt, AttemptCheck};
pub use contract::{ResultCapture, ResultContract, ResultPurpose};
pub use delegation::{
    nest_graph_budgets, nest_plan_budgets, Asked, Brief, Cancelled, ChildBudget, Children,
    Delegate, Delegated, DelegationPlan, Envelope, EventPage, Inbox, Inspection, Sent, Spawn,
    Spawned, Waited, BRIEF_ENTRY_MAX, BRIEF_MAX, DEFAULT_MAX_WAKES,
};
pub use drift::Drift;
pub use egress::{EgressActivity, Enforcement as EgressEnforcement};
pub use endpoint::SandboxHost;
pub use environments::{
    EnvironmentBuild, EnvironmentInfo, EnvironmentInput, EnvironmentOrigin, EnvironmentSnapshot,
    EnvironmentState, EnvironmentUse, Pruned as EnvironmentsPruned,
    DEFAULT_INPUTS as ENVIRONMENT_DEFAULT_INPUTS, DEFAULT_KEEP as ENVIRONMENT_DEFAULT_KEEP,
    DEFAULT_MAX_AGE as ENVIRONMENT_DEFAULT_MAX_AGE,
};
pub use feedback::{prompt_digest, Feedback, FeedbackItem, FeedbackKind, FeedbackSource};
pub use fleet::{
    classify, credit, entry_label as fleet_entry_label, fresh_seed as fleet_seed, harness_fault,
    inventory_default, pooled as fleet_pooled, recorded_choice, recorded_route, risk_words,
    stats as fleet_stats, Advice, AdviceScope, Advised, BranchOutcome, CandidateStats,
    Classification, Excluded, Fleet, FleetActivity, FleetCandidate, FleetEntry, InventoryDefault,
    JudgeMark, JudgeSpec, OutcomeRecord, ParentChoice, Route, RouteDecision, RouteOptions,
    RoutePick, Routed, TaskKind, DECIDED_BY_PARENT, DECIDED_BY_ROUTER, DEFAULT_EXPLORATION,
    DEFAULT_STALL, ENVELOPE_ENTRY, INVENTORY_ENTRY, RISK_WORDS,
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
pub use handoff::{Handoff, HandoffRequest, SourceVersion};
pub use inbox::{DeliveryHook, SteerDelivery, PERSON};
pub use json_schema::JsonSchema;
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
pub use land::{Landing, FIXES as LAND_FIXES};
pub use lifecycle::{Parked, Resting};
pub use liveness::{Liveness, LivenessState};
pub use map::{
    check_spec as check_map_spec, default_name as map_default_name,
    follow_up_prompt as map_follow_up_prompt, item_prompt as map_item_prompt,
    reduce_prompt as map_reduce_prompt, rows_csv as map_rows_csv, rows_jsonl as map_rows_jsonl,
    MapOptions, MapProgress, MapReduce, MapReport, MapRow, MapSpec, MapStatus, MapSummary,
    ProgressFn as MapProgressFn, DEFAULT_CONCURRENCY as MAP_DEFAULT_CONCURRENCY,
    DEFAULT_RETRIES as MAP_DEFAULT_RETRIES, MAX_CONCURRENCY as MAP_MAX_CONCURRENCY,
};
pub use map_input::{
    check_template as check_map_template, item_id as map_item_id, parse_answer as parse_map_answer,
    parse_items as parse_map_items, render as render_map_prompt, ItemFormat, MapItem,
    TemplateContext,
};
pub use memory::review::{Approved as MemoryApproved, MemoryRecordEdit};
pub use memory::{
    Consolidated, Memory, MemoryActivity, MemoryDraft, MemoryEdit, MemoryFilter, MemoryKind,
    MemoryRecord, MemorySettings, MemoryStatus, NewMemory, NewObservation, Observation,
    ObservationKind, RunRow as MemoryRun, Scope as MemoryScope, BODY_MAX as MEMORY_BODY_MAX,
    TITLE_MAX as MEMORY_TITLE_MAX,
};
pub use obligations::{Obligation, ObligationKind};
pub use plan::{
    approved_prompt, from_events as plan_from_events, parse_tasks as parse_plan_tasks,
    planning_prompt, read_only as read_only_policy, Plan, PlanActivity, PlanInfo, PlanPhase,
    PlanTask, READ_ONLY_TOOLS,
};
pub use policy::{PolicyPreset, PresetRules, EDIT_TOOLS, SHELL_TOOLS, WEB_TOOLS};
pub use pool::{
    recipe as pool_recipe, PoolDrained, PoolFill, PoolKeeper, PoolSlot, PoolSlotState, PoolSpec,
    PoolStatus, PoolUse, DEFAULT_MAX_AGE as POOL_DEFAULT_MAX_AGE,
    DEFAULT_MAX_BEHIND as POOL_DEFAULT_MAX_BEHIND, KEEP_EVERY as POOL_KEEP_EVERY,
    MAX_SIZE as POOL_MAX_SIZE,
};
pub use projection::{ENV_BRANCH, ENV_BY, ENV_ROOT, ENV_TOKEN};
pub use pull_request::{
    slug, CheckRun, CiSummary, IssueLink, PullRequestActivity, PullRequestObservation,
    PullRequestRef, Pushed, ResolvedThread, Restack, Restacked, StackLink,
};
pub use reported::{Reported, ReportedState, StatusReport};
pub use review::ReviewSpec;
pub use seats::{Seat, Seats};
use serde::{Deserialize, Serialize};
pub use skill_activity::SkillActivity;
pub use skills::{BranchSkills, SkillGate, SkillSet, WithheldSkills};
pub use snapshots::{
    Placed, SandboxConsistency, SandboxEvent, SandboxKeep, SandboxOrigin, SandboxScope,
    SandboxSnapshot, SnapshotMethod, Transfer,
};
pub use spotlight::{TryEntry, TryFile, TryState};
use std::collections::{BTreeMap, HashMap};
pub use storage::{ArtifactRef, ScratchArea, ScratchLock, DEFAULT_ARTIFACT_LIMIT};
pub use timeline::{MarkKind, Timeline, TimelineMark, TimelineTool, TimelineTurn};
pub use timers::Timer;
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

    /// Block until `branches` have settled, any one of them with `any`,
    /// else all, or until `timeout` passes, whichever process runs them:
    /// the wait reads their durable status and wakes when it changes
    /// ([`Delegate::wait_for`] is the same wait acting as a branch). With
    /// your authority: any branch may be waited for.
    pub fn wait_for(
        &self,
        branches: &[&str],
        any: bool,
        timeout: Option<Duration>,
    ) -> Result<Waited, Error> {
        self.wait_for_queued(branches, any, timeout, |_| Ok(false))
    }

    /// [`Yard::wait_for`], also waiting for a branch while `queued` says a
    /// turn was asked of it that has not started yet, such as a server's
    /// admitted send: its status is still the last turn's, and a wait
    /// called after the send waits for the turn the send asked for.
    pub fn wait_for_queued(
        &self,
        branches: &[&str],
        any: bool,
        timeout: Option<Duration>,
        queued: impl Fn(&str) -> Result<bool, Error>,
    ) -> Result<Waited, Error> {
        let names: Vec<String> = branches.iter().map(|b| (*b).to_owned()).collect();
        delegation::wait_for(
            self,
            &names,
            any,
            timeout,
            |name| delegation::trusted(self, name, TaskOptions::default())?.inspect(name),
            queued,
        )
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

    /// Set a settled branch aside with your authority (`by discard`): it
    /// ends [`BranchStatus::Discarded`] with `reason`, keeps its record,
    /// worktree and cost, runs no more turns, is never merged, and no
    /// longer counts against its parent's `max_children`. What waits for
    /// it is blocked. Refused while a turn runs ([`Error::Running`]: cancel
    /// it first), while it waits on its children, and once merged; a discarded branch stays as it is.
    pub fn discard(&self, branch: &str, reason: &str) -> Result<BranchInfo, Error> {
        ops::discard(self, branch, reason)
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
        *self.hub.connectors.lock_recovering("connectors") = Some(Arc::new(gateway));
    }

    /// Give this yard's branches the shared tool store `store`: a turn on a
    /// provider that sees the store at its path links the skills and
    /// connector packages the store has instead of copying them into its
    /// home; see `docs/tools-model.md`. Replaces any store set before.
    /// Shared by every clone of this `Yard`.
    pub fn use_tool_store(&self, store: toolstore::ToolStore) {
        *self.hub.tool_store.lock_recovering("tool store") = Some(Arc::new(store));
    }

    /// Give this yard's restricted network policies the package mirror hosts
    /// `mirrors` (`[network] mirrors`): every turn under a restricted policy
    /// may reach them, beside the connector gateway and the model gateway it is
    /// given; see `docs/egress.md`. Replaces any set before. Shared by every
    /// clone of this `Yard`.
    pub fn use_mirrors(&self, mirrors: Vec<branchyard_provision::network::HostRule>) {
        *self.hub.mirrors.lock_recovering("mirrors") = mirrors;
    }

    /// The mirror hosts set with [`Yard::use_mirrors`].
    pub fn mirrors(&self) -> Vec<branchyard_provision::network::HostRule> {
        self.hub.mirrors.lock_recovering("mirrors").clone()
    }

    /// Read sandbox images' labels from `source` when a turn's manifest
    /// records its image (docs/images.md). Without one, the labels
    /// `by images build` recorded in `.branchyard/images.json`. Replaces any
    /// set before. Shared by every clone of this `Yard`.
    pub fn use_image_labels(&self, source: Arc<dyn images::ImageLabelSource>) {
        *self.hub.image_labels.lock_recovering("image labels") = Some(source);
    }

    /// The label source set with [`Yard::use_image_labels`], or the
    /// repository's recorded images.
    pub fn image_labels(&self) -> Arc<dyn images::ImageLabelSource> {
        match self
            .hub
            .image_labels
            .lock_recovering("image labels")
            .clone()
        {
            Some(source) => source,
            None => Arc::new(images::RecordedLabels::new(self.root())),
        }
    }

    /// The store set with [`Yard::use_tool_store`], if any.
    pub fn tool_store(&self) -> Option<Arc<toolstore::ToolStore>> {
        self.hub.tool_store.lock_recovering("tool store").clone()
    }

    /// Give this yard's branches the model gateway `gateway`: a branch on
    /// it (`Provisioning::models`) gets a gateway of its own for each turn;
    /// see `docs/model-gateway.md`. Replaces any set before. Shared by
    /// every clone of this `Yard`.
    pub fn use_models(&self, gateway: models::Gateway) {
        *self.hub.models.lock_recovering("models") = Some(Arc::new(gateway));
    }

    /// The model gateway set with [`Yard::use_models`], if any.
    pub fn models(&self) -> Option<Arc<models::Gateway>> {
        self.hub.models.lock_recovering("models").clone()
    }

    /// Calls through the model gateway at or after `since_ms` (since the
    /// Unix epoch), oldest first, from every branch, removed ones too.
    pub fn model_usage(&self, since_ms: u64) -> Result<Vec<models::UsageRecord>, Error> {
        self.store().usage().usage_since(since_ms)
    }

    /// Cap what every branch acting for each person may reach: the
    /// connectors, models and network of a turn are its request's within
    /// its person's [`Ceiling`] (by subject: `local:<user>` locally, a
    /// server's principal). Replaces any set before. See
    /// `docs/model-gateway.md#one-scope`.
    pub fn use_ceilings(&self, ceilings: std::collections::BTreeMap<String, Ceiling>) {
        *self.hub.ceilings.lock_recovering("ceilings") = Arc::new(ceilings);
    }

    /// The ceiling of `subject`, if one is set.
    pub fn ceiling(&self, subject: &str) -> Option<Ceiling> {
        self.hub
            .ceilings
            .lock_recovering("ceilings")
            .get(subject)
            .cloned()
    }

    /// The connector gateway set with [`Yard::use_connectors`], if any.
    pub fn connectors(&self) -> Option<Arc<connectors::Gateway>> {
        self.hub.connectors.lock_recovering("connectors").clone()
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
        *self.hub.delivery_hook.lock_recovering("delivery_hook") = Some(hook);
    }

    /// Stop trying a hook set with [`Yard::set_delivery_hook`].
    pub fn clear_delivery_hook(&self) {
        *self.hub.delivery_hook.lock_recovering("delivery_hook") = None;
    }

    /// Register a kind of message branches may send with `message`
    /// ([`messaging::KindSpec`]: a name, a class, a payload schema). A
    /// built-in name or one registered already is refused. The registry is
    /// this process's: a `by` outside a harness or another server knows
    /// only the built-in kinds unless it registers the same. See
    /// `docs/messaging.md`.
    pub fn register_kind(&self, spec: messaging::KindSpec) -> Result<(), Error> {
        self.hub
            .messaging
            .lock_recovering("messaging")
            .registry
            .register(spec)
    }

    /// Every kind of message this yard knows, by name.
    pub fn message_kinds(&self) -> Vec<messaging::KindSpec> {
        self.hub
            .messaging
            .lock_recovering("messaging")
            .registry
            .kinds()
    }

    /// Replace the stages every message goes through
    /// ([`messaging::Pipeline`]): the standard pipeline with a stage of
    /// yours before or after one of its own.
    pub fn set_messaging_pipeline(&self, pipeline: messaging::Pipeline) {
        self.hub.messaging.lock_recovering("messaging").pipeline = pipeline;
    }

    /// The registry and pipeline as they are now.
    pub(crate) fn messaging(&self) -> messaging::Messaging {
        self.hub.messaging.lock_recovering("messaging").clone()
    }

    pub(crate) fn delivery_hook(&self) -> Option<Arc<dyn DeliveryHook>> {
        self.hub
            .delivery_hook
            .lock_recovering("delivery_hook")
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

    /// This repository's service registry ([`services::LocalRegistry`]):
    /// `.branchyard/registry.db`, or the file `BRANCHYARD_REGISTRY` names,
    /// shared by every process on this machine; opened once per yard and
    /// its clones. See `docs/registry.md`.
    pub fn services(&self) -> Result<Arc<services::LocalRegistry>, Error> {
        let mut held = self.hub.services.lock_recovering("services");
        if let Some(registry) = &*held {
            return Ok(registry.clone());
        }
        let registry = Arc::new(services::LocalRegistry::open(services::local_path(
            &self.root,
        ))?);
        *held = Some(registry.clone());
        Ok(registry)
    }

    /// Whether this repository has a registry file yet: what lets a
    /// reader skip opening one that would be empty.
    pub fn has_services(&self) -> bool {
        self.hub.services.lock_recovering("services").is_some()
            || services::local_path(&self.root).is_file()
    }

    /// Register `service` in this repository's registry for `ttl`, renewed
    /// from a thread of its own until the returned handle is dropped,
    /// which deregisters it.
    pub fn register_service(
        &self,
        service: services::Service,
        ttl: Duration,
    ) -> Result<services::Registration, Error> {
        Ok(services::Registration::start(
            self.services()?,
            service,
            ttl,
            services::Clock::system(),
        )?)
    }

    /// The live service [`services::resolve`] picks for `query` in this
    /// repository's registry, if any; none without a registry file.
    pub fn resolve_service(
        &self,
        query: &services::Query,
    ) -> Result<Option<services::Service>, Error> {
        if !self.has_services() {
            return Ok(None);
        }
        Ok(services::resolve(
            &*self.services()?,
            query,
            branchyard_support::time::now_ms(),
        )?)
    }

    /// Expire every service in this repository's registry whose lease ran
    /// out or whose owner is gone, and reclaim what Branchyard started for
    /// it: a leaked process, a sandbox or recipe machine (through the
    /// branch's recovery), pool slots. [`Yard::recover`] does this too.
    pub fn reclaim_services(&self) -> Result<Vec<services::Reaped>, Error> {
        services::reclaim::sweep(self, branchyard_support::time::now_ms())
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

    /// Start describing a task. On a yard given a configuration
    /// ([`Yard::configure`], [`Yard::open_configured`]) the options start as
    /// its `[defaults]` ([`configure::task_options`]) and the run fills in
    /// its provisioning where the caller set nothing
    /// ([`configure::fill_provisioning`]); otherwise they start as
    /// [`TaskOptions::default`].
    pub fn task(&self, prompt: impl Into<String>) -> TaskBuilder {
        let (defaults, options) = match self.task_defaults() {
            Some((config, options)) => (Some(config), options),
            None => (None, TaskOptions::default()),
        };
        TaskBuilder {
            yard: self.clone(),
            prompt: prompt.into(),
            options,
            defaults,
        }
    }

    /// Every branch Branchyard knows, oldest first, each with what its
    /// running turn waits on a person for ([`BranchInfo::awaiting`]) and
    /// what it needs from a person ([`BranchInfo::stage`]) and whether its
    /// engine is still heard from ([`BranchInfo::liveness`]).
    pub fn branches(&self) -> Result<Vec<BranchInfo>, Error> {
        let parked = parked(self.store().effects().asks(true)?);
        let alive = self.liveness()?;
        let questions = self.store().backend().unanswered()?;
        let infos = self
            .store()
            .list()?
            .into_iter()
            .map(|record| with_liveness(with_awaiting(record.info, &parked), &alive))
            .collect();
        Ok(stage::staged(infos, &questions))
    }

    /// Look up one branch by name.
    pub fn branch(&self, name: &str) -> Result<Branch, Error> {
        let record = self.store().read(name)?;
        let alive = self.liveness()?;
        let info = with_liveness(
            with_awaiting(record.info, &parked(self.store().effects().asks(true)?)),
            &alive,
        );
        let questions = self.store().backend().unanswered()?;
        let parent = match info.parent.as_deref() {
            Some(parent) if info.depth > 0 => {
                self.store().backend().read(parent)?.map(|p| p.info.status)
            }
            _ => None,
        };
        let asking = stage::asking(&questions, |n| (n == info.name).then_some(&info.status));
        let info = stage::with_stage(info, asking.get(name), parent.as_ref());
        Ok(Branch {
            yard: self.clone(),
            info,
        })
    }

    /// What branches still owe, or what `branch` and the branches
    /// delegated below it owe: unknown and staged effects, closing undo
    /// windows, waiting approvals, lost turns and engines, blocked and
    /// waiting children, parked parents and ready children nobody
    /// integrated. Read from the ledger, the asks, the records and the
    /// leases; nothing is stored. See `docs/obligations.md`.
    pub fn obligations(&self, branch: Option<&str>) -> Result<Vec<Obligation>, Error> {
        self.obligations_at(branch, self.store().backend().clock_ms()?)
    }

    /// [`Yard::obligations`] as of `now_ms`, for an undo window's deadline
    /// and an engine's lease.
    pub fn obligations_at(
        &self,
        branch: Option<&str>,
        now_ms: u64,
    ) -> Result<Vec<Obligation>, Error> {
        obligations::list(&self.store(), branch, now_ms)
    }

    /// Branches whose record disagrees with their event log: the status
    /// the record holds against the last status the log recorded. Every
    /// write that changes a status appends its event in the same
    /// transaction, so this is empty unless a write path broke that rule.
    /// `branch` checks one; `None` checks every branch. See
    /// `docs/durability.md#the-record-and-the-log`.
    pub fn drift(&self, branch: Option<&str>) -> Result<Vec<Drift>, Error> {
        let store = self.store();
        let records = match branch {
            Some(name) => vec![store.read(name)?],
            None => store.list()?,
        };
        let mut drifted = Vec::new();
        for record in records {
            let events = record::read(&store, &record.info.name)?;
            drifted.extend(drift::check(&record, &events));
        }
        Ok(drifted)
    }

    /// The liveness of each branch holding a lease, by branch.
    fn liveness(&self) -> Result<HashMap<String, Liveness>, Error> {
        let store = self.store();
        let leases = store.backend().leases()?;
        Ok(liveness::by_branch(&leases, store.backend().clock_ms()?))
    }

    /// Per branch, the unanswered question or escalation its stage counts
    /// as asking ([`stage::Reason::Question`]): its oldest one, while it
    /// runs, or whatever its status when it asked the person. A branch that
    /// asks nothing now has no entry.
    pub fn asking(&self) -> Result<HashMap<String, Message>, Error> {
        let questions = self.store().backend().unanswered()?;
        let statuses: HashMap<String, BranchStatus> = self
            .store()
            .list()?
            .into_iter()
            .map(|record| (record.info.name, record.info.status))
            .collect();
        Ok(stage::asking(&questions, |name| statuses.get(name)))
    }

    /// The person's inbox: every message addressed to [`PERSON`], oldest
    /// first, with the ids of those answered in `answered`. A branch
    /// nobody delegated (a root, or a fork) sends its questions and
    /// escalations here; [`Yard::answer`] answers them.
    pub fn inbox(&self) -> Result<Inbox, Error> {
        let store = self.store();
        let messages = store.backend().inbox(PERSON)?;
        let mut answered = Vec::new();
        for message in &messages {
            if store.backend().answer_to(message.id)?.is_some() {
                answered.push(message.id);
            }
        }
        Ok(Inbox {
            branch: PERSON.to_owned(),
            messages,
            steered_this_turn: Vec::new(),
            answered,
        })
    }

    /// Answer the question or escalation `message_id` with `text`, as
    /// whoever it was sent to. One sent to [`PERSON`] is answered from the
    /// person: the answer goes to its sender, steered into the sender's
    /// running turn, or else given at the start of its next turn, which it
    /// does not start. One sent to a branch is answered as that branch
    /// ([`Delegate::answer`]). An unknown id is [`Error::UnknownMessage`];
    /// a message of another kind, one answered already, and empty text are
    /// refused.
    pub fn answer(&self, message_id: u64, text: &str) -> Result<Message, Error> {
        let message = self
            .store()
            .backend()
            .message(message_id)?
            .ok_or(Error::UnknownMessage(message_id))?;
        let asker = &message.from;
        if !matches!(
            message.kind,
            MessageKind::Question | MessageKind::Escalation
        ) {
            return Err(Error::Denied(format!(
                "message #{message_id} is a {}, and only a question or an escalation takes an \
                 answer; to tell {asker} something, send it a prompt",
                message.kind
            )));
        }
        if let Some(answer) = self.store().backend().answer_to(message_id)? {
            return Err(Error::Denied(format!(
                "message #{message_id} was answered already, by #{}; to tell {asker} more, send \
                 it a prompt",
                answer.id
            )));
        }
        if message.to == PERSON {
            return inbox::send(
                self,
                None,
                PERSON,
                MessageKind::Answer,
                asker,
                text,
                Some(message_id),
            );
        }
        self.branch(&message.to)?
            .delegate(TaskOptions::default())?
            .answer(message_id, text)
    }

    /// Merge `branch`'s candidate into the local branch `target`, running the
    /// branch's check. Refuses if `target` moved since the check started, if
    /// the check fails, or if the merge conflicts.
    pub fn merge(&self, branch: &str, target: &str) -> Result<Merged, Error> {
        let merged = ops::merge(self, branch, target)?;
        // The outcome store learns the merge; it never undoes one.
        best_effort(
            "record the fleet outcome",
            fleet::observe(self, branch, None),
        );
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
    /// keys. An error, with nothing removed, when the branches and pool
    /// slots that may link an environment cannot be read.
    pub fn prune_environments(
        &self,
        keep: usize,
        max_age: Duration,
        only: &[String],
    ) -> Result<environments::Pruned, Error> {
        environments::prune(self, keep, max_age, only)
    }

    /// `spec`'s warm pool on this host: its slots, oldest first. See
    /// `docs/pools.md`.
    pub fn pool_status(&self, spec: &WorkspaceSpec) -> Result<PoolStatus, Error> {
        pool::status(self, spec)
    }

    /// Every warm pool slot on this host, of any pool.
    pub fn pool_slots(&self) -> Result<Vec<PoolSlot>, Error> {
        pool::slots(self)
    }

    /// Discard `spec`'s stale slots and make new ones until its pool has
    /// `size` ready, after any other process filling it. A slot whose
    /// environment key has none built runs setup to build it: the caller
    /// decides whether `spec`'s scripts may run, as for
    /// [`TaskOptions::workspace`].
    pub fn fill_pool(&self, spec: &WorkspaceSpec) -> Result<PoolFill, Error> {
        pool::fill(self, spec)
    }

    /// Remove every ready slot on this host, and what stopped processes
    /// left.
    pub fn drain_pool(&self) -> Result<PoolDrained, Error> {
        pool::drain(self)
    }

    /// Keep the pool `spec` returns (read again each time) filled from a
    /// thread of its own: at once, whenever a branch in this process claims
    /// a slot or finds none, and otherwise every `every`. `on_fill` sees
    /// each fill. Stops when the keeper is dropped (without waiting for a
    /// fill under way) or stopped ([`PoolKeeper::stop`], which waits).
    pub fn keep_pool(
        &self,
        spec: impl Fn() -> Option<WorkspaceSpec> + Send + 'static,
        every: Duration,
        on_fill: impl Fn(&PoolFill) + Send + 'static,
    ) -> PoolKeeper {
        PoolKeeper::start(self.clone(), Box::new(spec), every, Box::new(on_fill))
    }

    /// The repository skills `branch` runs with, fixed when it (or the
    /// branch it was forked or delegated from) was created: the commit they
    /// are read from, their entries and digest, and whether they are staged
    /// or withheld and why. `None` when its base commit had none. See
    /// [`skills`].
    pub fn branch_skills(&self, branch: &str) -> Result<Option<BranchSkills>, Error> {
        Ok(self.store().read(branch)?.skills)
    }

    /// The variables `branch`'s scripts and harness get:
    /// `BRANCHYARD_BRANCH`, `BRANCHYARD_WORKTREE`, `BRANCHYARD_ROOT` and
    /// `BRANCHYARD_PORT`, reserving the branch's port now if it has none
    /// yet, with its `[workspace.env]` and what its `shell_setup` adds or
    /// changes (run now, in the worktree). For running a command in its
    /// worktree, as `by workspace run` does. [`Error::Config`] when
    /// `shell_setup` fails, saying why.
    pub fn workspace_env(&self, branch: &str) -> Result<Vec<(String, String)>, Error> {
        let store = self.store();
        let record = store.read(branch)?;
        let port = workspace::reserve_port(&store, &self.root, branch)?;
        let vars = workspace::variables(self, branch, &record.info.worktree, Some(port));
        match record.workspace.as_ref() {
            Some(state) => {
                workspace::script_env(&state.spec, &record.info.worktree, vars, &|| None)
                    .map_err(Error::Config)
            }
            None => Ok(vars),
        }
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

    /// Tell this yard (and its clones) where its sandboxes reach this
    /// host: a sandboxed turn that may delegate is served the delegation
    /// tools over HTTP there, with a token of its own, for as long as the
    /// turn runs. See `docs/delegation.md#sandboxed-harnesses`. Replaces
    /// any set before.
    pub fn use_sandbox_host(&self, host: SandboxHost) {
        *projection::lock(&self.hub.sandbox_host) = Some(host);
    }

    /// Where this yard's sandboxes reach this host: what
    /// [`Yard::use_sandbox_host`] set, else the connector gateway's
    /// effect-ledger proxy address ([`connectors::EffectsProxy`]) when it
    /// names a host for sandboxes; `None` when neither does.
    pub fn sandbox_host(&self) -> Option<SandboxHost> {
        if let Some(host) = projection::lock(&self.hub.sandbox_host).clone() {
            return Some(host);
        }
        let gateway = self.connectors()?;
        let host = gateway.effects.sandbox_host.clone()?;
        Some(SandboxHost {
            listen: gateway.effects.listen,
            host,
        })
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

    /// What to know before picking a harness for `prompt`: the candidates
    /// (`scope`'s fleet entry for the task's kind, or the harnesses the
    /// caller may run), each with its availability and recorded outcomes,
    /// the router's own pick, the budget left, and notes. Nothing runs.
    /// `kind` overrides the classifier. See `docs/fleet.md#advice`.
    pub fn advise(
        &self,
        prompt: &str,
        options: &TaskOptions,
        kind: Option<TaskKind>,
        scope: AdviceScope<'_>,
    ) -> Result<Advice, Error> {
        fleet::advise(self, options, prompt, kind, scope)
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

    /// Run a wide map: `spec.prompt` over every item of `spec.items`, each
    /// on its own branch, at most `spec.concurrency` at once, each answer
    /// checked against `spec.schema`, then the reduce turn. Recorded in
    /// `.branchyard/maps/<name>/`; running it again skips the items done.
    /// See `docs/map.md`.
    pub fn map(&self, spec: MapSpec, options: &MapOptions) -> Result<MapReport, Error> {
        map::run(self, spec, options)
    }

    /// Every recorded map, oldest first.
    pub fn maps(&self) -> Result<Vec<MapSummary>, Error> {
        map::list(self)
    }

    /// A recorded map's rows and progress.
    pub fn map_report(&self, name: &str) -> Result<MapReport, Error> {
        map::report(self, name)
    }

    /// A recorded map's spec, with its items, for running it again.
    pub fn map_spec(&self, name: &str) -> Result<MapSpec, Error> {
        map::spec(self, name)
    }

    /// Forget a map's record; its branches stay. Refused while it runs.
    pub fn remove_map(&self, name: &str) -> Result<(), Error> {
        map::remove(self, name)
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

    /// How this yard (and its clones) captures memory observations and
    /// consolidates them; see `docs/memory.md`. Replaces the settings set
    /// before; without a call, [`MemorySettings::default`].
    pub fn use_memory(&self, settings: MemorySettings) {
        *projection::lock(&self.hub.memory) = Some(Arc::new(settings));
    }

    /// The settings [`Yard::use_memory`] set, or the defaults.
    pub fn memory_settings(&self) -> Arc<MemorySettings> {
        projection::lock(&self.hub.memory)
            .clone()
            .unwrap_or_default()
    }

    /// The team's memory scope of this repository on `tenant`
    /// ([`memory::observe::LOCAL_TENANT`] without a server).
    pub fn memory_scope(&self, tenant: &str) -> MemoryScope {
        MemoryScope::Repo {
            tenant: tenant.to_owned(),
            repo: self.store.label(),
        }
    }

    /// One person's memory scope on `tenant`.
    pub fn person_memory_scope(&self, tenant: &str, subject: &str) -> MemoryScope {
        MemoryScope::Person {
            tenant: tenant.to_owned(),
            subject: subject.to_owned(),
            repo: None,
        }
    }

    /// Derive, scrub and store the observations of `branch`'s turn `turn`
    /// now, with `secrets` the values to scrub: what the engine does at
    /// every turn's end. Idempotent per turn; returns the observation ids.
    pub fn observe_turn(
        &self,
        branch: &str,
        turn: u32,
        secrets: &[String],
    ) -> Result<Vec<u64>, Error> {
        let incarnation = memory::observe::incarnation_of(self, branch)?;
        Ok(memory::observe::observe_turn(self, branch, incarnation, turn, secrets, None)?.ids)
    }

    /// The observations of `scope` with an id above `since`, oldest
    /// first, at most `limit`; for `by memory observations` and the tests.
    pub fn observations(
        &self,
        scope: &MemoryScope,
        since: u64,
        limit: usize,
    ) -> Result<Vec<Observation>, Error> {
        self.store.memory().observations_since(scope, since, limit)
    }

    /// Memory rows matching `filter`, by id.
    pub fn memory_records(&self, filter: &MemoryFilter) -> Result<Vec<MemoryRecord>, Error> {
        memory::review::list(self, filter)
    }

    /// One memory row.
    pub fn memory_record(&self, id: u64) -> Result<MemoryRecord, Error> {
        memory::review::get(self, id)
    }

    /// The observations a memory row's provenance names.
    pub fn memory_evidence(&self, id: u64) -> Result<Vec<Observation>, Error> {
        memory::review::evidence(self, id)
    }

    /// Approve memory row `id` as `by`: it is written into its scope's
    /// bundle (`.agents/skills/` of the checkout for the team's;
    /// `person_dir`, the subject's skills folder, for a person's) and
    /// marked approved. A person's memory is approved only by its subject.
    pub fn approve_memory(
        &self,
        id: u64,
        by: &str,
        person_dir: Option<&Path>,
    ) -> Result<MemoryApproved, Error> {
        memory::review::approve(self, id, by, person_dir)
    }

    /// Reject memory row `id` as `by`, with `reason`; the rejection sticks
    /// against new evidence.
    pub fn reject_memory(
        &self,
        id: u64,
        by: &str,
        reason: Option<&str>,
    ) -> Result<MemoryRecord, Error> {
        memory::review::reject(self, id, by, reason)
    }

    /// Change memory row `id`; an approved memory's file is rewritten too.
    pub fn edit_memory_record(
        &self,
        id: u64,
        edit: &MemoryRecordEdit,
        by: &str,
        person_dir: Option<&Path>,
    ) -> Result<MemoryRecord, Error> {
        memory::review::edit(self, id, edit, by, person_dir)
    }

    /// Offer a person's memory `id` to the team: a new proposed row in
    /// `repo`; the original stays.
    pub fn promote_memory(
        &self,
        id: u64,
        by: &str,
        repo: MemoryScope,
    ) -> Result<MemoryRecord, Error> {
        memory::review::promote(self, id, by, repo)
    }

    /// Fold memory row `from` into `into`: provenance union, `seen` summed;
    /// `from` is deleted. For two proposals that say one thing.
    pub fn merge_memory(&self, into: u64, from: u64) -> Result<MemoryRecord, Error> {
        self.store.memory().merge(into, from)?;
        memory::review::get(self, into)
    }

    /// Delete memory row `id` and its bundle file, recording it without the
    /// text on the branches it came from.
    pub fn forget_memory(
        &self,
        id: u64,
        by: &str,
        person_dir: Option<&Path>,
    ) -> Result<MemoryRecord, Error> {
        Ok(memory::review::forget(self, id, by, person_dir)?.memory)
    }

    /// Run one deterministic consolidation of `scope` now, as `by`; see
    /// `docs/memory.md`, section 4. Nothing is approved.
    pub fn consolidate_memory(
        &self,
        scope: &MemoryScope,
        by: &str,
        trigger: &str,
    ) -> Result<Consolidated, Error> {
        memory::consolidate::consolidate(self, scope, by, trigger)
    }

    /// The scopes with observations no consolidation has read yet.
    pub fn memory_scopes_due(&self) -> Result<Vec<MemoryScope>, Error> {
        memory::consolidate::due_scopes(self)
    }

    /// Consolidation runs, newest first, at most `limit`; of `scope` when
    /// given.
    pub fn memory_runs(
        &self,
        scope: Option<&MemoryScope>,
        limit: usize,
    ) -> Result<Vec<MemoryRun>, Error> {
        self.store.memory().runs(scope, limit)
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

    /// The team's memories: every memory of the `memory-<topic>` bundles
    /// under `.agents/skills/` of the repository's checkout, by topic. A
    /// person's own are a [`memory::Library`] over their skills folder.
    /// See `docs/memory.md`.
    pub fn memories(&self) -> Result<Vec<Memory>, Error> {
        memory::Library::team(&self.root).read()
    }

    /// Add a team memory to `.agents/skills/memory-<topic>/` of the
    /// checkout, rewriting the topic's bundle: the files to commit and
    /// review as a pull request, which reach turns after `by skills
    /// trust`. Nothing a turn can find until then.
    pub fn add_memory(&self, new: NewMemory) -> Result<Memory, Error> {
        Ok(memory::Library::team(&self.root).add(new)?.memory)
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

    /// Relay an immutable settled result to another branch without steering
    /// or scheduling its next turn; see [`HandoffRequest`].
    pub fn handoff(&self, request: HandoffRequest) -> Result<Message, Error> {
        crate::handoff::relay(self, None, request)
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
        policy_binding: match profile.composition.binding {
            branchyard_harness::Binding::Always => PolicyBinding::Anywhere,
            branchyard_harness::Binding::PrivateHome => PolicyBinding::PrivateHome,
            branchyard_harness::Binding::Unverified => PolicyBinding::Unverified,
        },
    })
}

/// [`harness_profile`], refused as a run refuses it before it creates
/// anything: no harness or profile of that name, or one whose tools would
/// run without the policy when `unapproved_tools` is not set. A caller can
/// ask before it opens a [`Yard`], which creates `.branchyard/`.
pub fn admit_harness(id: &str, unapproved_tools: bool) -> Result<HarnessProfile, Error> {
    harness::check_approvals(harness::select(Some(id))?, unapproved_tools)?;
    harness_profile(id)
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
    /// Where it keeps a restricting policy against its harness's own
    /// permissions; elsewhere such a run needs
    /// [`TaskOptions::unapproved_tools`]. See
    /// `docs/harness-integration.md#composing-with-a-harnesss-own-permissions`.
    #[serde(default)]
    pub policy_binding: PolicyBinding,
}

/// Where a profile keeps a restricting policy against its harness's own
/// permissions ([`HarnessProfile::policy_binding`]).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyBinding {
    /// In any home.
    Anywhere,
    /// Only in a home Branchyard prepares: an isolated branch or a sandbox.
    PrivateHome,
    /// Nowhere yet: no mechanism is verified for it.
    #[default]
    Unverified,
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
    /// Structured output for a root run or fan. Deliverable contracts allow
    /// useful work without Git changes; native final JSON is captured without
    /// enabling delegation or controlling the harness's own planning.
    pub result: Option<ResultContract>,
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
    /// Update only model or reasoning effort during a send, preserving the
    /// stored credentials and other provisioning. Mutually exclusive with
    /// [`TaskOptions::provision`]; refused for new tasks, forks and reincarnations.
    pub session_patch: Option<SessionPatch>,
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
    /// The tenant's tool-policy version a server admitted this call under.
    /// The branch's turn manifest records it when it is resolved, and a
    /// send that carries a newer one (the server re-checked the session's
    /// tools against the policy) updates it. `None`: no server, or no
    /// change. See `docs/connectors.md#the-turn-manifest`.
    pub policy_version: Option<u64>,
    /// The built-in skills the policy allows as it stands for this call,
    /// by id: a server that re-checked the session's tools sets it. Each
    /// turn narrows the branch's stored
    /// [`Provisioning::allowed_skills`] to it ([`Provisioning::narrow_allowed_skills`]),
    /// so a policy narrowed after admission mounts fewer skills the next
    /// turn; it never widens them. `None`: no word, the stored set stands.
    pub allowed_skills: Option<Vec<String>>,
    /// The server trigger run that started a new task, recorded on it
    /// ([`tasks::Task::trigger`]); a server sets it when a trigger fires.
    /// Read only when a branch starts a new task.
    pub trigger: Option<tasks::TaskTrigger>,
    /// A W3C `traceparent` for this call's turns: each turn's harness gets
    /// it as `TRACEPARENT`, so what it calls (a connector SDK putting it in
    /// the gateway call's `_meta`, a tool exporting its own spans) joins
    /// the caller's trace. Not stored with the branch. A server sets it to
    /// its operation's span; see `docs/observability.md`.
    pub trace_parent: Option<String>,
    /// When the operation these turns belong to was admitted, in
    /// milliseconds since the Unix epoch: where each turn's startup phases
    /// are counted from ([`startup::Startup`]), so the queue's wait is
    /// the first of them. `None`: a turn counts from its own start. A
    /// server sets it to its operation's creation time.
    pub admitted_ms: Option<u64>,
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
    /// The fleet table the task was started under, for the advice a
    /// delegating harness asks for ([`Yard::advise`], `by fleet advise`):
    /// its entry for a subtask's kind lists the candidates. Carried to
    /// children. `None` lists the harnesses the envelope allows instead.
    pub fleet: Option<Fleet>,
    /// The task a new top-level branch joins as another attempt (its ID);
    /// `None` starts a task of its own (a fan's branches share one, and a
    /// fork joins its parent's). Read only when a branch is created; see
    /// [`tasks`].
    pub join_task: Option<String>,
    /// Tools a new branch's harness is denied outright, ahead of
    /// [`TaskOptions::policy`]'s rules; a trailing `*` matches a prefix
    /// (`by run --deny`). Stored with the branch: they bound every later
    /// turn, whoever sends it, and every child it delegates to, as a
    /// parent's denials bound a spawned child (`by spawn --deny`). Read
    /// only when a branch is created.
    pub deny: Vec<String>,
    /// Whether a new branch stages the repository's own skills
    /// (`.agents/skills/` at its base commit), and why not when it does
    /// not. Read only when a branch is created by `run`, a fan or
    /// `run_attempts`; a fork, reincarnation or delegated child inherits
    /// its parent's. Withheld by default. Whether a repository's skills
    /// may be staged is the caller's decision: `by` stages a set you
    /// trusted by its digest (`by skills trust`), a server one its
    /// operator allows (`allow_repository_skills`). See
    /// `docs/knowledge-skills.md`.
    pub repository_skills: SkillGate,
    /// The person's skills folder (`~/.config/branchyard/skills/`, the
    /// local `user` source of `docs/skill-registry.md`) whose skills a new
    /// branch's turns stage beside the built-in and repository skills,
    /// read again each turn; `None` stages none. `by` sets it outside any
    /// branch; a server and the SDK leave it. Read only when a branch is
    /// created by `run`, a fan or `run_attempts`; a fork, reincarnation or
    /// delegated child inherits its parent's. See `docs/knowledge-skills.md`.
    pub user_skills: Option<PathBuf>,
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
    /// A machine an environment recipe's scripts make, per turn (or kept
    /// paused between turns when the recipe can suspend and resume),
    /// reached over ssh or the recipe's exec command; the worktree and the
    /// private home are copied in and back. See `docs/recipes.md`.
    Recipe(RecipeOptions),
    /// An owned container per turn with copied workspace/home and a required
    /// runtime. The whole allocation is destroyed after output capture.
    Container(ContainerOptions),
    /// A Kubernetes SIGs Agent Sandbox per turn on managed GKE gVisor or Kata,
    /// with copied files and whole-allocation destruction after capture.
    AgentSandbox(AgentSandboxOptions),
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
    /// A kept sandbox unused for longer than this many seconds is
    /// destroyed instead of resumed: by the branch's next turn, or by the
    /// next turn ending on this provider. Unset: kept until `max_paused`
    /// evicts it. See `docs/sandbox-snapshots.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_kept_secs: Option<u64>,
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
    /// The router base URL. The bridge requests carry the actor routing
    /// header. Use `https://` or `wss://`, or `http://` or `ws://` on
    /// loopback or with [`SubstrateOptions::insecure`].
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
    /// Path to a Control bearer token, reread for every RPC.
    #[serde(default)]
    pub api_token_file: Option<PathBuf>,
    /// Verified TLS service name for Control, also through a port-forward.
    #[serde(default)]
    pub server_name: Option<String>,
    /// Outgoing connections actors may make through Substrate.
    #[serde(default)]
    pub egress: config::SubstrateEgress,
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
    /// A kept sandbox unused for longer than this many seconds is
    /// destroyed instead of resumed: by the branch's next turn, or by the
    /// next turn ending on this provider. Unset: kept until `max_paused`
    /// evicts it. See `docs/sandbox-snapshots.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_kept_secs: Option<u64>,
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

/// An environment recipe a branch's harness runs on, as it was trusted when
/// the branch was given it: its commands are stored with the branch, so
/// every later turn, its recovery and its removal run what was trusted,
/// not what the configuration says by then. Resolved and trust-checked by
/// the caller (`by run --provider recipe:NAME`); see `docs/recipes.md`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RecipeOptions {
    /// The recipe's name, `[recipes.NAME]`.
    pub name: String,
    /// Prints the new machine's result (`create`).
    pub create: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// As written: `"none"` disables it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destroy: Option<String>,
    /// Per script; unset: 900.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// The SHA-256 of the commands that were trusted, for messages.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
    /// Where the worktree is placed on the machine. Empty means
    /// `/tmp/branchyard/<branch>-<8 hex>/workspace`, unique to this
    /// repository's branch.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workdir: String,
    /// The harness's `HOME` on the machine. Empty means the same directory's
    /// `home`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub home: String,
    /// Variables copied by name from this process to the machine. Names
    /// are stored with the branch; values are not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pass_env: Vec<String>,
    /// Destroy the machine when a turn ends (the default), or suspend it
    /// for the next turn; pausing needs both `suspend` and `resume`.
    #[serde(default, skip_serializing_if = "SandboxKeep::is_destroy")]
    pub keep: SandboxKeep,
    /// At most this many kept machines of this recipe in the repository;
    /// parking one more destroys the least recently used. Unset: 4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_paused: Option<u32>,
    /// A kept sandbox unused for longer than this many seconds is
    /// destroyed instead of resumed: by the branch's next turn, or by the
    /// next turn ending on this provider. Unset: kept until `max_paused`
    /// evicts it. See `docs/sandbox-snapshots.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_kept_secs: Option<u64>,
}

impl RecipeOptions {
    /// The default directory on the machine for `worktree`'s branch
    /// `branch`: `/tmp/branchyard/<branch>-<8 hex of the worktree path>`.
    fn base(branch: &str, worktree: &std::path::Path) -> String {
        let hash = blake3::hash(worktree.as_os_str().as_encoded_bytes());
        format!("/tmp/branchyard/{branch}-{}", &hash.to_hex().as_str()[..8])
    }

    /// [`RecipeOptions::workdir`], defaulted for the branch whose worktree
    /// is `worktree`.
    pub fn workdir(&self, branch: &str, worktree: &std::path::Path) -> String {
        match self.workdir.is_empty() {
            true => format!("{}/workspace", RecipeOptions::base(branch, worktree)),
            false => self.workdir.clone(),
        }
    }

    /// [`RecipeOptions::home`], defaulted likewise.
    pub fn home(&self, branch: &str, worktree: &std::path::Path) -> String {
        match self.home.is_empty() {
            true => format!("{}/home", RecipeOptions::base(branch, worktree)),
            false => self.home.clone(),
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
    /// The yard's configuration, when it has one: what a run fills in.
    defaults: Option<Arc<config::ProjectConfig>>,
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

    /// Request structured output; see [`TaskOptions::result`].
    pub fn result(mut self, contract: ResultContract) -> Self {
        self.options.result = Some(contract);
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
    pub fn run(mut self) -> Result<Branch, Error> {
        self.complete()?;
        let branch = run::run(&self.yard, &self.prompt, &self.options)?;
        goal::pursue(&self.yard, branch, &self.options)
    }

    /// Run the same task on several harnesses in parallel, one branch each,
    /// named `<name>-<harness>`. Every harness is resolved and found before
    /// any branch is created. A failure on one branch does not stop the
    /// others; it is recorded in that branch's status.
    pub fn run_on(mut self, harnesses: &[&str]) -> Result<Vec<Branch>, Error> {
        self.complete()?;
        let branches = run::run_on(&self.yard, &self.prompt, &self.options, harnesses)?;
        goal::pursue_all(&self.yard, branches, &self.options)
    }

    /// Fill what the options leave unset from the yard's configuration,
    /// when it has one.
    fn complete(&mut self) -> Result<(), Error> {
        match &self.defaults {
            Some(config) => configure::complete(&self.yard, config, &mut self.options),
            None => Ok(()),
        }
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

    /// The prompt `by send --retry` submits again: that of this branch's
    /// last turn that recovery found cut off, kept until a prompt reaches
    /// its harness. Refused, with what to do instead, when there is none.
    pub fn retry_prompt(&self) -> Result<String, Error> {
        run::retry_prompt(&self.yard.store().read(&self.info.name)?)
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

    /// The commit of this branch's checkpoint `turn` (0 is its base), or the
    /// error [`Branch::rewind`] and [`Branch::fork_at`] give for it: no such
    /// checkpoint, or its ref no longer names its commit. Lets a caller
    /// refuse a checkpoint before it queues work at it.
    pub fn checkpoint_commit(&self, turn: u32) -> Result<String, Error> {
        checkpoint::commit_at(&self.yard, &self.info.name, turn)
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

    /// The structured output contract stored with this branch, when requested.
    pub fn result_contract(&self) -> Result<Option<ResultContract>, Error> {
        Ok(self.yard.store().read(&self.info.name)?.result_contract)
    }

    /// The turn manifest the session runs with: what it was admitted with,
    /// resolved when it started and again only when its tools changed.
    /// `None` until its first turn has started.
    pub fn manifest(&self) -> Result<Option<manifest::TurnManifest>, Error> {
        Ok(self.yard.store().read(&self.info.name)?.manifest)
    }

    /// Where this branch's harness runs; `None` is a local process.
    pub fn provider(&self) -> Result<Option<Provider>, Error> {
        Ok(self.yard.store().read(&self.info.name)?.provider)
    }

    /// The executable and fixed arguments the branch was started with in
    /// place of its profile's own (`TaskOptions::command`), which its
    /// later turns keep; `None` when it runs the profile's.
    pub fn command(&self) -> Result<Option<Vec<String>>, Error> {
        Ok(self.yard.store().read(&self.info.name)?.command)
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
    /// What the running turn is parked on, waiting for a person: the
    /// oldest approval ask it has open (`docs/effects.md#approvals`).
    /// Read from the pending asks when the record is listed or looked up,
    /// never stored, so it is exact: `None` the moment the ask is
    /// answered, and `None` whenever the branch is not running. A parked
    /// turn is never stalled; it moves only when someone answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaiting: Option<Awaiting>,
    /// What it needs from a person now, and why: the first row of
    /// [`stage::STAGES`] that matches it (`docs/concepts.md`, "What a
    /// branch needs from you"). Derived when the record is listed or
    /// looked up, never stored, like [`BranchInfo::awaiting`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<stage::Stage>,
    /// Whether a running turn's engine is still heard from: `live`,
    /// `unknown` once its lease renewals stopped, `lost` once the lease no
    /// longer protects it and recovery will end the turn. Read from the
    /// branch's lease when the record is listed or looked up, never stored;
    /// `None` whenever the branch is not running. See
    /// `docs/durability.md#liveness`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liveness: Option<Liveness>,
    /// The branch [`Branch::reincarnate`] started from this one's latest
    /// candidate, with a fresh session and a handoff brief.
    #[serde(default)]
    pub superseded_by: Option<String>,
    /// The model its harness runs, as it was asked for (`--model`), when
    /// one was; unset, the harness's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Where its harness runs, as `--provider` names it: `local`,
    /// `microsandbox`, `substrate` or `recipe:NAME`. A record from before
    /// this was kept reads `local`.
    #[serde(default = "local_provider")]
    pub provider: String,
    /// Its harness runs on this host with a private `HOME` of its own
    /// (`--isolated`, or inherited from the branch it was forked or
    /// delegated from). Not a sandbox: it still runs as your user and can
    /// read your files and reach your network. `false` for a sandbox
    /// provider, whose private home is the sandbox's.
    #[serde(default)]
    pub isolated: bool,
    /// What held its harness to its permission preset's file scope on its
    /// last turn: Landlock and where it could write, or nothing and why
    /// (`docs/modes.md#the-file-guard`). `None` until a turn has started
    /// its harness, and for a record from before it was kept.
    #[serde(default)]
    pub guard: Option<Guard>,
}

/// The `provider` of a record that predates the field.
fn local_provider() -> String {
    "local".to_owned()
}

/// The oldest of the pending `asks` of each branch that has one, by
/// branch: what a running turn is parked on.
fn parked(asks: Vec<effects::ApprovalAsk>) -> HashMap<String, Awaiting> {
    let mut parked = HashMap::new();
    for ask in asks {
        parked
            .entry(ask.branch.clone())
            .or_insert_with(|| Awaiting {
                kind: AwaitingKind::Approval,
                id: ask.id.clone(),
                about: ask.about.describe(),
                since_ms: ask.created_ms,
            });
    }
    parked
}

/// `info` with what its turn is parked on, if it is running and has an
/// ask open; any other state waits on nobody.
fn with_awaiting(mut info: BranchInfo, parked: &HashMap<String, Awaiting>) -> BranchInfo {
    info.awaiting = match info.status {
        BranchStatus::Running => parked.get(&info.name).cloned(),
        _ => None,
    };
    info
}

/// Every running `info` has liveness: its held lease's, or lost when no
/// held lease backs it. Other states have no engine to hear from.
fn with_liveness(mut info: BranchInfo, alive: &HashMap<String, Liveness>) -> BranchInfo {
    info.liveness = match info.status {
        BranchStatus::Running => Some(
            alive
                .get(&info.name)
                .cloned()
                .unwrap_or_else(Liveness::unowned),
        ),
        _ => None,
    };
    info
}

/// What a running turn waits on a person for; see [`BranchInfo::awaiting`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Awaiting {
    /// What kind of answer it waits for.
    pub kind: AwaitingKind,
    /// The ask's id: `by approvals allow ID`, `…/approvals/{id}/allow`.
    pub id: String,
    /// What it asks, in words, as [`effects::AskAbout::describe`] puts it.
    pub about: String,
    /// When the turn parked, milliseconds since the Unix epoch.
    pub since_ms: u64,
}

/// The kinds of answer a parked turn waits for.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AwaitingKind {
    /// An [`effects::ApprovalAsk`]: may the turn run a tool or a call?
    Approval,
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
    /// A delegating branch whose turn ended while children it delegated
    /// were still running or waiting: it is parked, and its next turn
    /// starts on its own once they have all settled, with a summary of
    /// what they did (`docs/delegation.md`, "Waiting on children"). Not
    /// finished: a wait for its subtree waits for that turn too.
    WaitingOnChildren,
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
    /// Settled and set aside on purpose, with the reason (`by discard`):
    /// it runs no more turns and is never integrated, its record and cost
    /// are kept, and it no longer counts against its parent's
    /// `max_children`.
    Discarded {
        /// Why, as whoever discarded it said.
        reason: String,
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
    /// How long a turn with no duration limit may be held open after its
    /// harness answered it, for the harness's background tasks, before the
    /// hold is cut and the turn ends with the outcome it was held with.
    /// `None` is 30 minutes.
    pub hold_cap: Option<Duration>,
    /// How long a held turn may wait for its harness to start the cycle on
    /// the notification of background tasks that ended, with none running
    /// ([`branchyard_harness::Driver::awaiting_follow_up`]), before the
    /// hold ends and the turn keeps the outcome it was held with. `None`
    /// is 10 seconds.
    pub follow_up_grace: Option<Duration>,
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

    /// Cut a held turn's hold after this long; see [`Budget::hold_cap`].
    pub fn hold_cap(mut self, cap: Duration) -> Self {
        self.hold_cap = Some(cap);
        self
    }

    /// End a hold that waits for a follow-up cycle none started after
    /// this long; see [`Budget::follow_up_grace`].
    pub fn follow_up_grace(mut self, grace: Duration) -> Self {
        self.follow_up_grace = Some(grace);
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
    /// The preset this policy came from, whose approvals are the last
    /// layer connector operations resolve through (`docs/effects.md`).
    preset: Option<PolicyPreset>,
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
    /// Whether this policy refuses or asks about anything: a deny rule, or
    /// a fallback that is not allow. A policy that allows every call needs
    /// no harness to keep a restriction for it.
    pub fn restricts(&self) -> bool {
        !matches!(self.fallback, Fallback::Allow) || self.rules.iter().any(|rule| !rule.allow)
    }

    /// One line saying what the rules allow and deny, and what happens to
    /// everything else: a task's record of the policy it ran under.
    pub fn summary(&self) -> String {
        let tools = |allow: bool| {
            self.rules
                .iter()
                .filter(|r| r.allow == allow)
                .map(|r| r.tool.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut parts = Vec::new();
        for (allow, word) in [(true, "allow"), (false, "deny")] {
            let listed = tools(allow);
            if !listed.is_empty() {
                parts.push(format!("{word} {listed}"));
            }
        }
        parts.push(
            match self.fallback {
                Fallback::Allow => "allow the rest",
                Fallback::Deny => "deny the rest",
                Fallback::Ask(_) => "ask about the rest",
            }
            .into(),
        );
        parts.join("; ")
    }

    pub fn allow_all() -> Self {
        Policy {
            rules: Vec::new(),
            fallback: Fallback::Allow,
            preset: None,
        }
    }

    pub fn deny_all() -> Self {
        Policy {
            rules: Vec::new(),
            fallback: Fallback::Deny,
            preset: None,
        }
    }

    /// Ask `ask(branch, request)` for anything no rule decides.
    pub fn ask(
        ask: impl Fn(&str, &PermissionRequest) -> PermissionDecision + Send + Sync + 'static,
    ) -> Self {
        Policy {
            rules: Vec::new(),
            fallback: Fallback::Ask(Arc::new(ask)),
            preset: None,
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

    /// Allow exactly the harness's shell commands that run `by` with an
    /// operation a harness may run on its own branch (every row of
    /// [`operations::OPERATIONS`] allowed inside a harness: `spawn`,
    /// `inspect`, `send`, `discard`, `artifact publish`, `ask`, ...),
    /// and nothing else. The command
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

    /// The preset this policy stands for, when it came from one.
    pub fn preset(&self) -> Option<PolicyPreset> {
        self.preset
    }

    /// Remember that this policy stands for `preset`, whose approvals
    /// connector operations then resolve through.
    pub fn with_preset(mut self, preset: Option<PolicyPreset>) -> Self {
        self.preset = preset;
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
        policy::decide(self, branch, request, None)
    }

    /// [`Policy::decide_with_source`] for a harness working in `root`: a
    /// rule's relative path (`Edit(src/**)`) is matched against the call's
    /// path relative to `root`, and a path outside it matches no relative
    /// rule. See `docs/egress.md#rules-that-name-arguments-and-paths`.
    pub fn decide_in(
        &self,
        branch: &str,
        request: &PermissionRequest,
        root: &std::path::Path,
    ) -> (PermissionDecision, DecisionSource) {
        policy::decide(self, branch, request, Some(root))
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
    /// The approval policy, which tightened what the policy allowed: a
    /// block, or an ask a person (or the ask's expiry) answered. See
    /// `docs/effects.md`.
    Approval {
        resolved: effects::Resolved,
        /// The ask, when one was asked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ask: Option<String>,
        /// Who answered it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
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
    /// What the turn that is ending cost on its own, in US dollars,
    /// recorded once its metering has settled (the harness's closing
    /// figure folded in) and before the branch's closing status. For a
    /// harness that reports its session's running total, the part of it
    /// this turn added, never the total; on the model gateway, the turn's
    /// metered calls. Recorded only for a turn whose spend something
    /// measured: a harness that reports no usage records none. The
    /// branch's running total stays [`BranchInfo::cost_usd`]. A record of
    /// its own rather than a field of the harness's `turn_ended`, which
    /// arrives before the harness's closing figure, or of
    /// [`Activity::Checkpoint`], which a turn whose snapshot failed does
    /// not record. See `docs/delegation.md#budgets`.
    TurnCost {
        /// The turn's own cost.
        cost_usd: f64,
    },
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
        /// The skills staged for the turn, built-in and the repository's,
        /// each by layer, name and digest, with the set's digest; and the
        /// repository's skills that were found and withheld, and why.
        /// Recorded on every turn that reaches provisioning, whether or not
        /// a file changed; an event from before it was kept reads empty.
        /// See `docs/knowledge-skills.md`.
        #[serde(default)]
        skills: skills::SkillSet,
        /// The managed settings a sandboxed Claude Code turn wrote before 3
        /// November 2026, kept so those turns' records read back; no turn
        /// writes them now (`docs/provisioning.md#managed-settings`). Names
        /// and flags only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        managed: Option<branchyard_provision::ManagedWritten>,
        /// The permission rules written into the harness's own settings,
        /// from the branch's policy, for a harness that enforces its own
        /// rather than asking Branchyard (Antigravity); see
        /// `docs/provisioning.md#antigravity-permission-rules`. Rules only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permissions: Option<branchyard_provision::PermissionsWritten>,
        /// The digest of the turn manifest the session runs with (its file
        /// is `.branchyard/manifest.json`, named by `BRANCHYARD_MANIFEST`,
        /// and the gateway token's `by_manifest` carries the same value);
        /// see `docs/connectors.md#the-turn-manifest`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        manifest: Option<String>,
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
    /// The turn that just began is traced live: its spans are exported as
    /// the events are recorded, under this W3C `traceparent`. Recorded right
    /// after the turn's [`Activity::Prompt`] by the process that traces it,
    /// so a reader in any process (a server writing spans after the fact)
    /// knows the turn is already traced and writes no second set.
    Traced {
        /// The turn span's `traceparent`.
        traceparent: String,
    },
    /// A harness-to-harness message was sent or delivered; see
    /// [`crate::inbox`]. Recorded on both the sending and the receiving
    /// branch's event log.
    // The doc comment is the schema description, so the private link stays.
    #[allow(rustdoc::private_intra_doc_links)]
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
    /// Feedback sent as the prompt (or steer) whose digest it names, as
    /// data: `by pr --watch`'s pull request feedback or `by review`'s
    /// comments. Recorded just before that prompt. See [`Feedback`].
    Feedback(Box<Feedback>),
    /// Where a turn's sandbox came from (fresh, resumed, branched from a
    /// provider snapshot) and what became of it: kept, destroyed, evicted,
    /// a snapshot released. See `docs/sandbox-snapshots.md`.
    Sandbox(Box<SandboxEvent>),
    /// How long the turn took to start, phase by phase, from the
    /// operation's admission to the harness's first event, and the provider
    /// and path that made its sandbox ready. Recorded once per turn that
    /// started a harness. See [`startup::Startup`] and
    /// `docs/providers.md#measuring-launch-time`.
    Startup(Box<startup::Startup>),
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
    /// What the turn did with its skills: a search of its tool tree (the
    /// term count and the ids found, never the words), a skill, reference,
    /// connector or helper shown, a staged skill's script run. A local
    /// measure for `by skills usage` and the skill-discovery eval, never an
    /// input to ranking. See [`SkillActivity`] and
    /// `docs/skill-registry.md`.
    Skill(Box<SkillActivity>),
    /// The branch was made from a harness session that already existed on
    /// this machine (`by adopt`); its next turn resumes that session.
    Adopted(Box<Adoption>),
    /// Repository knowledge: the branch was distilled into proposed
    /// entries. See [`KnowledgeActivity`] and `docs/knowledge.md`.
    Knowledge(Box<KnowledgeActivity>),
    /// Memory: a consolidation consumed this branch's observations, or a
    /// memory they fed was forgotten. See [`memory::MemoryActivity`] and
    /// `docs/memory.md`.
    Memory(Box<memory::MemoryActivity>),
    /// Plan approval: planning, a proposed plan, its approval, rejection or
    /// escalation. See [`PlanActivity`] and `docs/plans-and-goals.md`.
    Plan(Box<PlanActivity>),
    /// A goal and its judge's verdicts. See [`GoalActivity`].
    Goal(Box<GoalActivity>),
    /// The model gateway: how the turn's harness reaches its models, each
    /// call it made through the gateway, and budget alerts. See
    /// [`models::ModelActivity`] and `docs/model-gateway.md`.
    Model(Box<models::ModelActivity>),
    /// The person's ceiling narrowed what the turn may reach. See
    /// [`AccessActivity`].
    Access(Box<AccessActivity>),
    /// Approvals and the effect ledger: a person asked and answering, a
    /// call blocked, an entry begun, confirmed, staged, reconciled or
    /// undone. See [`effects::EffectActivity`] and `docs/effects.md`.
    Effect(Box<effects::EffectActivity>),
    /// The harness's own account of what it is doing, from its
    /// `report_status` tool or `by report-status`: a claim beside the
    /// protocol's status, never a replacement for it. See
    /// `docs/delegation.md#self-reported-status`.
    StatusReport {
        /// What the harness says it is doing.
        state: ReportedState,
        /// One line of at most 280 characters.
        summary: String,
        /// Where to read more, an `http` or `https` URL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link: Option<String>,
        /// How long the report holds, in whole seconds (1 to 7 days),
        /// when the harness gave a time-to-live. See
        /// [`reported::judge`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttl_seconds: Option<u64>,
    },
    /// The result the branch reported with `report`, a JSON value that
    /// matched its parent's [`contract::ResultContract`] when it had one:
    /// the latest is the branch's [`Inspection::result`]. See
    /// `docs/delegation.md#result-contract`.
    Result {
        /// The result, as the child gave it.
        value: serde_json::Value,
    },
    /// An entry of the branch's shared brief, recorded on the delegating
    /// branch: every child it delegates to is given all its entries each
    /// turn, oldest first. See `docs/delegation.md#plans-and-the-shared-brief`.
    Brief {
        /// The entry, as the branch wrote it.
        text: String,
    },
    /// A branch that a trigger keeps alive moved through its lifecycle: it
    /// went idle (its kept sandbox released), dormant (its next run starts
    /// a fresh session), was reincarnated, or was deleted. See
    /// [`LifecycleState`] and `docs/triggers.md#lifecycle`.
    Lifecycle {
        /// Where the branch stands now.
        state: LifecycleState,
        /// Why, and what it released.
        reason: String,
        /// Milliseconds since the Unix epoch when the branch last ended a
        /// turn: where its idleness is counted from.
        idle_since_ms: u64,
    },
}

/// Where a branch that lives on stands in its lifecycle: the `state` of
/// [`Activity::Lifecycle`]. Serialized in snake case.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, strum::Display, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum LifecycleState {
    /// It has not ended a turn for its idle time and is running none: what
    /// it held (a kept sandbox) was released. The next send continues it
    /// as before.
    Idle,
    /// It has been idle for its dormant time: the next run that would send
    /// to it starts a fresh harness session on the same worktree instead.
    Dormant,
    /// A fresh harness session was started on the same worktree, with a
    /// handoff of what the old one did.
    Reincarnated,
    /// It had been idle for its delete time, and was removed: the next run
    /// creates it again from the trigger's task.
    Deleted,
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
    /// Where the harness delivers steered input within the running turn,
    /// as `docs/harness-integration.md` "Steering a running turn" names
    /// each profile's boundary (`claude_next_model_call`,
    /// `codex_turn_steer`, ...). Set once the input is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<String>,
}

/// Where a [`Steer`] is. Serialized as an object tagged by `state`, such as
/// `{"state": "refused", "reason": "..."}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[derive(strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum SteerState {
    /// Queued in Branchyard for the running turn; the engine running it
    /// writes it to the harness within about 100 ms, in whichever process
    /// it runs.
    Pending,
    /// Written to the harness's input; the harness has not confirmed it
    /// yet. Stored as `delivered` before it had this name, which still
    /// reads as this state.
    #[serde(alias = "delivered")]
    Written,
    /// Joined the running turn: the harness queued it into the turn in
    /// flight, and the model reads it at the [`Steer::boundary`] the
    /// harness delivers steered input at (for Claude Code, before its next
    /// model call, still within this turn). Steered input never waits for
    /// a later turn: one the turn does not take is refused.
    Accepted,
    /// Never reached the model: the harness refused or dropped it, an
    /// interrupt cancelled it, or the turn ended first.
    Refused { reason: String },
}

/// What a message means, and so who it may go to: a `question` and a
/// `report` go to the sender's parent; an `escalation` goes to the parent
/// too, or further up an ancestor its rig seat's `escalates_to` names; an
/// `answer` goes from a branch to one of its own descendants, and normally
/// carries `in_reply_to` a question's id. A branch nobody delegated (a
/// root, or a fork) sends its questions and escalations to the person,
/// `@person`, whose answers carry `in_reply_to`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Question,
    Report,
    Escalation,
    Answer,
    /// A message between siblings, the children of one parent, of a kind
    /// the registry names in [`Message::meta`] (`note`, or one registered
    /// with [`Yard::register_kind`]); delivered at the recipient's next
    /// turn. See `docs/messaging.md`.
    Peer,
}

impl MessageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageKind::Question => "question",
            MessageKind::Report => "report",
            MessageKind::Escalation => "escalation",
            MessageKind::Answer => "answer",
            MessageKind::Peer => "peer",
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
            "peer" => Ok(MessageKind::Peer),
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
    /// A frozen result and artifact handoff, delivered only at the recipient's next turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<Handoff>,
    /// Leave this message for the next turn instead of steering a running one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub next_turn: bool,
    /// What the legacy fields do not carry: the registered kind and its
    /// version, the payload, the trace and the deadline
    /// ([`messaging::MessageMeta`]). Omitted when there is nothing to
    /// carry, so a message of a legacy kind reads as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<messaging::MessageMeta>,
}

impl Message {
    /// The registered kind's name: `meta.kind` when the message carries
    /// one, else the legacy `kind`'s.
    pub fn kind_name(&self) -> &str {
        self.meta
            .as_ref()
            .and_then(|meta| meta.kind.as_deref())
            .unwrap_or_else(|| self.kind.as_str())
    }

    /// Whether the message carries a deadline ([`messaging::MessageMeta::deadline_ms`])
    /// that is at or before `now_ms`: a message that is not yet delivered
    /// then is expired, not given to a turn.
    pub fn expired_at(&self, now_ms: u64) -> bool {
        self.meta
            .as_ref()
            .and_then(|meta| meta.deadline_ms)
            .is_some_and(|deadline| deadline <= now_ms)
    }
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

/// The text a branch's [`Activity::Warning`] begins with when it records a
/// step that failed during its turn and that the engine survived. The same
/// failure is logged where it happened (on `by`'s standard error); this keeps
/// it with the branch for `by log` and `by show` afterwards.
pub const SURVIVED_FAILURE: &str = "a step failed and the run carried on: ";

/// The survived failures `events` record (see [`SURVIVED_FAILURE`]), each as
/// `step: error`, oldest first.
pub fn survived_failures(events: &[RecordedEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match &event.activity {
            Activity::Warning(text) => text.strip_prefix(SURVIVED_FAILURE),
            _ => None,
        })
        .collect()
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
    /// The merge commit that brought the branch's candidate into the
    /// target.
    pub commit: String,
    /// The target already contained the candidate, so nothing was merged:
    /// the branch was recorded as merged through `via`. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub already: bool,
    /// For a candidate that was already contained: the commit that brought
    /// it in, with its subject (such as a sibling's merge). Omitted
    /// otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

/// Several branches integrated into one target together
/// ([`Delegate::integrate_all`], `by integrate a b c`): merged in order in
/// one temporary worktree, checked once on the result, and promoted with
/// one compare-and-swap of the target from `previous` to `commit`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergedAll {
    /// The parent's git branch the children were merged into.
    pub target: String,
    /// The target before the call.
    pub previous: String,
    /// The target after the call; `previous` when every branch was already
    /// contained.
    pub commit: String,
    /// One per branch, in the order given. Merges stack: each one's
    /// `previous` is the merge before it (the target's `previous` for the
    /// first), so `previous..commit` is that merge's own range.
    pub branches: Vec<Merged>,
    /// The distinct checks of the branches merged, each run once on the
    /// result, in the order they ran; empty when none has a check or
    /// nothing moved. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Vec<String>>,
    /// The same checks, each with the branches whose check it is and its
    /// outcome (all passed). Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check_results: Vec<IntegrationCheck>,
}

/// Known harness profiles, whether their executable is on `PATH`, and
/// their live qualification status, without opening a repository (what
/// [`Yard::harnesses`] returns).
pub fn harnesses() -> Vec<HarnessInfo> {
    harness::list()
}

/// Validate an executable by path or `PATH`, without opening a yard or running it.
pub use harness::check_available as check_harness_command;

/// Known harness profile and local availability.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInfo {
    pub harness: String,
    pub profile: String,
    pub default: bool,
    /// The executable was found on `PATH`, or an admitted server command is executable.
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
    /// No memory has this slug.
    UnknownMemory(String),
    /// A memory or memory bundle that cannot be written or read: a title
    /// or body over its limit, a slug or topic that is not one, a bundle
    /// file that does not parse.
    InvalidMemory(String),
    /// The branch has no plan, or none awaiting approval.
    NoPlan(String),
    /// Not a usable branch name: lowercase `[a-z0-9._-]`, starting with a
    /// letter or digit, one path segment.
    InvalidName {
        name: String,
        reason: String,
    },
    UnknownHarness(String),
    /// The harness executable could not be found or started: `reason`
    /// says so in words, `cause` as data a caller can act on (install it,
    /// fix the command, pick another).
    HarnessUnavailable {
        harness: String,
        reason: String,
        /// Why it cannot start.
        cause: Unavailable,
    },
    /// The requested operation needs a capability the harness lacks.
    Unsupported(String),
    /// A profile whose driver does not route tool permission requests to
    /// Branchyard, run without allowing that
    /// ([`TaskOptions::unapproved_tools`]): its tools would run under the
    /// harness's own configuration, not the task's policy. Its kind is
    /// `unsupported`, like [`Error::Unsupported`]; its detail says
    /// `unrouted_approvals` and the profile.
    UnroutedApprovals {
        /// The profile refused, as `--harness` names it (`pi-rpc`).
        profile: String,
    },
    /// The profile cannot make the task's restrictions binding on its
    /// harness's own permission system ([`branchyard_harness::Composition`]):
    /// the harness's configuration could approve a call the policy refuses,
    /// without asking. Refused unless the task lets the harness's own
    /// permissions decide ([`TaskOptions::unapproved_tools`]). Its kind is
    /// `unsupported`; its detail names `unbound_policy`, the profile, the
    /// reason and whether a private home would bind it.
    UnboundPolicy {
        /// The profile refused (`gemini-cli-acp`).
        profile: String,
        /// Why the profile cannot bind the restrictions here.
        reason: String,
        /// Whether a home Branchyard prepares (`--isolated`, or a sandbox)
        /// would bind them.
        private_home_binds: bool,
    },
    /// A configuration file, or a section of one, that describes nothing
    /// usable: its message names the file and line, or the section
    /// (`[models]`) or key (`defaults.check`).
    Config(String),
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
    /// Integrating several branches together: `branch` conflicted, in
    /// `files`, with the target plus the branches in `merged`, merged
    /// before it in the same call. Nothing was integrated. Its kind is
    /// `conflict`, like [`Error::Conflict`].
    ConflictBetween {
        /// The branch whose merge conflicted.
        branch: String,
        /// The git branch merged into.
        target: String,
        /// The branches merged before it in the same call.
        merged: Vec<String>,
        /// The conflicting paths.
        files: Vec<String>,
    },
    CheckFailed {
        /// The output of the check that failed.
        output_tail: String,
        /// The check that ran and how it ended, when the engine recorded
        /// it (not for a check run elsewhere; an integration of several
        /// branches has `checks`, which name each).
        ran: Option<Box<FailedCheck>>,
        /// Integrating branches: every check of the branches merged, in
        /// order, each with its branches and outcome, the failed one among
        /// them. Empty for a merge of no delegated branch (`by merge`).
        checks: Vec<IntegrationCheck>,
        /// When the branches integrated left out siblings that share their
        /// check and could change the result: who they are, and the
        /// integration that runs it on all of them. A child under its
        /// parent's whole-suite check may pass it only together with its
        /// siblings.
        shared: Option<Box<SharedCheck>>,
        /// When siblings left out share the failed check but nothing says
        /// they are needed (the branches integrated passed it alone in
        /// their last `by check` of the same work, or none is recorded):
        /// the branches integrated, on whose own work it failed. Empty
        /// otherwise.
        own_work: Vec<String>,
    },
    CheckTimedOut {
        timeout: Duration,
        output_tail: String,
        /// As [`Error::CheckFailed`]'s `checks`.
        checks: Vec<IntegrationCheck>,
    },
    /// The check command could not be started.
    CheckNotStarted {
        /// Why it could not start.
        reason: String,
        /// As [`Error::CheckFailed`]'s `checks`, the one that could not
        /// start among them.
        checks: Vec<IntegrationCheck>,
    },
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
    /// The call lacks an argument its operation needs, or gives
    /// arguments that contradict each other; the message says which and
    /// how to give it. Its kind is `usage`.
    Usage(String),
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
    /// broker, with the [`Error::kind`] and [`Error::detail`] it had there.
    Remote {
        kind: String,
        message: String,
        /// What the error carried beyond its message, as
        /// [`Error::detail`] gave it there.
        detail: Option<Box<serde_json::Value>>,
    },
    Git(String),
    Harness(String),
    Io(std::io::Error),
    State(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotARepository(path) => write!(
                f,
                "{} is not inside a git repository",
                std::path::absolute(path)
                    .unwrap_or_else(|_| path.clone())
                    .display()
            ),
            Error::UnknownBranch(name) => write!(f, "no branch named {name}"),
            Error::BranchExists(name) => write!(
                f,
                "branch {name} already exists; a branch that was discarded or merged keeps its \
                 name until it is removed: `by rm {name}` frees it (a removed child's spend \
                 still counts in its parent's budget), or choose another name"
            ),
            Error::UnknownMessage(id) => write!(f, "no message #{id} in this inbox"),
            Error::UnknownKnowledge(id) => write!(f, "no knowledge entry #{id}"),
            Error::UnknownMemory(id) => write!(f, "no memory {id}"),
            Error::InvalidMemory(why) => write!(f, "{why}"),
            Error::NoPlan(why) => write!(f, "no plan: {why}"),
            Error::InvalidName { name, reason } => {
                write!(f, "{name:?} is not a usable branch name: {reason}")
            }
            Error::UnknownHarness(name) => f.write_str(&harness::unknown_message(name)),
            Error::HarnessUnavailable {
                harness,
                reason,
                cause: Unavailable::NotOnPath,
            } => write!(f, "{harness} is not installed: {reason}"),
            Error::HarnessUnavailable {
                harness, reason, ..
            } => write!(f, "{harness} is unavailable: {reason}"),
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::UnroutedApprovals { profile } => write!(
                f,
                "unsupported: {profile} does not route tool permission requests to Branchyard, \
                 so its tools would run without your policy; allow that explicitly \
                 (--allow-unapproved-tools, TaskOptions::unapproved_tools) or choose another \
                 profile"
            ),
            Error::UnboundPolicy {
                profile,
                reason,
                private_home_binds,
            } => write!(
                f,
                "unsupported: {profile} cannot keep your policy's restrictions{}: {reason}; {}allow \
                 its own permissions to decide (--allow-unapproved-tools, \
                 TaskOptions::unapproved_tools) or choose another profile",
                if *private_home_binds { " in your own home" } else { "" },
                if *private_home_binds { "run it with --isolated, " } else { "" }
            ),
            Error::Config(what) => f.write_str(what),
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
            Error::ConflictBetween {
                branch,
                target,
                merged,
                files,
            } => match merged.is_empty() {
                true => write!(
                    f,
                    "{branch} conflicts with {target} in {}; nothing was integrated",
                    files.join(", ")
                ),
                false => write!(
                    f,
                    "{branch} conflicts with {target} plus {} in {}; nothing was integrated",
                    merged.join(", "),
                    files.join(", ")
                ),
            },
            Error::CheckFailed {
                output_tail,
                ran,
                checks,
                shared,
                own_work,
            } => {
                match ran.as_deref().map(|ran| ran.status.as_str()) {
                    None | Some("") => write!(f, "check failed:\n{output_tail}")?,
                    Some(status) => write!(f, "check failed ({status}):\n{output_tail}")?,
                }
                write_checks(f, checks)?;
                if !own_work.is_empty() {
                    write!(
                        f,
                        "\nNothing was integrated. The check failed on the integrated branches' \
                         own work ({}): no `by check` of that work says it needs a sibling's. \
                         Fix it there, and `by check` before integrating again",
                        own_work.join(", ")
                    )?;
                }
                match shared {
                    Some(shared) => write!(f, "\n{shared}"),
                    None => Ok(()),
                }
            }
            Error::CheckTimedOut {
                timeout,
                output_tail,
                checks,
            } => {
                write!(f, "check timed out after {timeout:?}:\n{output_tail}")?;
                write_checks(f, checks)
            }
            Error::CheckNotStarted { reason, checks } => {
                write!(f, "check could not start: {reason}")?;
                write_checks(f, checks)
            }
            Error::DirtyTarget(worktree) => write!(
                f,
                "the target is checked out with uncommitted changes in {}",
                worktree.display()
            ),
            Error::AlreadyMerged { target } => {
                write!(f, "the candidate is already contained in {target}")
            }
            Error::InvalidCandidate(message) => write!(f, "invalid candidate: {message}"),
            Error::Denied(why) | Error::Usage(why) => f.write_str(why),
            Error::Running(name) => write!(f, "{name} is still running its turn"),
            Error::NotRunning(name) => write!(f, "{name} has no running turn to steer"),
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
            Error::Git(message) => f.write_str(message),
            Error::Harness(message) => write!(f, "harness: {message}"),
            Error::Io(error) => write!(f, "{error}"),
            Error::State(message) => f.write_str(message),
        }
    }
}

impl Error {
    /// This error, said as the reason a read or write of the store `could
    /// not` be done: `could not read the record for x: <why>`. A store that
    /// cannot be read is an error the caller returns, not an answer such as
    /// "no such branch" or "nothing recorded". An I/O or state error takes
    /// the sentence; one that already says something specific (an unknown
    /// branch, a lost lease) keeps its kind and message.
    pub(crate) fn could_not(self, doing: impl fmt::Display) -> Error {
        match self {
            Error::State(why) => Error::State(format!("could not {doing}: {why}")),
            Error::Io(why) => Error::State(format!("could not {doing}: {why}")),
            other => other,
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
            Error::UnknownMemory(_) => "unknown_memory",
            Error::InvalidMemory(_) => "invalid_memory",
            Error::NoPlan(_) => "no_plan",
            Error::InvalidName { .. } => "invalid_name",
            Error::UnknownHarness(_) => "unknown_harness",
            Error::HarnessUnavailable { .. } => "harness_unavailable",
            Error::Unsupported(_)
            | Error::UnroutedApprovals { .. }
            | Error::UnboundPolicy { .. } => "unsupported",
            Error::Config(_) => "config",
            Error::NoCandidate(_) => "no_candidate",
            Error::TargetMoved { .. } => "target_moved",
            Error::Conflict { .. } | Error::ConflictBetween { .. } => "conflict",
            Error::CheckFailed { .. } => "check_failed",
            Error::CheckTimedOut { .. } => "check_timed_out",
            Error::CheckNotStarted { .. } => "check_not_started",
            Error::DirtyTarget(_) => "dirty_target",
            Error::AlreadyMerged { .. } => "already_merged",
            Error::InvalidCandidate(_) => "invalid_candidate",
            Error::Denied(_) => "denied",
            Error::Usage(_) => "usage",
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

impl Error {
    /// What the error carries beyond its message, as JSON: `by --json`
    /// prints it as the error's `detail`, the broker passes it on, the MCP
    /// tools return it as the error's structured content, and the Python
    /// module raises it as `BranchyardError.detail`. A check of an
    /// integration that failed, timed out or could not start has its
    /// `checks` ([`IntegrationCheck`]) and, when a failed one left out
    /// siblings sharing it, the [`SharedCheck`]'s fields; a failed check
    /// also has its `status` (`exit 3`) and `command` (its argv) when the
    /// engine recorded them. `None` for an
    /// error with nothing more to say.
    pub fn detail(&self) -> Option<serde_json::Value> {
        match self {
            Error::CheckFailed {
                ran,
                checks,
                shared,
                own_work,
                ..
            } => {
                let mut detail = shared
                    .as_ref()
                    .and_then(|shared| serde_json::to_value(shared).ok())
                    .unwrap_or_else(|| serde_json::json!({}));
                if let Some(ran) = ran {
                    if !ran.status.is_empty() {
                        detail["status"] = serde_json::json!(ran.status);
                    }
                    if !ran.command.is_empty() {
                        detail["command"] = serde_json::json!(ran.command);
                    }
                }
                if !checks.is_empty() {
                    detail["checks"] = serde_json::to_value(checks).ok()?;
                }
                if !own_work.is_empty() {
                    detail["own_work"] = serde_json::to_value(own_work).ok()?;
                }
                detail
                    .as_object()
                    .is_some_and(|d| !d.is_empty())
                    .then_some(detail)
            }
            Error::CheckTimedOut { checks, .. } | Error::CheckNotStarted { checks, .. }
                if !checks.is_empty() =>
            {
                Some(serde_json::json!({ "checks": checks }))
            }
            Error::Remote { detail, .. } => detail.as_deref().cloned(),
            Error::HarnessUnavailable { harness, cause, .. } => Some(serde_json::json!({
                "harness": harness,
                "cause": cause.as_str(),
            })),
            Error::UnroutedApprovals { profile } => Some(serde_json::json!({
                "unsupported": "unrouted_approvals",
                "profile": profile,
            })),
            Error::UnboundPolicy {
                profile,
                reason,
                private_home_binds,
            } => Some(serde_json::json!({
                "unsupported": "unbound_policy",
                "profile": profile,
                "reason": reason,
                "private_home_binds": private_home_binds,
            })),
            _ => None,
        }
    }
}

/// Why a harness cannot be started ([`Error::HarnessUnavailable`]); in an
/// error's detail as `cause`, by [`Unavailable::as_str`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// Its executable was not found on `PATH`: it is not installed here.
    NotOnPath,
    /// The command names a file that is not an executable file.
    NotExecutable,
    /// The command to start it is empty.
    NoCommand,
    /// A fleet entry none of whose candidates can run (each one's reason
    /// is in the message).
    NoCandidate,
}

impl Unavailable {
    /// Every cause, for a caller that maps each one.
    pub const ALL: [Unavailable; 4] = [
        Unavailable::NotOnPath,
        Unavailable::NotExecutable,
        Unavailable::NoCommand,
        Unavailable::NoCandidate,
    ];

    /// Its stable name in an error's detail: `not_on_path`,
    /// `not_executable`, `no_command` or `no_candidate`.
    pub fn as_str(self) -> &'static str {
        match self {
            Unavailable::NotOnPath => "not_on_path",
            Unavailable::NotExecutable => "not_executable",
            Unavailable::NoCommand => "no_command",
            Unavailable::NoCandidate => "no_candidate",
        }
    }

    /// The cause named `name`, as [`Unavailable::as_str`] gives it.
    pub fn from_name(name: &str) -> Option<Unavailable> {
        Unavailable::ALL
            .into_iter()
            .find(|cause| cause.as_str() == name)
    }
}

/// A check that failed: its command, and how it ended
/// ([`Error::CheckFailed`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedCheck {
    /// The check, as its argv; empty when the engine did not record it.
    pub command: Vec<String>,
    /// How it ended, as `exit 3` or `signal 9`; empty when unknown.
    pub status: String,
}

/// After a failed check's output, which of an integration's checks ran,
/// on which branches, and how each ended.
fn write_checks(f: &mut fmt::Formatter<'_>, checks: &[IntegrationCheck]) -> fmt::Result {
    if checks.is_empty() {
        return Ok(());
    }
    let said: Vec<String> = checks.iter().map(ToString::to_string).collect();
    write!(f, "\nChecks on the merged result: {}", said.join("; "))
}

/// One check of the branches an integration merged, run once on the
/// result ([`MergedAll::check_results`], [`Error::CheckFailed`]).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationCheck {
    /// The check, as its argv.
    pub check: Vec<String>,
    /// The branches integrated whose check it is, in the order named.
    pub branches: Vec<String>,
    /// How it ended.
    pub outcome: CheckVerdict,
}

impl fmt::Display for IntegrationCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let outcome = match self.outcome {
            CheckVerdict::Passed => "passed",
            CheckVerdict::Failed => "failed",
            CheckVerdict::TimedOut => "timed out",
            CheckVerdict::NotStarted => "could not start",
            CheckVerdict::NotRun => "did not run",
        };
        write!(
            f,
            "`{}` ({}) {outcome}",
            self.check.join(" "),
            self.branches.join(", ")
        )
    }
}

/// How an [`IntegrationCheck`] ended. The checks run in order and stop at
/// the first that does not pass: those after it are `not_run`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckVerdict {
    /// The check exited successfully.
    Passed,
    /// The check exited unsuccessfully.
    Failed,
    /// The check ran past its timeout and was killed.
    TimedOut,
    /// The check could not be started.
    NotStarted,
    /// A check before it did not pass.
    NotRun,
}

/// A branch's check run on its current work, as integrating the branch
/// would run it, with nothing integrated; see [`Delegate::check`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckReport {
    /// The branch checked.
    pub branch: String,
    /// Its check, as its argv: its own, or the one it inherited. `None`
    /// when it has none: nothing ran, and integrating it runs none.
    pub check: Option<Vec<String>>,
    /// The parent whose check it is, when the branch inherited it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_from: Option<String>,
    /// Its current work as a commit: its worktree's files as they are,
    /// committed nowhere, or its git branch's head when it has no worktree.
    pub work: String,
    /// The git branch the work was merged into first, as integrating it
    /// would: its parent's. `None` for a branch without a parent, whose
    /// work was checked alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The commit the check ran on: the merge of `work` into `target`'s
    /// head, or `work` itself. `None` when the check did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<String>,
    /// How it ended; `not_run` when there is no check or the merge
    /// conflicted.
    pub outcome: CheckVerdict,
    /// Files where the work conflicts with `target`: integrating it would
    /// fail before its check.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    /// Paths the work changes outside the paths the branch owns
    /// ([`crate::scope`]): integrating it would be refused for them,
    /// whatever its check says. Empty for a branch without a scope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outside_scope: Vec<String>,
    /// The last [`branchyard_workspace::OUTPUT_TAIL_BYTES`] bytes of its
    /// output, or why it could not start.
    #[serde(default)]
    pub output_tail: String,
}

impl CheckReport {
    /// Whether integrating the work as it is would get past its check:
    /// nothing is outside its scope, and the check passed, or there is
    /// none and nothing conflicted.
    pub fn passed(&self) -> bool {
        self.outside_scope.is_empty()
            && match self.check {
                Some(_) => self.outcome == CheckVerdict::Passed,
                None => self.conflicts.is_empty(),
            }
    }
}

/// Siblings that share a check an integration failed, and the integration
/// that runs it on all of them ([`Error::CheckFailed`]).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SharedCheck {
    /// The check, as its argv.
    pub check: Vec<String>,
    /// The parent whose check it is, when the branches inherited it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_from: Option<String>,
    /// Siblings left out of the integration that share it and may still
    /// be integrated (ready, or not settled yet), oldest first.
    pub siblings: Vec<String>,
    /// Those of `siblings` that have not settled yet: wait for them first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsettled: Vec<String>,
    /// The branches to integrate together: those named, then `siblings`.
    pub integrate_together: Vec<String>,
    /// Those named whose last `by check` of the same work failed it alone
    /// too: why the siblings may be what it misses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_alone: Vec<String>,
}

impl fmt::Display for SharedCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whose = match &self.inherited_from {
            Some(parent) => format!("{parent}'s check, which they inherited"),
            None => "the same check".to_owned(),
        };
        if !self.failed_alone.is_empty() {
            write!(
                f,
                "Nothing was integrated. {} failed it alone too (`by check`), so it may need \
                 siblings' work. ",
                self.failed_alone.join(", ")
            )?;
        } else {
            write!(f, "Nothing was integrated. ")?;
        }
        write!(
            f,
            "Siblings {} share {whose}, which may pass only with all of them: if so, \
             integrate them together, `by integrate {}`",
            self.siblings.join(", "),
            self.integrate_together.join(" ")
        )?;
        if !self.unsettled.is_empty() {
            write!(
                f,
                ", once {} settle{} (`by wait {}`)",
                self.unsettled.join(", "),
                if self.unsettled.len() == 1 { "s" } else { "" },
                self.unsettled.join(" ")
            )?;
        }
        write!(
            f,
            ". A child that should land alone needs a check of its own (`by spawn --check`)"
        )
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
