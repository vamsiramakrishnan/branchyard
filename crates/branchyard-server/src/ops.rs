//! The operation registry: long operations admitted durably into a queue,
//! claimed and run by workers, and looked up by ID or idempotency key.
//!
//! Admission is a durable enqueue: the operation's record, its idempotency
//! binding, its branch locks and a queue row describing the work are
//! written in one transaction before the server answers `202 Accepted`
//! ([`OperationStore::admit`]). The work is a serializable description
//! (`crate::work::Work`), not a closure, so any worker on the database can
//! run it: this server's, another server's, or a `--worker` process.
//!
//! A dispatcher thread claims queued operations in scheduling order
//! ([`Scheduling`]), up to `max_running` at once plus one follow-up and one
//! control on slots of their own ([`Lane`]), each under a lease it renews
//! and a fence that every later write names. A worker records an operation
//! `running` before it runs it, and its outcome after, releasing the branch
//! locks in the same transaction. A claim whose lease expired, or whose
//! process is gone from this host, is claimed again by any worker: an
//! operation still `queued` then runs, and one already `running` is
//! recorded as `interrupted`, never run again, since its turn may have
//! started; the engine recovers that turn's branch as it does any whose
//! engine died.
//!
//! Each operation belongs to the tenant of the principal that admitted it,
//! recorded with it: only that tenant sees it, admission counts the
//! tenant's queued and running operations against its quotas in the same
//! transaction, and whichever worker runs it acts as that principal.
//!
//! Its idempotency key, scoped to the caller, maps every retry to the same
//! operation on any server sharing the store, and a retry never starts a
//! second run. Branch locks keep two operations from changing one branch
//! at once, which the SDK does not coordinate on its own.
//!
//! Operations run on their own threads, not on a request, so a client that
//! disconnects changes nothing (invariant 1). At shutdown, an operation
//! still running after the grace period is recorded as `interrupted`;
//! queued ones stay queued for the next worker, here after a restart or on
//! another server.

use branchyard_support::time::now_ms;
use branchyard_support::{CondvarExt as _, LockExt as _};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use branchyard_client::api::{
    ErrorBody, Operation, OperationKind, OperationResult, OperationState, Problem, ProblemKind,
};
use serde_json::Value;

use crate::config::Principal;
use crate::error::ApiError;
use crate::metrics;
use crate::observe::Observability;
use crate::problems::{Found, Scope};
use crate::store::{
    Admission, AdmissionQuota, Claim, Idempotency, Lane, OperationStore, Projection, Scheduling,
    StoredOperation, Worker,
};

/// What a finished operation reports.
pub struct Finished {
    /// Additional configured values resolved by the executing worker, kept
    /// with the operation before its outcome is recorded.
    pub projection: Projection,
    /// Native task identities observed for the operation's created branches.
    pub projection_tasks: BTreeMap<String, String>,
    pub result: Result<OperationResult, ErrorBody>,
    /// The feed head once the operation's activity was ingested.
    pub end_cursor: Option<u64>,
}

/// Runs a claimed operation's work description, as the principal its
/// record names (`StoredOperation::principal`), never as the worker.
pub trait Executor: Send + Sync {
    /// Resolve additional configured values this worker will use, without
    /// execution effects. Recorded with the running operation before any
    /// branch diagnostics can become visible. Empty by default.
    fn projection(
        &self,
        _operation: &StoredOperation,
        _work: &Value,
    ) -> Result<Projection, ApiError> {
        Ok(Projection::default())
    }
    fn execute(&self, operation: &StoredOperation, work: &Value) -> Finished;
    /// Stop `operation`'s running turns for a shutdown, through the same
    /// durable cancel `by cancel` records, so they end with a known
    /// outcome instead of dying with the process. Nothing by default.
    fn stop_for_shutdown(&self, _operation: &StoredOperation) {}
}

pub struct NewOperation {
    pub repo: String,
    pub kind: OperationKind,
    pub branches: Vec<String>,
    pub cursor: u64,
    /// Branches of `repo` to lock until the operation finishes.
    pub locks: Vec<String>,
    pub idempotency: Option<Idempotency>,
    /// The admitting principal, as its credential verified. Its tenant
    /// owns the operation: `GET /v1/operations/{id}` hides it from every
    /// other tenant, and the worker that runs it acts as this principal.
    pub principal: Principal,
    /// Branches of `repo` the operation will create, as planned.
    pub creates: Vec<String>,
    /// The tenant's ceilings, checked in the admission's transaction.
    pub quota: AdmissionQuota,
    /// Worker labels the operation needs (`require_labels`).
    pub requires: Vec<String>,
    /// Its priority, checked and capped (`docs/server.md#scheduling`).
    pub priority: i32,
    /// The request's `traceparent` header, which its trace continues.
    pub trace: Option<String>,
    /// Which capacity a worker claims it from.
    pub lane: Lane,
}

/// How a registry dispatches.
#[derive(Clone, Debug)]
pub struct Options {
    /// Normal operations this process runs at once; more wait queued. One
    /// follow-up and one control may run beyond it, each on a slot of its
    /// own ([`Lane`]).
    pub max_running: usize,
    /// How long a claim lasts without renewal. Renewed every third of it.
    pub lease: Duration,
    /// How often an idle dispatcher looks for work other processes queued.
    pub poll: Duration,
    /// The repositories this process can run operations of.
    pub repos: Vec<String>,
    /// No other process uses the store (a data directory's SQLite, held
    /// by its lock): every claim in it is a predecessor's, released at
    /// open.
    pub exclusive: bool,
    /// The labels this process's worker carries: it claims only operations
    /// whose required labels are all among them.
    pub labels: Vec<String>,
    /// How long an operation may wait queued before, when no live worker
    /// carries the labels it requires, it reports why (`waiting`).
    pub unclaimable_after: Duration,
    /// How claims choose among queued operations: priority, aging and the
    /// tenants' fair shares.
    pub scheduling: Scheduling,
    /// Where claims, renewals and finished operations are counted and
    /// traced.
    pub observability: Observability,
    /// What this worker's machine has of each harness
    /// (docs/harness-lifecycle.md), detected now and every
    /// [`INVENTORY_EVERY`] on a thread of its own, advertised with each
    /// beat; its `harness:<id>` labels are added to [`Options::labels`].
    /// `None` advertises nothing.
    pub inventory: Option<InventorySource>,
}

/// Detects this machine's harness inventory; `None` when it cannot.
#[derive(Clone)]
pub struct InventorySource(
    pub Arc<dyn Fn() -> Option<branchyard::inventory::Inventory> + Send + Sync>,
);

impl std::fmt::Debug for InventorySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InventorySource")
    }
}

impl InventorySource {
    /// Detection on this machine with `/bin/sh` and the default options
    /// (`BRANCHYARD_HARNESS_DIRS` read), without usage meters.
    pub fn local() -> InventorySource {
        InventorySource(Arc::new(|| {
            branchyard::inventory::detect_local(&branchyard::inventory::DetectOptions::from_env())
                .map_err(|e| tracing::warn!(error = %e, "could not detect the harnesses here"))
                .ok()
        }))
    }
}

impl Options {
    pub fn new(repos: Vec<String>) -> Options {
        Options {
            max_running: 8,
            lease: DEFAULT_LEASE,
            poll: Duration::from_millis(250),
            repos,
            exclusive: true,
            labels: Vec::new(),
            unclaimable_after: DEFAULT_UNCLAIMABLE_AFTER,
            scheduling: Scheduling::default(),
            observability: Observability::default(),
            inventory: None,
        }
    }
}

static INVENTORY_SOURCE: std::sync::OnceLock<InventorySource> = std::sync::OnceLock::new();

/// Detect inventories with `source` in every registry this process opens
/// from a configuration: how `by serve` and `by worker` add the usage
/// meters `by usage` reads. Only the first call counts.
pub fn set_inventory_source(source: InventorySource) {
    branchyard_support::best_effort(
        "keep the first inventory source",
        INVENTORY_SOURCE
            .set(source)
            .map_err(|_| "an inventory source was already set"),
    );
}

/// The inventory source set with [`set_inventory_source`], else
/// [`InventorySource::local`].
pub fn inventory_source() -> InventorySource {
    INVENTORY_SOURCE
        .get()
        .cloned()
        .unwrap_or_else(InventorySource::local)
}

/// How often a worker detects its harnesses again.
pub const INVENTORY_EVERY: Duration = Duration::from_secs(300);

/// How long an operation waits before an unclaimable one says why.
pub const DEFAULT_UNCLAIMABLE_AFTER: Duration = Duration::from_secs(60);
/// How often a dispatcher records that its worker is alive, with its
/// labels; a worker unseen for three beats is not counted as live.
pub const BEAT: Duration = Duration::from_secs(5);

/// How long a claim lasts without renewal, by default: as long as an
/// engine's lease on a turn.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(30);
/// How long a branch held for a synchronous change stays held if its
/// holder never releases it, such as a server that died mid-removal.
const HOLD_TTL: Duration = Duration::from_secs(600);

pub struct Registry {
    store: Box<dyn OperationStore>,
    options: Options,
    worker: Worker,
    state: Mutex<State>,
    changed: Condvar,
    /// This worker's harness inventory, once detected.
    inventory: Mutex<Option<branchyard::inventory::Inventory>>,
}

struct State {
    /// Operations this process runs, by ID, with their claim's fence and
    /// the capacity it was claimed from.
    running: HashMap<String, (i64, Lane)>,
    /// Set by [`Registry::start`]; cleared at close.
    executor: Option<Arc<dyn Executor>>,
    accepting: bool,
    /// Shut down: nothing more is recorded.
    closed: bool,
    /// Something was admitted here since the dispatcher last looked.
    admitted: bool,
    /// How many operations this process has finished running: what a
    /// [`Registry::wait_for_tenant`] that read the store before one
    /// finished sees moved.
    finished: u64,
}

fn interrupted(message: &str) -> ErrorBody {
    ErrorBody {
        code: "interrupted".into(),
        message: message.into(),
        detail: None,
    }
}

pub const STOPPED: &str = "the server stopped before this operation finished; \
     a turn it left running is recovered as interrupted when its repository is next opened";
/// How long after its last beat a worker counts as live: what
/// [`Registry::live_workers`] reads, and a worker's lease as a service.
pub fn live_window() -> Duration {
    BEAT * 3
}

pub const WORKER_LOST: &str = "the worker running this operation stopped before it \
     finished; a turn it left running is recovered as interrupted, and nothing is run again";

impl Registry {
    /// Open the registry on `store`. An unfinished operation left without a
    /// queue row by an earlier version of the server is recorded as
    /// interrupted; with [`Options::exclusive`], every claim is released.
    /// Nothing runs until [`Registry::start`].
    pub fn open(store: Box<dyn OperationStore>, options: Options) -> io::Result<Arc<Registry>> {
        if options.exclusive {
            store.reset()?;
        }
        for mut stored in store.orphans()? {
            stored.operation.state = OperationState::Interrupted;
            stored.operation.error = Some(interrupted(STOPPED));
            stored.operation.finished_at_ms = Some(now_ms());
            store.save(&stored)?;
        }
        Ok(Arc::new(Registry {
            store,
            worker: Worker::current(),
            options: Options {
                max_running: options.max_running.max(1),
                ..options
            },
            state: Mutex::new(State {
                running: HashMap::new(),
                executor: None,
                accepting: true,
                closed: false,
                admitted: false,
                finished: 0,
            }),
            changed: Condvar::new(),
            inventory: Mutex::new(None),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock_recovering("state")
    }

    /// This registry's worker identity, as its claims record it.
    pub fn worker(&self) -> &Worker {
        &self.worker
    }

    /// Where this registry counts and traces.
    pub fn observability(&self) -> &Observability {
        &self.options.observability
    }

    /// The shared queue as the store holds it, for metrics.
    pub fn queue(&self) -> io::Result<Vec<crate::store::Queued>> {
        self.store.queue()
    }

    /// Workers seen alive recently, for metrics.
    pub fn live_workers(&self) -> io::Result<Vec<crate::store::LiveWorker>> {
        self.store.workers(BEAT * 3)
    }

    /// The fleet's service registry, in the operation store's database.
    pub fn services(&self) -> &dyn branchyard::services::ServiceStore {
        self.store.services()
    }

    /// Read console metadata. Authorization remains the caller's responsibility.
    pub fn console_get(
        &self,
        collection: crate::console_store::Collection,
        id: &str,
    ) -> Result<Option<crate::console_store::Record>, ApiError> {
        self.store
            .console_get(collection, id)
            .map_err(console_error)
    }

    /// List one tenant's console metadata.
    pub fn console_list(
        &self,
        collection: crate::console_store::Collection,
        tenant: &str,
    ) -> Result<Vec<crate::console_store::Record>, ApiError> {
        self.store
            .console_list(collection, tenant)
            .map_err(console_error)
    }

    /// Internal ownership lookup across tenants, for legacy resource guards only.
    pub(crate) fn console_list_all(
        &self,
        collection: crate::console_store::Collection,
    ) -> Result<Vec<crate::console_store::Record>, ApiError> {
        self.store
            .console_list_all(collection)
            .map_err(console_error)
    }

    /// Commit a revision-checked metadata batch.
    pub fn console_mutate(
        &self,
        mutations: &[crate::console_store::Mutation],
    ) -> Result<(), ApiError> {
        self.store.console_mutate(mutations).map_err(console_error)
    }

    /// This worker's harness inventory, once detected.
    pub fn inventory(&self) -> Option<branchyard::inventory::Inventory> {
        self.inventory.lock_recovering("inventory").clone()
    }

    /// The labels this worker claims with: its host/boot, configured ones
    /// and those its inventory derives (`harness:<id>`).
    pub fn labels(&self) -> Vec<String> {
        let mut labels = self.options.labels.clone();
        labels.push(format!("worker-host:{}", self.worker.host));
        if let Some(inventory) = self.inventory() {
            labels.extend(inventory.labels());
        }
        labels.sort();
        labels.dedup();
        labels
    }

    /// The labels an operation running `harnesses` by name on `repo` should
    /// require, from the inventories live workers advertise; see
    /// [`crate::store::harness_requirement`].
    pub fn harness_requirement(&self, repo: &str, harnesses: &[String]) -> Vec<String> {
        match self.store.workers(BEAT * 3) {
            Ok(workers) => crate::store::harness_requirement(harnesses, repo, &workers),
            Err(e) => {
                tracing::warn!(error = %e, "could not read the live workers");
                Vec::new()
            }
        }
    }

    /// Detect the inventory now and every [`INVENTORY_EVERY`] until the
    /// registry stops accepting work.
    fn detect_inventory(self: Arc<Self>, source: InventorySource) {
        loop {
            let found = (source.0)();
            if found.is_some() {
                *self.inventory.lock_recovering("inventory") = found;
                // Beat with it now rather than at the next beat.
                self.changed.notify_all();
            }
            let deadline = Instant::now() + INVENTORY_EVERY;
            let mut state = self.lock();
            loop {
                if state.closed || !state.accepting {
                    return;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                state = self
                    .changed
                    .wait_timeout_recovering(state, left, "changed")
                    .0;
            }
        }
    }

    /// The priority a spawn from `branch` of `repo` inherits: that of the
    /// latest operation that worked on it, else 0.
    pub fn branch_priority(&self, repo: &str, branch: &str) -> Result<i32, ApiError> {
        self.store
            .branch_priority(repo, branch)
            .map(|p| p.unwrap_or(0))
            .map_err(|e| ApiError::internal(format!("could not read the operations: {e}")))
    }

    /// Start dispatching: claim queued operations and run them with
    /// `executor`.
    pub fn start(self: &Arc<Self>, executor: Arc<dyn Executor>) -> io::Result<()> {
        self.lock().executor = Some(executor);
        if let Some(source) = self.options.inventory.clone() {
            let registry = self.clone();
            std::thread::Builder::new()
                .name("branchyard-inventory".into())
                .spawn(move || registry.detect_inventory(source))?;
        }
        let registry = self.clone();
        std::thread::Builder::new()
            .name("branchyard-dispatch".into())
            .spawn(move || registry.dispatch())?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<Operation>, ApiError> {
        self.store
            .get(id)
            .map(|found| found.map(|s| self.describe(s.operation)))
            .map_err(|e| ApiError::internal(format!("could not read the operation: {e}")))
    }

    /// The private projection snapshot of an operation on `repo`. Callers
    /// authorize the operation before asking for its projection; a missing
    /// record or a record on another repository contributes no values.
    pub(crate) fn operation_projection(
        &self,
        id: &str,
        repo: &str,
    ) -> Result<Projection, ApiError> {
        self.store
            .get(id)
            .map(|found| match found {
                Some(stored) if stored.operation.repo == repo => stored.projection,
                _ => Projection::default(),
            })
            .map_err(|e| {
                ApiError::internal(format!("could not read the operation projection: {e}"))
            })
    }

    /// The direct task projection for the current branch incarnation. A
    /// terminal operation must record the current native task identity; a
    /// queued or running creation owns the branch name under its queue lock
    /// until its worker captures that identity.
    pub(crate) fn branch_projection(
        &self,
        repo: &str,
        branch: &str,
        task_id: &str,
    ) -> Result<Projection, ApiError> {
        let candidates = self.store.creating(repo, branch).map_err(|error| {
            ApiError::internal(format!("could not read the branch projection: {error}"))
        })?;
        for stored in candidates.into_iter().rev() {
            let matches = match stored.projection_tasks.get(branch) {
                Some(recorded) => recorded == task_id,
                None if stored.operation.state.is_terminal() && !stored.projection.is_empty() => {
                    return Err(ApiError::internal(
                        "could not verify the branch's preset projection",
                    ));
                }
                None => !stored.operation.state.is_terminal(),
            };
            if matches {
                return Ok(stored.projection);
            }
        }
        Ok(Projection::default())
    }

    /// Preserve a removed branch's diagnostic boundary during a read/remove
    /// race. Without a current native task identity, only hidden values are
    /// combined; this grants no resource access.
    pub(crate) fn removed_branch_projection(
        &self,
        repo: &str,
        branch: &str,
    ) -> Result<Projection, ApiError> {
        let operations = self.store.creating(repo, branch).map_err(|error| {
            ApiError::internal(format!(
                "could not read the removed branch projection: {error}"
            ))
        })?;
        let mut projection = Projection::default();
        for operation in operations {
            projection.extend(operation.projection);
        }
        Ok(projection)
    }

    /// Preset snapshots whose recorded feed interval contains `seq`, scoped
    /// to `repo`. The caller verifies branch ownership against these records
    /// or the captured native tasks' durable attempt history before use.
    pub(crate) fn feed_projection_candidates(
        &self,
        repo: &str,
        seq: u64,
    ) -> Result<Vec<StoredOperation>, ApiError> {
        self.store
            .feed_operations(repo, None, seq)
            .map_err(|error| {
                ApiError::internal(format!("could not read the feed projections: {error}"))
            })
    }

    /// The durable projection for one historical feed entry, scoped to its
    /// repository, branch and admitted-to-finished sequence interval.
    pub(crate) fn feed_projection(
        &self,
        repo: &str,
        branch: &str,
        seq: u64,
    ) -> Result<Projection, ApiError> {
        let operations = self.feed_projection_candidates(repo, seq)?;
        let mut projection = Projection::default();
        for operation in operations {
            let owned = operation
                .operation
                .branches
                .iter()
                .any(|name| name == branch)
                || operation.creates.iter().any(|name| name == branch)
                || operation.projection_tasks.contains_key(branch)
                || operation.operation.result.as_ref().is_some_and(|result| {
                    result
                        .branches
                        .iter()
                        .chain(&result.descendants)
                        .any(|recorded| recorded.name == branch)
                });
            if !owned {
                continue;
            }
            if operation.operation.state.is_terminal() && operation.operation.end_cursor.is_none() {
                return Err(ApiError::internal(
                    "could not verify the feed's preset projection interval",
                ));
            }
            projection.extend(operation.projection);
        }
        Ok(projection)
    }

    /// `operation`, saying why it waits when it has waited longer than
    /// [`Options::unclaimable_after`] queued and no live worker serving its
    /// repository carries every label it requires.
    pub fn describe(&self, mut operation: Operation) -> Operation {
        let waited = now_ms().saturating_sub(operation.created_at_ms);
        if operation.state == OperationState::Queued
            && !operation.requires.is_empty()
            && waited >= self.options.unclaimable_after.as_millis() as u64
        {
            if let Ok(workers) = self.store.workers(BEAT * 3) {
                operation.waiting =
                    crate::store::unclaimable(&operation.requires, &operation.repo, &workers);
            }
        }
        operation
    }

    /// Every tenant's queued operations that [`Registry::describe`] says
    /// wait for a worker: unclaimed, requiring labels no live worker
    /// serving their repository carries, for at least
    /// [`Options::unclaimable_after`] since admission. For
    /// `GET /v1/problems`, each seen by its tenant on its repository.
    pub(crate) fn problems(&self) -> io::Result<Vec<Found>> {
        let now = now_ms();
        let after = self.options.unclaimable_after.as_millis() as u64;
        let admitted = |q: &crate::store::Queued| u64::try_from(q.enqueued_ms).unwrap_or(0);
        let waiting: Vec<crate::store::Queued> = self
            .store
            .queue()?
            .into_iter()
            .filter(|q| {
                !q.claimed && !q.requires.is_empty() && now.saturating_sub(admitted(q)) >= after
            })
            .collect();
        if waiting.is_empty() {
            return Ok(Vec::new());
        }
        let workers = self.store.workers(BEAT * 3)?;
        Ok(waiting
            .into_iter()
            .filter_map(|q| {
                let error = crate::store::unclaimable(&q.requires, &q.repo, &workers)?;
                let labels: Vec<String> = q.requires.iter().map(|l| format!("--label {l}")).collect();
                Some(Found {
                    problem: Problem {
                        kind: ProblemKind::Operation,
                        what: format!("operation {} on {}", q.id, q.repo),
                        since_ms: admitted(&q),
                        failures: None,
                        error,
                        next: format!(
                            "start `by worker` or `by serve` for {} with `{}`; it claims the operation",
                            q.repo,
                            labels.join(" ")
                        ),
                        key: q.id,
                    },
                    scope: Scope::Tenant {
                        tenant: q.tenant,
                        repo: Some(q.repo),
                    },
                })
            })
            .collect())
    }

    /// `id`'s operation, only when it belongs to `tenant`: an operation of
    /// another tenant reads as absent, exactly like an unknown ID, so a
    /// principal cannot distinguish another tenant's operation from one
    /// that never existed.
    pub fn get_for_tenant(&self, id: &str, tenant: &str) -> Result<Option<Operation>, ApiError> {
        let found = self
            .store
            .get(id)
            .map_err(|e| ApiError::internal(format!("could not read the operation: {e}")))?;
        Ok(found
            .filter(|stored| stored.tenant() == tenant)
            .map(|stored| self.describe(stored.operation)))
    }

    /// `id`'s operation for `tenant`, as [`Registry::get_for_tenant`]
    /// reads it, once it is terminal or `timeout` has passed, whichever is
    /// first; at shutdown, as it is then. An operation this process runs
    /// wakes the wait as it finishes; one another worker on the store runs
    /// is seen within [`Options::poll`], as the dispatcher sees their work.
    pub fn wait_for_tenant(
        &self,
        id: &str,
        tenant: &str,
        timeout: Duration,
    ) -> Result<Option<Operation>, ApiError> {
        let deadline = Instant::now() + timeout;
        loop {
            let seen = self.lock().finished;
            let Some(operation) = self.get_for_tenant(id, tenant)? else {
                return Ok(None);
            };
            let now = Instant::now();
            if operation.state.is_terminal() || now >= deadline {
                return Ok(Some(operation));
            }
            let state = self.lock();
            if state.closed || !state.accepting {
                return Ok(Some(operation));
            }
            if state.finished == seen {
                let left = (deadline - now).min(self.options.poll);
                drop(self.changed.wait_timeout_recovering(state, left, "changed"));
            }
        }
    }

    /// `tenant`'s queued and running operations, as the store holds them.
    pub fn unfinished(&self, tenant: &str) -> Result<Vec<StoredOperation>, ApiError> {
        self.store
            .unfinished(tenant)
            .map_err(|e| ApiError::internal(format!("could not read the operations: {e}")))
    }

    /// Whether a queued or running operation of `repo`, of any tenant, runs
    /// a turn of `branch` or creates it: a send, a task, a spawn. Until a
    /// worker starts it the branch's status is still its last turn's, so a
    /// wait for the branch waits for the operation too. A merge, an
    /// integration or a check runs no turn of it and is not counted.
    pub fn turn_queued(&self, repo: &str, branch: &str) -> Result<bool, ApiError> {
        let read = |e: io::Error| ApiError::internal(format!("could not read the queue: {e}"));
        for queued in self.store.queue().map_err(read)? {
            if queued.repo != repo {
                continue;
            }
            let Some(stored) = self.store.get(&queued.id).map_err(read)? else {
                continue;
            };
            let operation = &stored.operation;
            if !matches!(
                operation.kind,
                OperationKind::Merge | OperationKind::Integrate | OperationKind::Check
            ) && operation.branches.iter().any(|b| b == branch)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The operation an earlier request of `tenant` with this key created,
    /// if any.
    pub fn replay(&self, idem: &Idempotency, tenant: &str) -> Result<Option<Operation>, ApiError> {
        let found = self
            .store
            .by_key(&idem.caller, &idem.key)
            .map_err(|e| ApiError::internal(format!("could not read the operation: {e}")))?;
        found
            .map(|stored| same_request(stored, idem, tenant))
            .transpose()
    }

    /// The operation `caller` of `tenant` created with `key`, whatever the
    /// request; another tenant's reads as absent.
    pub fn by_key(
        &self,
        caller: &str,
        key: &str,
        tenant: &str,
    ) -> Result<Option<Operation>, ApiError> {
        self.store
            .by_key(caller, key)
            .map(|found| {
                found
                    .filter(|stored| stored.tenant() == tenant)
                    .map(|s| self.describe(s.operation))
            })
            .map_err(|e| ApiError::internal(format!("could not read the operation: {e}")))
    }

    /// Admit a new operation durably, with `work` describing what to run,
    /// or return the one an earlier request with the same key created
    /// (`true`).
    pub fn submit(&self, new: NewOperation, work: Value) -> Result<(Operation, bool), ApiError> {
        self.submit_console(new, work, &[])
    }

    /// Admit a direct task with its tenant-resolved projection kept in the
    /// same transaction as its operation and queue row.
    pub(crate) fn submit_projected(
        &self,
        new: NewOperation,
        work: Value,
        projection: Projection,
    ) -> Result<(Operation, bool), ApiError> {
        self.submit_with_projection(new, work, &[], projection)
    }

    /// Admit console metadata and work atomically through the existing queue.
    pub fn submit_console(
        &self,
        new: NewOperation,
        work: Value,
        mutations: &[crate::console_store::Mutation],
    ) -> Result<(Operation, bool), ApiError> {
        self.submit_with_projection(new, work, mutations, Projection::default())
    }

    #[allow(clippy::expect_used)] // ratchet: branchyard-server
    fn submit_with_projection(
        &self,
        new: NewOperation,
        work: Value,
        mutations: &[crate::console_store::Mutation],
        projection: Projection,
    ) -> Result<(Operation, bool), ApiError> {
        if let Some(idem) = &new.idempotency {
            if let Some(existing) = self.replay(idem, &new.principal.tenant)? {
                return Ok((existing, true));
            }
        }
        if !self.lock().accepting {
            return Err(ApiError::shutting_down());
        }
        let id = format!("op_{}", &branchyard_client::new_key()[..24]);
        let observability = &self.options.observability;
        // The admission span: live for the transaction below, a child of
        // the request's `traceparent` when it sent a valid one.
        let admission_span = crate::telemetry::admission(
            &crate::telemetry::Admission {
                operation: &id,
                repo: &new.repo,
                kind: &kind_name(new.kind),
                tenant: &new.principal.tenant,
                priority: new.priority,
            },
            new.trace.as_deref(),
        );
        let _admitting = admission_span.enter();
        let stored = StoredOperation {
            projection,
            projection_tasks: BTreeMap::new(),
            operation: Operation {
                id: id.clone(),
                repo: new.repo,
                kind: new.kind,
                state: OperationState::Queued,
                branches: new.branches,
                cursor: new.cursor,
                end_cursor: None,
                created_at_ms: now_ms(),
                finished_at_ms: None,
                result: None,
                error: None,
                requires: new.requires,
                waiting: None,
                priority: new.priority,
            },
            idempotency: new.idempotency,
            locks: new.locks,
            tenant: new.principal.tenant.clone(),
            principal: Some(new.principal),
            creates: new.creates,
            trace: crate::telemetry::traceparent(&admission_span, new.trace.as_deref()),
            lane: new.lane,
        };
        let mut mutations = mutations.to_vec();
        if work.get("kind").and_then(Value::as_str) == Some("console") {
            let workspace_id = work
                .get("command")
                .and_then(|command| command.get("workspace_id"))
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::bad_request("console work requires workspace_id"))?;
            let command = &work["command"];
            for mutation in &mut mutations {
                let crate::console_store::Mutation::Put { record, expected } = mutation else {
                    continue;
                };
                let operation_field = match record.collection {
                    crate::console_store::Collection::Sessions
                        if command.get("action").and_then(Value::as_str) == Some("control")
                            && command.get("control").and_then(Value::as_str) == Some("cancel")
                            && command.get("session_id").and_then(Value::as_str)
                                == Some(record.id.as_str())
                            && record.body.get("workspace_id").and_then(Value::as_str)
                                == Some(workspace_id) =>
                    {
                        Some("_cancel_operation_id")
                    }
                    crate::console_store::Collection::Sessions
                        if expected.is_none()
                            && command.get("session_id").and_then(Value::as_str)
                                == Some(record.id.as_str())
                            && record.body.get("workspace_id").and_then(Value::as_str)
                                == Some(workspace_id) =>
                    {
                        Some("operation_id")
                    }
                    crate::console_store::Collection::Workspaces
                        if matches!(
                            command.get("action").and_then(Value::as_str),
                            Some("initialize" | "lifecycle")
                        ) && record.id == workspace_id =>
                    {
                        Some("_operation_id")
                    }
                    _ => None,
                };
                if let Some(field) = operation_field {
                    record
                        .body
                        .as_object_mut()
                        .ok_or_else(|| ApiError::bad_request("console metadata must be an object"))?
                        .insert(field.into(), Value::String(stored.operation.id.clone()));
                }
            }
            mutations.push(crate::console_store::Mutation::Put {
                record: crate::console_store::Record {
                    collection: crate::console_store::Collection::Bindings,
                    id: format!("operation:{}", stored.operation.id),
                    tenant: stored.tenant().into(),
                    revision: 1,
                    body: serde_json::json!({"workspace_id":workspace_id,"operation_id":stored.operation.id,"repo":stored.operation.repo}),
                },
                expected: None,
            });
        }
        let admitted = self
            .store
            .admit_console(&stored, &work, &new.quota, &mutations)
            .map_err(|e| {
                crate::telemetry::fail(&admission_span, &e.to_string());
                console_error(e)
            })?;
        if admitted != Admission::Admitted {
            crate::telemetry::attr(&admission_span, "by.admitted", false);
        }
        match admitted {
            Admission::Admitted => {
                observability.metrics.inc(
                    metrics::ADMITTED,
                    &[
                        ("kind", &kind_name(stored.operation.kind)),
                        ("tenant", stored.tenant()),
                    ],
                );
                let mut state = self.lock();
                state.admitted = true;
                self.changed.notify_all();
                Ok((stored.operation, false))
            }
            Admission::Replayed(existing) => {
                let idem = stored.idempotency.as_ref().expect("replayed by its key");
                Ok((same_request(*existing, idem, stored.tenant())?, true))
            }
            Admission::Busy { branch, holder } => Err(busy(&branch, &holder)),
            Admission::Quota {
                limit,
                max,
                reserved,
            } => Err(quota_exceeded(limit, stored.tenant(), max, reserved)),
        }
    }

    /// Claim and run queued operations until shutdown, renewing the
    /// claims of those running.
    fn dispatch(self: Arc<Self>) {
        let mut renewed = Instant::now();
        let renew_every = self.options.lease / 3;
        let mut beaten: Option<Instant> = None;
        let mut advertised: Option<u64> = None;
        loop {
            let inventory = self.inventory();
            let detected = inventory.as_ref().map(|i| i.detected_at_ms);
            if beaten.is_none_or(|at| at.elapsed() >= BEAT) || detected != advertised {
                if let Err(e) = self.store.beat(
                    &self.worker,
                    &self.labels(),
                    &self.options.repos,
                    inventory.as_ref(),
                ) {
                    tracing::warn!(error = %e, "could not record this worker as alive");
                }
                beaten = Some(Instant::now());
                advertised = detected;
            }
            let (control_free, lane, executor) = {
                let mut state = self.lock();
                if state.closed || !state.accepting {
                    return;
                }
                state.admitted = false;
                let free = |lane: Lane, slots: usize| {
                    state
                        .running
                        .values()
                        .filter(|(_, held)| *held == lane)
                        .count()
                        < slots
                };
                // A normal claim takes follow-ups too, ahead of the rest, so
                // the follow-up slot is only claimed from once normal is full.
                let lane = if free(Lane::Normal, self.options.max_running) {
                    Some(Lane::Normal)
                } else if free(Lane::FollowUp, 1) {
                    Some(Lane::FollowUp)
                } else {
                    None
                };
                (free(Lane::Control, 1), lane, state.executor.clone())
            };
            let Some(executor) = executor else { return };
            if renewed.elapsed() >= renew_every {
                self.renew();
                renewed = Instant::now();
            }
            if control_free {
                if let Some(claim) = self.claim_from(Lane::Control) {
                    self.claimed(&claim);
                    self.run(claim, Lane::Control, executor.clone());
                }
            }
            if let Some(lane) = lane {
                if let Some(claim) = self.claim_from(lane) {
                    self.claimed(&claim);
                    self.run(claim, lane, executor);
                    continue;
                }
            }
            let wait = self.options.poll.min(renew_every);
            let state = self.lock();
            if !state.admitted || (!control_free && lane.is_none()) {
                drop(self.changed.wait_timeout_recovering(state, wait, "changed"));
            }
        }
    }

    /// Claim the next operation `lane`'s capacity takes, if any.
    fn claim_from(&self, lane: Lane) -> Option<Claim> {
        self.store
            .claim_from(
                &self.worker,
                &self.options.repos,
                &self.labels(),
                self.options.lease,
                &self.options.scheduling,
                lane,
            )
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, lane = lane.as_str(), "could not claim an operation");
                None
            })
    }

    /// Count a claim, and trace it as a child of its admission.
    fn claimed(&self, claim: &Claim) {
        let observability = &self.options.observability;
        let op = &claim.operation.operation;
        let priority = op.priority.to_string();
        let metrics = &observability.metrics;
        metrics.inc(
            metrics::CLAIMS,
            &[
                ("tenant", claim.operation.tenant()),
                ("priority", &priority),
            ],
        );
        let waited = now_ms().saturating_sub(op.created_at_ms);
        metrics.observe(
            metrics::CLAIM_WAIT,
            &[("priority", &priority)],
            waited as f64 / 1000.0,
        );
        if claim.took_over {
            metrics.inc(metrics::EXPIRIES, &[]);
        }
        if let Some(admission) = claim.operation.trace.as_deref() {
            // The claim's span is the operation's wait in the queue: from
            // its admission to this claim.
            crate::telemetry::claim(
                &crate::telemetry::Claimed {
                    operation: &op.id,
                    worker: &self.worker.id,
                    fence: claim.fence,
                    took_over: claim.took_over,
                    priority: op.priority,
                    admitted_ms: op.created_at_ms,
                    claimed_ms: now_ms(),
                },
                admission,
            );
        }
    }

    /// Extend the lease of every operation running here.
    fn renew(&self) {
        let running: Vec<(String, i64)> = self
            .lock()
            .running
            .iter()
            .map(|(id, (fence, _))| (id.clone(), *fence))
            .collect();
        for (id, fence) in running {
            let metrics = &self.options.observability.metrics;
            match self
                .store
                .renew(&self.worker, &id, fence, self.options.lease)
            {
                Ok(true) => metrics.inc(metrics::RENEWALS, &[("result", "renewed")]),
                Ok(false) => {
                    metrics.inc(metrics::RENEWALS, &[("result", "lost")]);
                    tracing::warn!(%id, "lost the claim on an operation; another worker took it over")
                }
                Err(e) => {
                    metrics.inc(metrics::RENEWALS, &[("result", "error")]);
                    tracing::error!(%id, error = %e, "could not renew the claim on an operation")
                }
            }
        }
    }

    /// Run an operation claimed from `lane`'s capacity on a thread of its
    /// own.
    fn run(self: &Arc<Self>, claim: Claim, lane: Lane, executor: Arc<dyn Executor>) {
        let id = claim.operation.operation.id.clone();
        {
            let mut state = self.lock();
            if state.closed || !state.accepting {
                drop(state);
                branchyard_support::best_effort(
                    "release the operation's branch lock",
                    self.store.release(&self.worker, &id, claim.fence),
                );
                return;
            }
            state.running.insert(id.clone(), (claim.fence, lane));
        }
        let registry = self.clone();
        let fence = claim.fence;
        let spawned = std::thread::Builder::new()
            .name(format!("branchyard-{id}"))
            .spawn(move || registry.work(claim, executor));
        if let Err(error) = spawned {
            tracing::error!(%id, %error, "could not start a worker thread for an operation");
            branchyard_support::best_effort(
                "release the operation's branch lock",
                self.store.release(&self.worker, &id, fence),
            );
            self.done(&id);
        }
    }

    fn work(&self, claim: Claim, executor: Arc<dyn Executor>) {
        let Claim {
            operation: mut stored,
            work,
            fence,
            ..
        } = claim;
        let id = stored.operation.id.clone();
        if stored.operation.state != OperationState::Queued {
            // Claimed before, and recorded running: its turn may have
            // started, so it is never run again.
            stored.operation.state = OperationState::Interrupted;
            stored.operation.error = Some(interrupted(WORKER_LOST));
            stored.operation.finished_at_ms = Some(now_ms());
            self.record(&stored, fence);
            self.done(&id);
            return;
        }
        let projection = catch_unwind(AssertUnwindSafe(|| executor.projection(&stored, &work)))
            .unwrap_or_else(|_| {
                Err(ApiError::internal(
                    "the operation projection panicked; see the server log",
                ))
            });
        match projection {
            Ok(projection) => stored.projection.extend(projection),
            Err(error) => {
                stored.operation.state = OperationState::Failed;
                stored.operation.error = Some(stored.projection.error(*error.body));
                // Preparation performs no effects and produces no events.
                stored.operation.end_cursor = Some(stored.operation.cursor);
                stored.operation.finished_at_ms = Some(now_ms());
                self.record(&stored, fence);
                self.done(&id);
                return;
            }
        }
        stored.operation.state = OperationState::Running;
        match self
            .store
            .start(&self.worker, fence, &stored, self.options.lease)
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(%id, "lost the claim on an operation before it started");
                self.done(&id);
                return;
            }
            Err(e) => {
                tracing::error!(%id, error = %e, "could not record an operation as running");
                branchyard_support::best_effort(
                    "release the operation's branch lock",
                    self.store.release(&self.worker, &id, fence),
                );
                self.done(&id);
                return;
            }
        }
        // The operation's span, a child of its admission's: the executor
        // gets its context as the operation's trace, for the turns it runs.
        let span = crate::telemetry::operation(
            &crate::telemetry::Running {
                operation: &id,
                repo: &stored.operation.repo,
                kind: &kind_name(stored.operation.kind),
                tenant: stored.tenant(),
                branches: &stored.operation.branches.join(","),
                fence,
            },
            stored.trace.as_deref(),
        );
        let mut admitted = stored.clone();
        admitted.trace = crate::telemetry::traceparent(&span, stored.trace.as_deref());
        let started = Instant::now();
        let finished = {
            // The executor, and the engine's spans and log lines under it,
            // run inside the operation's span.
            let _running = span.enter();
            catch_unwind(AssertUnwindSafe(|| executor.execute(&admitted, &work))).unwrap_or_else(
                |_| Finished {
                    projection: Default::default(),
                    projection_tasks: Default::default(),
                    result: Err(
                        *ApiError::internal("the operation panicked; see the server log").body,
                    ),
                    end_cursor: None,
                },
            )
        };
        if let Err(error) = &finished.result {
            crate::telemetry::attr(&span, "by.error", error.code.clone());
            crate::telemetry::fail(&span, &error.message);
        }
        drop(span);
        stored.projection.extend(finished.projection);
        stored.projection_tasks.extend(finished.projection_tasks);
        let (outcome, result, error) = match finished.result {
            Ok(result) => (OperationState::Succeeded, Some(result), None),
            Err(error) if error.code == "pr_effect_uncertain" => {
                (OperationState::Interrupted, None, Some(error))
            }
            Err(error) => (OperationState::Failed, None, Some(error)),
        };
        stored.operation.state = outcome;
        stored.operation.result = result;
        stored.operation.error = error;
        stored.operation.end_cursor = finished.end_cursor;
        stored.operation.finished_at_ms = Some(now_ms());
        if self.record(&stored, fence) {
            self.options.observability.metrics.observe(
                metrics::OPERATION_DURATION,
                &[
                    ("kind", &kind_name(stored.operation.kind)),
                    (
                        "state",
                        &format!("{:?}", stored.operation.state).to_lowercase(),
                    ),
                ],
                started.elapsed().as_secs_f64(),
            );
        }
        self.done(&id);
    }

    /// Record an outcome under the claim `fence`. After shutdown, or once
    /// another worker took the claim over, the record is left as they
    /// wrote it.
    fn record(&self, stored: &StoredOperation, fence: i64) -> bool {
        let id = &stored.operation.id;
        match self.store.finish(&self.worker, fence, stored) {
            Ok(true) => {
                self.options.observability.metrics.inc(
                    metrics::FINISHED,
                    &[
                        ("kind", &kind_name(stored.operation.kind)),
                        (
                            "state",
                            &format!("{:?}", stored.operation.state).to_lowercase(),
                        ),
                    ],
                );
                true
            }
            Ok(false) => {
                if !self.lock().closed {
                    tracing::warn!(
                        %id,
                        "the outcome of an operation was not recorded: another worker took its \
                         claim over"
                    );
                }
                false
            }
            Err(e) => {
                tracing::error!(%id, error = %e, "could not record the outcome of an operation");
                false
            }
        }
    }

    fn done(&self, id: &str) {
        let mut state = self.lock();
        state.running.remove(id);
        state.finished += 1;
        drop(state);
        self.changed.notify_all();
    }

    /// Hold `branch` for a short synchronous change, such as a removal.
    pub fn hold(
        self: &Arc<Self>,
        repo: &str,
        branch: &str,
        holder: &str,
    ) -> Result<Hold, ApiError> {
        if !self.lock().accepting {
            return Err(ApiError::shutting_down());
        }
        let token = format!("hold_{}", &branchyard_client::new_key()[..20]);
        let held = self
            .store
            .hold(repo, branch, holder, &token, HOLD_TTL)
            .map_err(|e| ApiError::internal(format!("could not lock the branch: {e}")))?;
        if let Some(existing) = held {
            return Err(busy(branch, &existing));
        }
        Ok(Hold {
            registry: self.clone(),
            repo: repo.to_owned(),
            branch: branch.to_owned(),
            token,
        })
    }

    /// How many operations this process runs now.
    pub fn running_here(&self) -> usize {
        self.lock().running.len()
    }

    /// How many operations of this registry's repositories wait in the
    /// queue without running here: queued ones, and ones another worker on
    /// the store runs. `None`, with the store's error logged, when it
    /// cannot say: the count is only for the shutdown's log lines, which
    /// then say the queue could not be read.
    pub fn in_queue(&self) -> Option<usize> {
        let pending = branchyard_support::best_effort(
            "count the queued operations",
            self.store.pending(&self.options.repos),
        )?;
        Some(pending.saturating_sub(self.running_here()))
    }

    /// At shutdown, before the grace period runs out: ask the executor to
    /// stop the turns of each operation still running here
    /// ([`Executor::stop_for_shutdown`]). Returns the IDs it asked about.
    pub fn stop_running_turns(&self) -> Vec<String> {
        let (ids, executor) = {
            let state = self.lock();
            let ids: Vec<String> = state.running.keys().cloned().collect();
            (ids, state.executor.clone())
        };
        let Some(executor) = executor else {
            return Vec::new();
        };
        for id in &ids {
            if let Some(Some(stored)) =
                branchyard_support::best_effort("read a running operation", self.store.get(id))
            {
                executor.stop_for_shutdown(&stored);
            }
        }
        ids
    }

    /// Whether operation `id` still runs here.
    pub fn runs_here(&self, id: &str) -> bool {
        self.lock().running.contains_key(id)
    }

    /// Refuse new operations and stop claiming queued ones.
    pub fn stop_accepting(&self) {
        self.lock().accepting = false;
        self.changed.notify_all();
    }

    /// Wait until nothing runs here and nothing this registry could run is
    /// queued, for at most `timeout`. True when idle.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        self.wait(timeout, true)
    }

    /// Wait until nothing runs here, for at most `timeout`. True when
    /// nothing does.
    pub fn wait_running(&self, timeout: Duration) -> bool {
        self.wait(timeout, false)
    }

    fn wait(&self, timeout: Duration, queued_too: bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let running = !self.lock().running.is_empty();
            let queued = queued_too
                && self
                    .store
                    .pending(&self.options.repos)
                    .map_or(true, |n| n > 0);
            if !running && !queued {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let state = self.lock();
            drop(self.changed.wait_timeout_recovering(
                state,
                (deadline - now).min(Duration::from_millis(20)),
                "changed",
            ));
        }
    }

    /// Record every operation still running here as interrupted and record
    /// nothing more; queued ones stay queued. Returns how many were
    /// interrupted.
    pub fn close(&self) -> usize {
        let running: Vec<(String, i64)> = {
            let mut state = self.lock();
            state.accepting = false;
            state.executor = None;
            state
                .running
                .drain()
                .map(|(id, (fence, _))| (id, fence))
                .collect()
        };
        let mut count = 0;
        for (id, fence) in running {
            let Ok(Some(mut stored)) = self.store.get(&id) else {
                continue;
            };
            stored.operation.state = OperationState::Interrupted;
            stored.operation.error = Some(interrupted(STOPPED));
            stored.operation.finished_at_ms = Some(now_ms());
            if self.record(&stored, fence) {
                count += 1;
            }
        }
        branchyard_support::best_effort("leave the worker pool", self.store.leave(&self.worker));
        self.lock().closed = true;
        self.changed.notify_all();
        count
    }
}

/// An operation kind as the API names it: `task`, `send`, `merge`, ...
pub fn kind_name(kind: OperationKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{kind:?}").to_lowercase())
}

/// The stored operation, if the key's original request was this one, by
/// `tenant`. A key bound by another tenant is refused without naming its
/// operation.
fn same_request(
    stored: StoredOperation,
    idem: &Idempotency,
    tenant: &str,
) -> Result<Operation, ApiError> {
    if stored.tenant() != tenant {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "idempotency_key_reused",
            "this idempotency key was used for a different request",
        ));
    }
    let same = stored
        .idempotency
        .as_ref()
        .is_some_and(|original| original.fingerprint == idem.fingerprint);
    if !same {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "idempotency_key_reused",
            "this idempotency key was used for a different request",
        )
        .detail(serde_json::json!({ "operation": stored.operation.id })));
    }
    Ok(stored.operation)
}

fn busy(branch: &str, holder: &str) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "branch_busy",
        format!("branch {branch} is busy with {holder}"),
    )
    .detail(serde_json::json!({ "branch": branch, "holder": holder }))
}

/// `429 quota_exceeded`, for any of the quotas in
/// `docs/server.md#quotas`.
pub(crate) fn quota_exceeded(limit: &str, tenant: &str, max: usize, reserved: usize) -> ApiError {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "quota_exceeded",
        format!(
            "tenant {tenant} is at its {limit} quota ({reserved} of {max}); wait for one to \
             finish, or ask the operator to raise it"
        ),
    )
    .detail(
        serde_json::json!({ "tenant": tenant, "limit": limit, "max": max, "reserved": reserved }),
    )
}

/// A branch held by [`Registry::hold`], released on drop.
pub struct Hold {
    registry: Arc<Registry>,
    repo: String,
    branch: String,
    token: String,
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Err(e) = self
            .registry
            .store
            .unhold(&self.repo, &self.branch, &self.token)
        {
            tracing::error!(
                branch = %self.branch,
                ttl_s = HOLD_TTL.as_secs(),
                error = %e,
                "could not release a hold; it frees itself within the TTL"
            );
        }
    }
}

fn console_error(error: io::Error) -> ApiError {
    if error.kind() == io::ErrorKind::AlreadyExists {
        ApiError::new(StatusCode::CONFLICT, "revision_conflict", error.to_string())
    } else {
        ApiError::internal(format!("could not persist console metadata: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MemoryStore, SqliteStore};
    use std::sync::mpsc;

    /// The admission's trace is stored with the operation, so the worker
    /// that claims it, on any server sharing the database, continues the
    /// trace: its `claim` and `operation` spans are children of the
    /// admission, in its trace.
    #[test]
    fn another_worker_continues_the_admissions_stored_trace() {
        use branchyard_telemetry::testing::{capture, span_id_of, trace_id_of};
        let capture = capture().unwrap();
        let registry = Registry::open(
            Box::new(SqliteStore::memory()),
            Options::new(vec!["r".into()]),
        )
        .unwrap();
        // Not started: nothing here claims the operation.
        let trace = branchyard_client::new_key();
        let mut request = new(Some("handoff"), &["a"]);
        request.trace = Some(format!("00-{trace}-00f067aa0ba902b7-01"));
        let (operation, _) = registry.submit(request, Value::Null).unwrap();
        let admission = capture
            .trace(&trace)
            .into_iter()
            .find(|s| {
                s.name == "admission" && s.attributes.get("by.operation") == Some(&operation.id)
            })
            .expect("the admission span");
        assert_eq!(admission.parent_id.as_deref(), Some("00f067aa0ba902b7"));
        // Another process claims it from the shared store.
        let other = Worker {
            id: "worker-in-another-process".into(),
            ..registry.worker().clone()
        };
        let claim = registry
            .store
            .claim(&other, &["r".into()], &registry.labels(), DEFAULT_LEASE)
            .unwrap()
            .unwrap();
        let stored = claim.operation.trace.clone().expect("a stored trace");
        assert_eq!(trace_id_of(&stored).as_deref(), Some(trace.as_str()));
        assert_eq!(
            span_id_of(&stored).as_deref(),
            Some(admission.span_id.as_str())
        );
        // What that worker does with it, as the dispatcher does.
        crate::telemetry::claim(
            &crate::telemetry::Claimed {
                operation: &operation.id,
                worker: &other.id,
                fence: claim.fence,
                took_over: false,
                priority: 0,
                admitted_ms: operation.created_at_ms,
                claimed_ms: now_ms(),
            },
            &stored,
        );
        let span = crate::telemetry::operation(
            &crate::telemetry::Running {
                operation: &operation.id,
                repo: "r",
                kind: &kind_name(operation.kind),
                tenant: "default",
                branches: "a",
                fence: claim.fence,
            },
            Some(&stored),
        );
        drop(span);
        let continued = capture.trace(&trace);
        for name in ["claim", "operation send"] {
            let span = continued
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("no {name} span in {continued:#?}"));
            assert_eq!(span.parent_id.as_ref(), Some(&admission.span_id), "{name}");
        }
    }

    #[test]
    fn core_execution_host_labels_prevent_foreign_dispatcher_claims() {
        let registry = Registry::open(
            Box::new(SqliteStore::memory()),
            Options::new(vec!["r".into()]),
        )
        .unwrap();
        assert_eq!(registry.worker().host, branchyard::process_identity().0);
        let required = format!("worker-host:{}", registry.worker().host);
        assert!(registry.labels().contains(&required));
        let mut request = new(Some("host-bound"), &["a"]);
        request.requires = vec![required];
        let (operation, _) = registry.submit(request, Value::Null).unwrap();
        let foreign = Worker {
            id: "foreign-dispatcher".into(),
            host: "another-host-and-boot".into(),
            ..registry.worker().clone()
        };
        assert!(registry
            .store
            .claim(
                &foreign,
                &["r".into()],
                &["worker-host:another-host-and-boot".into()],
                DEFAULT_LEASE
            )
            .unwrap()
            .is_none());
        let claim = registry
            .store
            .claim(
                registry.worker(),
                &["r".into()],
                &registry.labels(),
                DEFAULT_LEASE,
            )
            .unwrap()
            .unwrap();
        assert_eq!(claim.operation.operation.id, operation.id);
        registry.close();
    }

    #[test]
    fn ambiguous_pr_effect_is_interrupted_and_same_key_never_executes_it_again() {
        struct Uncertain(std::sync::atomic::AtomicUsize);
        impl Executor for Uncertain {
            fn execute(&self, _: &StoredOperation, _: &Value) -> Finished {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Finished {
                    projection: Default::default(),
                    projection_tasks: Default::default(),
                    result: Err(ErrorBody {
                        code: "pr_effect_uncertain".into(),
                        message: "inspect forge state before another publication".into(),
                        detail: Some(serde_json::json!({"unknown_effect":true})),
                    }),
                    end_cursor: None,
                }
            }
        }
        let registry = Registry::open(
            Box::new(SqliteStore::memory()),
            Options::new(vec!["r".into()]),
        )
        .unwrap();
        let executor = Arc::new(Uncertain(std::sync::atomic::AtomicUsize::new(0)));
        registry.start(executor.clone()).unwrap();
        let (operation, _) = registry
            .submit(new(Some("uncertain"), &["a"]), Value::Null)
            .unwrap();
        branchyard_testkit::wait::until("uncertain PR outcome to be recorded", || {
            registry
                .get(&operation.id)
                .unwrap()
                .is_some_and(|op| op.state == OperationState::Interrupted)
        });
        let (replayed, was_replay) = registry
            .submit(new(Some("uncertain"), &["a"]), Value::Null)
            .unwrap();
        assert!(was_replay);
        assert_eq!(replayed.id, operation.id);
        assert_eq!(replayed.state, OperationState::Interrupted);
        assert_eq!(executor.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        registry.close();
    }

    #[test]
    fn projection_preflight_failures_never_execute_and_release_branch_locks() {
        struct RefusingHook(bool);
        impl Executor for RefusingHook {
            fn projection(&self, _: &StoredOperation, _: &Value) -> Result<Projection, ApiError> {
                if self.0 {
                    panic!("preflight failure");
                }
                Err(ApiError::internal("cannot verify the execution preset"))
            }
            fn execute(&self, _: &StoredOperation, _: &Value) -> Finished {
                panic!("execution must not start after projection failure");
            }
        }
        for panics in [false, true] {
            let registry = Registry::open(
                Box::new(SqliteStore::memory()),
                Options::new(vec!["r".into()]),
            )
            .unwrap();
            registry.start(Arc::new(RefusingHook(panics))).unwrap();
            let (operation, _) = registry
                .submit(new(Some("preflight"), &["a"]), Value::Null)
                .unwrap();
            let done = registry
                .wait_for_tenant(&operation.id, "default", Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(done.state, OperationState::Failed);
            assert_eq!(
                done.error.unwrap().message,
                if panics {
                    "the operation projection panicked; see the server log"
                } else {
                    "cannot verify the execution preset"
                }
            );
            assert!(registry
                .submit(new(Some("after-preflight"), &["a"]), Value::Null)
                .is_ok());
            registry.close();
        }
    }

    #[test]
    fn task_projection_survives_restart_worker_changes_and_idempotent_replay() {
        struct ChangedWorker;
        impl Executor for ChangedWorker {
            fn projection(&self, _: &StoredOperation, _: &Value) -> Result<Projection, ApiError> {
                serde_json::from_value(serde_json::json!([{
                    "value":"/worker/preflight.key",
                    "placeholder":"[preset pool: bridge key]"
                }]))
                .map_err(|error| ApiError::internal(error.to_string()))
            }
            fn execute(&self, stored: &StoredOperation, _: &Value) -> Finished {
                let mut text = serde_json::json!("/worker/preflight.key");
                stored.projection.apply(&mut text);
                assert_eq!(text, "[preset pool: bridge key]");
                Finished {
                    projection: serde_json::from_value(serde_json::json!([{
                        "value":"http://worker.internal:7443",
                        "placeholder":"[preset pool: control endpoint]"
                    }]))
                    .unwrap(),
                    projection_tasks: BTreeMap::from([("a".into(), "native-task".into())]),
                    result: Ok(OperationResult::default()),
                    end_cursor: Some(0),
                }
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("projection.sqlite");
        let registry = Registry::open(
            Box::new(SqliteStore::open(&path, None).unwrap()),
            Options::new(vec!["r".into()]),
        )
        .unwrap();
        let projection: Projection = serde_json::from_value(serde_json::json!([{
            "value":"/operator/admission.key",
            "placeholder":"[preset pool: bridge key]"
        }]))
        .unwrap();
        let task_admission = || {
            let mut request = new(Some("projected-task"), &["a"]);
            request.kind = OperationKind::Task;
            request.creates = vec!["a".into()];
            request
        };
        let (operation, replayed) = registry
            .submit_projected(task_admission(), Value::Null, projection.clone())
            .unwrap();
        assert!(!replayed);
        assert_eq!(
            registry.operation_projection(&operation.id, "r").unwrap(),
            projection
        );
        assert_eq!(
            registry.branch_projection("r", "a", "native-task").unwrap(),
            projection
        );
        assert!(registry
            .branch_projection("other", "a", "native-task")
            .unwrap()
            .is_empty());
        assert!(registry
            .operation_projection(&operation.id, "other")
            .unwrap()
            .is_empty());
        assert!(registry
            .operation_projection("unknown", "r")
            .unwrap()
            .is_empty());
        assert!(registry
            .get_for_tenant(&operation.id, "foreign")
            .unwrap()
            .is_none());
        assert!(serde_json::to_value(&operation)
            .unwrap()
            .get("projection")
            .is_none());
        let stored = registry.store.get(&operation.id).unwrap().unwrap();
        let mut legacy = serde_json::to_value(&stored).unwrap();
        legacy.as_object_mut().unwrap().remove("projection");
        legacy.as_object_mut().unwrap().remove("projection_tasks");
        let legacy: StoredOperation = serde_json::from_value(legacy).unwrap();
        assert!(legacy.projection.is_empty());
        assert!(legacy.projection_tasks.is_empty());
        registry.close();
        drop(registry);

        let registry = Registry::open(
            Box::new(SqliteStore::open(&path, None).unwrap()),
            Options::new(vec!["r".into()]),
        )
        .unwrap();
        assert_eq!(
            registry.operation_projection(&operation.id, "r").unwrap(),
            projection
        );
        registry.start(Arc::new(ChangedWorker)).unwrap();
        branchyard_testkit::wait::until("the projected task to finish", || {
            registry
                .get(&operation.id)
                .unwrap()
                .is_some_and(|op| op.state.is_terminal())
        });
        let snapshot = registry.operation_projection(&operation.id, "r").unwrap();
        assert_eq!(
            registry.branch_projection("r", "a", "native-task").unwrap(),
            snapshot
        );
        assert!(registry
            .branch_projection("r", "a", "recreated-raw-task")
            .unwrap()
            .is_empty());
        let mut other = registry.store.get(&operation.id).unwrap().unwrap();
        other.operation.id = "other-repository".into();
        other.operation.repo = "other".into();
        other.idempotency = None;
        other
            .projection_tasks
            .insert("a".into(), "other-native-task".into());
        registry.store.save(&other).unwrap();
        assert!(registry
            .branch_projection("other", "a", "native-task")
            .unwrap()
            .is_empty());
        assert_eq!(
            registry
                .branch_projection("other", "a", "other-native-task")
                .unwrap(),
            snapshot
        );
        assert_eq!(
            registry.branch_projection("r", "a", "native-task").unwrap(),
            snapshot
        );
        let restored: StoredOperation = serde_json::from_slice(
            &serde_json::to_vec(&registry.store.get(&operation.id).unwrap().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(restored.projection_tasks["a"], "native-task");
        // Historical feed projection is bound to the operation's sequence
        // interval, independent of the branch's current existence or task ID.
        let mut historical = restored.clone();
        historical.operation.id = "historical-feed-interval".into();
        historical.idempotency = None;
        historical.operation.cursor = 10;
        historical.operation.end_cursor = Some(20);
        let child = serde_json::from_value(serde_json::json!({
            "name":"removed-child", "git_branch":"by/removed-child", "worktree":"/public/work",
            "prompt":"public task", "harness":"gemini-cli", "profile":"gemini-cli",
            "session":null, "parent":"a", "base":"base", "candidate":null,
            "status":{"state":"failed", "reason":"public failure"},
            "turns":1, "cost_usd":null, "created_at":0
        }))
        .unwrap();
        historical.operation.result = Some(OperationResult {
            descendants: vec![child],
            ..OperationResult::default()
        });
        registry.store.save(&historical).unwrap();
        for seq in [11, 20] {
            assert_eq!(registry.feed_projection("r", "a", seq).unwrap(), snapshot);
            assert_eq!(
                registry.feed_projection("r", "removed-child", seq).unwrap(),
                snapshot
            );
        }
        for (repo, branch, seq) in [
            ("r", "a", 10),
            ("r", "a", 21),
            ("other", "a", 11),
            ("r", "unrelated", 11),
        ] {
            assert!(registry
                .feed_projection(repo, branch, seq)
                .unwrap()
                .is_empty());
        }
        let mut active = historical;
        active.operation.id = "active-feed-interval".into();
        active.operation.state = OperationState::Running;
        active.operation.cursor = 30;
        active.operation.end_cursor = None;
        registry.store.save(&active).unwrap();
        assert_eq!(registry.feed_projection("r", "a", 31).unwrap(), snapshot);
        assert_eq!(
            registry.removed_branch_projection("r", "a").unwrap(),
            snapshot
        );
        assert!(registry
            .removed_branch_projection("wrong-repo", "a")
            .unwrap()
            .is_empty());
        let candidates = registry.feed_projection_candidates("r", 11).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].operation.id, "historical-feed-interval");
        let mut send = restored.clone();
        send.operation.id = "followup-send-feed".into();
        send.operation.kind = OperationKind::Send;
        send.operation.cursor = 40;
        send.operation.end_cursor = Some(50);
        send.operation.result = None;
        send.idempotency = None;
        send.creates.clear();
        send.projection_tasks.clear();
        registry.store.save(&send).unwrap();
        assert_eq!(registry.feed_projection("r", "a", 45).unwrap(), snapshot);
        send.operation.id = "missing-feed-end".into();
        send.operation.cursor = 60;
        send.operation.end_cursor = None;
        registry.store.save(&send).unwrap();
        let refused = registry.feed_projection("r", "a", 61).unwrap_err();
        assert_eq!(
            refused.body.message,
            "could not verify the feed's preset projection interval"
        );
        assert!(registry
            .feed_projection_candidates("r", 61)
            .unwrap()
            .iter()
            .any(|op| op.operation.id == "missing-feed-end"));
        let mut interrupted = restored.clone();
        interrupted.operation.id = "interrupted-without-task-identity".into();
        interrupted.operation.state = OperationState::Interrupted;
        interrupted.idempotency = None;
        interrupted.creates = vec!["lost-branch".into()];
        interrupted.projection_tasks.clear();
        registry.store.save(&interrupted).unwrap();
        let refused = registry
            .branch_projection("r", "lost-branch", "unverified-task")
            .unwrap_err();
        assert_eq!(
            refused.body.message,
            "could not verify the branch's preset projection"
        );
        let mut raw = interrupted;
        raw.operation.id = "new-raw-incarnation".into();
        raw.projection = Projection::default();
        raw.projection_tasks
            .insert("lost-branch".into(), "new-raw-task".into());
        registry.store.save(&raw).unwrap();
        assert!(registry
            .branch_projection("r", "lost-branch", "new-raw-task")
            .unwrap()
            .is_empty());
        let mut message = serde_json::json!("/operator/admission.key; http://worker.internal:7443");
        snapshot.apply(&mut message);
        assert_eq!(
            message,
            "[preset pool: bridge key]; [preset pool: control endpoint]"
        );
        let (replayed, was_replay) = registry
            .submit_projected(task_admission(), Value::Null, Projection::default())
            .unwrap();
        assert!(was_replay);
        assert_eq!(replayed.id, operation.id);
        assert_eq!(
            registry.operation_projection(&operation.id, "r").unwrap(),
            snapshot
        );
        registry.close();
    }

    /// Runs work `{"n": N}` by recording N; `{"wait": true}` blocks until
    /// released through `gate`, and `{"wait": "<key>"}` through
    /// `gates[key]`.
    #[derive(Default)]
    struct Recorder {
        ran: Mutex<Vec<u64>>,
        gate: Mutex<Option<mpsc::Receiver<()>>>,
        gates: Mutex<HashMap<String, mpsc::Receiver<()>>>,
    }

    impl Recorder {
        /// Hold work `{"wait": key}` until the returned sender sends.
        fn gate(&self, key: &str) -> mpsc::Sender<()> {
            let (release, gate) = mpsc::channel();
            self.gates.lock().unwrap().insert(key.into(), gate);
            release
        }

        /// Whether work `{"wait": key}` has started.
        fn holds(&self, key: &str) -> bool {
            !self.gates.lock().unwrap().contains_key(key)
        }
    }

    impl Executor for Recorder {
        fn execute(&self, _: &StoredOperation, work: &Value) -> Finished {
            let gate = match &work["wait"] {
                Value::Bool(true) => self.gate.lock().unwrap().take(),
                Value::String(key) => self.gates.lock().unwrap().remove(key),
                _ => None,
            };
            if let Some(gate) = gate {
                gate.recv().unwrap();
            }
            if let Some(n) = work["n"].as_u64() {
                self.ran.lock().unwrap().push(n);
            }
            Finished {
                projection: Default::default(),
                projection_tasks: Default::default(),
                result: Ok(OperationResult::default()),
                end_cursor: Some(0),
            }
        }
    }

    fn new(key: Option<&str>, locks: &[&str]) -> NewOperation {
        NewOperation {
            repo: "r".into(),
            kind: OperationKind::Send,
            branches: locks.iter().map(|s| s.to_string()).collect(),
            cursor: 0,
            locks: locks.iter().map(|s| s.to_string()).collect(),
            idempotency: key.map(|key| Idempotency {
                caller: "c".into(),
                key: key.into(),
                fingerprint: "f".into(),
            }),
            principal: Principal::default_for("c"),
            creates: Vec::new(),
            quota: AdmissionQuota::default(),
            requires: Vec::new(),
            priority: 0,
            trace: None,
            lane: Lane::Normal,
        }
    }

    fn in_tenant(mut new: NewOperation, tenant: &str, max_running: Option<usize>) -> NewOperation {
        new.principal.tenant = tenant.into();
        new.quota.max_running = max_running;
        new
    }

    fn options(max_running: usize) -> Options {
        Options {
            max_running,
            poll: Duration::from_millis(10),
            ..Options::new(vec!["r".into()])
        }
    }

    fn started(
        store: Box<dyn OperationStore>,
        max_running: usize,
    ) -> (Arc<Registry>, Arc<Recorder>) {
        let registry = Registry::open(store, options(max_running)).unwrap();
        let recorder = Arc::new(Recorder::default());
        registry.start(recorder.clone()).unwrap();
        (registry, recorder)
    }

    fn state(registry: &Registry, id: &str) -> OperationState {
        registry.get(id).unwrap().unwrap().state
    }

    #[test]
    fn console_admission_binds_operation_and_replay_keeps_original_metadata() {
        use crate::console_store::{Collection, Mutation, Record};
        let registry = Registry::open(Box::new(MemoryStore::default()), options(1)).unwrap();
        let row = Record {
            collection: Collection::Workspaces,
            id: "workspace".into(),
            tenant: "default".into(),
            revision: 1,
            body: serde_json::json!({"id":"workspace"}),
        };
        let work = serde_json::json!({"kind":"console","command":{"action":"initialize","workspace_id":"workspace"}});
        let (operation, replayed) = registry
            .submit_console(
                new(Some("console"), &[]),
                work.clone(),
                &[Mutation::Put {
                    record: row.clone(),
                    expected: None,
                }],
            )
            .unwrap();
        assert!(!replayed);
        let binding = registry
            .console_get(Collection::Bindings, &format!("operation:{}", operation.id))
            .unwrap()
            .unwrap();
        assert_eq!(binding.tenant, "default");
        assert_eq!(binding.body["workspace_id"], "workspace");
        let (again, replayed) = registry
            .submit_console(new(Some("console"), &[]), work, &[])
            .unwrap();
        assert!(replayed);
        assert_eq!(again.id, operation.id);
        assert_eq!(
            registry
                .console_list_all(Collection::Bindings)
                .unwrap()
                .len(),
            1
        );
        let workspace = registry
            .console_get(Collection::Workspaces, "workspace")
            .unwrap()
            .unwrap();
        assert_eq!(workspace.body["_operation_id"], operation.id);
        let session_row = Record {
            collection: Collection::Sessions,
            id: "session".into(),
            tenant: "default".into(),
            revision: 1,
            body: serde_json::json!({"workspace_id":"workspace","branch":"session"}),
        };
        let (session_operation, _) = registry.submit_console(
            new(Some("session"), &[]),
            serde_json::json!({"kind":"console","command":{"action":"start","workspace_id":"workspace","session_id":"session"}}),
            &[Mutation::Put {record:session_row,expected:None}],
        ).unwrap();
        let session = registry
            .console_get(Collection::Sessions, "session")
            .unwrap()
            .unwrap();
        assert_eq!(session.body["operation_id"], session_operation.id);
        assert_eq!(session.revision, 1);
        let mut cancellation = session.clone();
        cancellation.revision = 2;
        cancellation.body["_cancel_requested"] = serde_json::json!(true);
        let (cancel, _) = registry.submit_console(
            new(Some("cancel"), &[]),
            serde_json::json!({"kind":"console","command":{"action":"control","control":"cancel","workspace_id":"workspace","session_id":"session"}}),
            &[Mutation::Put {record:cancellation,expected:Some(1)}],
        ).unwrap();
        let cancelled = registry
            .console_get(Collection::Sessions, "session")
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.body["_cancel_operation_id"], cancel.id);
        assert_eq!(cancelled.body["operation_id"], session_operation.id);
        assert_eq!(cancelled.revision, 2);
        let mut lifecycle_workspace = workspace.clone();
        lifecycle_workspace.revision = 2;
        lifecycle_workspace.body["state"] = serde_json::json!("archiving");
        let (lifecycle,_) = registry.submit_console(
            new(Some("archive"),&[]),
            serde_json::json!({"kind":"console","command":{"action":"lifecycle","intent":"archive","workspace_id":"workspace"}}),
            &[Mutation::Put {record:lifecycle_workspace,expected:Some(1)}],
        ).unwrap();
        let updated = registry
            .console_get(Collection::Workspaces, "workspace")
            .unwrap()
            .unwrap();
        assert_eq!(updated.body["_operation_id"], lifecycle.id);
        assert_eq!(updated.revision, 2);
        let error = registry
            .submit_console(
                new(Some("bad"), &[]),
                serde_json::json!({"kind":"console","command":{}}),
                &[],
            )
            .unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(registry.store.by_key("c", "bad").unwrap().is_none());
    }

    #[test]
    fn a_repeated_key_returns_the_first_operation_and_runs_once() {
        let (registry, recorder) = started(Box::new(MemoryStore::default()), 2);
        let (first, replayed) = registry
            .submit(new(Some("k"), &[]), serde_json::json!({ "n": 1 }))
            .unwrap();
        assert!(!replayed);
        let (again, replayed) = registry
            .submit(new(Some("k"), &[]), serde_json::json!({ "n": 1 }))
            .unwrap();
        assert!(replayed);
        assert_eq!(first.id, again.id);
        assert!(registry.wait_idle(Duration::from_secs(5)));
        assert_eq!(*recorder.ran.lock().unwrap(), [1]);
        assert_eq!(state(&registry, &first.id), OperationState::Succeeded);

        let mut other = new(Some("k"), &[]);
        other.idempotency.as_mut().unwrap().fingerprint = "g".into();
        let error = registry.submit(other, Value::Null).unwrap_err();
        assert_eq!(error.body.code, "idempotency_key_reused");
        registry.close();
    }

    #[test]
    fn short_control_lane_runs_while_normal_capacity_is_full() {
        let (registry, recorder) = started(Box::new(MemoryStore::default()), 1);
        let (release, gate) = mpsc::channel();
        *recorder.gate.lock().unwrap() = Some(gate);
        registry
            .submit(
                new(Some("held"), &["held"]),
                serde_json::json!({"wait":true}),
            )
            .unwrap();
        branchyard_testkit::wait::until("normal operation to fill the worker slot", || {
            recorder.gate.lock().unwrap().is_none()
        });
        let control = NewOperation {
            lane: Lane::Control,
            ..new(Some("control"), &[])
        };
        let (control,_) = registry.submit_console(control,serde_json::json!({"kind":"console","command":{"action":"control","control":"cancel","workspace_id":"w"},"n":2}),&[]).unwrap();
        branchyard_testkit::wait::until("control to run despite saturated normal capacity", || {
            registry.get(&control.id).unwrap().unwrap().state == OperationState::Succeeded
        });
        assert!(recorder.ran.lock().unwrap().contains(&2));
        release.send(()).unwrap();
        assert!(registry.wait_idle(Duration::from_secs(5)));
        registry.close();
    }

    fn follow_up() -> NewOperation {
        NewOperation {
            lane: Lane::FollowUp,
            ..new(None, &[])
        }
    }

    #[test]
    fn a_follow_up_runs_on_the_reserved_slot_while_capacity_is_full() {
        let (registry, recorder) = started(Box::new(MemoryStore::default()), 1);
        let release = recorder.gate("held");
        registry
            .submit(new(None, &[]), serde_json::json!({ "wait": "held" }))
            .unwrap();
        branchyard_testkit::wait::until("a normal operation to fill the worker slot", || {
            recorder.holds("held")
        });
        let (queued, _) = registry
            .submit(new(None, &[]), serde_json::json!({ "n": 1 }))
            .unwrap();
        let (continued, _) = registry
            .submit(follow_up(), serde_json::json!({ "n": 2 }))
            .unwrap();
        branchyard_testkit::wait::until("the follow-up to run on the reserved slot", || {
            state(&registry, &continued.id) == OperationState::Succeeded
        });
        assert_eq!(state(&registry, &queued.id), OperationState::Queued);
        assert_eq!(*recorder.ran.lock().unwrap(), [2]);
        release.send(()).unwrap();
        assert!(registry.wait_idle(Duration::from_secs(5)));
        assert_eq!(state(&registry, &queued.id), OperationState::Succeeded);
        assert_eq!(*recorder.ran.lock().unwrap(), [2, 1]);
        registry.close();
    }

    #[test]
    fn a_follow_up_is_claimed_before_older_launches() {
        let (registry, recorder) = started(Box::new(MemoryStore::default()), 1);
        let normal = recorder.gate("normal");
        let reserved = recorder.gate("reserved");
        registry
            .submit(new(None, &[]), serde_json::json!({ "wait": "normal" }))
            .unwrap();
        branchyard_testkit::wait::until("a normal operation to fill the worker slot", || {
            recorder.holds("normal")
        });
        registry
            .submit(follow_up(), serde_json::json!({ "wait": "reserved" }))
            .unwrap();
        branchyard_testkit::wait::until("a follow-up to fill the reserved slot", || {
            recorder.holds("reserved")
        });
        for n in 1..=2 {
            registry
                .submit(new(None, &[]), serde_json::json!({ "n": n }))
                .unwrap();
        }
        registry
            .submit(follow_up(), serde_json::json!({ "n": 3 }))
            .unwrap();
        normal.send(()).unwrap();
        branchyard_testkit::wait::until("the queued operations to run", || {
            recorder.ran.lock().unwrap().len() == 3
        });
        assert_eq!(*recorder.ran.lock().unwrap(), [3, 1, 2]);
        reserved.send(()).unwrap();
        assert!(registry.wait_idle(Duration::from_secs(5)));
        registry.close();
    }

    #[test]
    fn each_reserved_lane_admits_one_operation_at_the_tenant_ceiling() {
        let registry = Registry::open(Box::new(SqliteStore::memory()), options(1)).unwrap();
        let submit = |key: &str, lane: Lane| {
            let mut op = NewOperation {
                lane,
                ..in_tenant(new(Some(key), &[]), "acme", Some(1))
            };
            op.quota.max_branches = Some(0);
            registry.submit(op, Value::Null)
        };
        let refused = |key: &str, lane: Lane, reserved: usize| {
            let error = submit(key, lane).unwrap_err();
            assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS, "{key}");
            assert_eq!(
                error.body.detail,
                Some(serde_json::json!({
                    "tenant": "acme", "limit": "max_running", "max": 1, "reserved": reserved
                })),
                "{key}"
            );
            assert!(
                registry.by_key("c", key, "acme").unwrap().is_none(),
                "{key}"
            );
        };
        assert!(submit("normal", Lane::Normal).is_ok());
        refused("second-normal", Lane::Normal, 1);
        assert!(submit("follow-up", Lane::FollowUp).is_ok());
        refused("second-follow-up", Lane::FollowUp, 2);
        assert!(submit("control", Lane::Control).is_ok());
        refused("second-control", Lane::Control, 3);
        refused("third-normal", Lane::Normal, 3);
        assert_eq!(registry.store.queue().unwrap().len(), 3);
    }

    #[test]
    fn locks_refuse_a_second_change_until_the_first_finishes() {
        let (registry, recorder) = started(Box::new(MemoryStore::default()), 2);
        let (release, gate) = mpsc::channel::<()>();
        *recorder.gate.lock().unwrap() = Some(gate);
        let (op, _) = registry
            .submit(new(None, &["b"]), serde_json::json!({ "wait": true }))
            .unwrap();
        let error = registry.submit(new(None, &["b"]), Value::Null).unwrap_err();
        assert_eq!(error.body.code, "branch_busy");
        assert!(registry.hold("r", "b", "delete").is_err());
        release.send(()).unwrap();
        assert!(registry.wait_idle(Duration::from_secs(5)));
        assert_eq!(state(&registry, &op.id), OperationState::Succeeded);
        let hold = registry.hold("r", "b", "delete").unwrap();
        let error = registry.submit(new(None, &["b"]), Value::Null).unwrap_err();
        assert!(error.body.message.contains("busy with delete"), "{error:?}");
        drop(hold);
        assert!(registry.submit(new(None, &["b"]), Value::Null).is_ok());
        registry.close();
    }

    #[test]
    fn a_wait_for_an_operation_wakes_as_it_finishes_or_gives_up_at_its_timeout() {
        // A poll far longer than the test: only the finish can wake it.
        let registry = Registry::open(
            Box::new(MemoryStore::default()),
            Options {
                poll: Duration::from_secs(60),
                ..options(2)
            },
        )
        .unwrap();
        let recorder = Arc::new(Recorder::default());
        registry.start(recorder.clone()).unwrap();
        let (release, gate) = mpsc::channel::<()>();
        *recorder.gate.lock().unwrap() = Some(gate);
        let (op, _) = registry
            .submit(new(None, &["b"]), serde_json::json!({ "wait": true }))
            .unwrap();
        let tenant = crate::config::DEFAULT_TENANT;

        let pending = registry
            .wait_for_tenant(&op.id, tenant, Duration::from_millis(50))
            .unwrap()
            .unwrap();
        assert!(!pending.state.is_terminal(), "{pending:?}");
        assert!(registry
            .wait_for_tenant(&op.id, "other", Duration::from_secs(5))
            .unwrap()
            .is_none());

        let waiting = {
            let (registry, id) = (registry.clone(), op.id.clone());
            std::thread::spawn(move || {
                let started = Instant::now();
                let op = registry.wait_for_tenant(&id, tenant, Duration::from_secs(30));
                (op, started.elapsed())
            })
        };
        release.send(()).unwrap();
        let (finished, took) = waiting.join().unwrap();
        assert_eq!(finished.unwrap().unwrap().state, OperationState::Succeeded);
        assert!(took < Duration::from_secs(20), "woke after {took:?}");
        registry.close();
    }

    #[test]
    fn a_tenants_max_running_quota_is_reserved_at_admission_and_released() {
        let (registry, recorder) = started(Box::new(MemoryStore::default()), 4);
        let (release, gate) = mpsc::channel::<()>();
        *recorder.gate.lock().unwrap() = Some(gate);
        let (op, _) = registry
            .submit(
                in_tenant(new(None, &["a"]), "acme", Some(1)),
                serde_json::json!({ "wait": true }),
            )
            .unwrap();
        let error = registry
            .submit(in_tenant(new(None, &["b"]), "acme", Some(1)), Value::Null)
            .unwrap_err();
        assert_eq!(error.body.code, "quota_exceeded");
        assert_eq!(error.body.detail.as_ref().unwrap()["reserved"], 1);
        // A different tenant is unaffected.
        assert!(registry
            .submit(in_tenant(new(None, &["c"]), "other", Some(1)), Value::Null)
            .is_ok());
        release.send(()).unwrap();
        assert!(registry.wait_idle(Duration::from_secs(5)));
        assert_eq!(state(&registry, &op.id), OperationState::Succeeded);
        // Released: the tenant can submit again.
        assert!(registry
            .submit(in_tenant(new(None, &["d"]), "acme", Some(1)), Value::Null)
            .is_ok());
        registry.close();
    }

    #[test]
    fn get_for_tenant_hides_another_tenants_operation() {
        let registry = Registry::open(Box::new(MemoryStore::default()), options(1)).unwrap();
        let (op, _) = registry
            .submit(in_tenant(new(Some("k"), &[]), "acme", None), Value::Null)
            .unwrap();
        assert!(registry.get_for_tenant(&op.id, "acme").unwrap().is_some());
        assert!(registry.get_for_tenant(&op.id, "other").unwrap().is_none());
        assert!(registry
            .get_for_tenant("op_bogus", "acme")
            .unwrap()
            .is_none());
        assert!(registry.by_key("c", "k", "acme").unwrap().is_some());
        assert!(registry.by_key("c", "k", "other").unwrap().is_none());
        // Another tenant's key never replays its operation.
        let error = registry
            .submit(in_tenant(new(Some("k"), &[]), "other", None), Value::Null)
            .unwrap_err();
        assert_eq!(error.body.code, "idempotency_key_reused");
        assert!(error.body.detail.is_none());
    }

    #[test]
    fn queued_operations_start_in_order() {
        let (registry, recorder) = started(Box::new(MemoryStore::default()), 1);
        let (release, gate) = mpsc::channel::<()>();
        *recorder.gate.lock().unwrap() = Some(gate);
        registry
            .submit(new(None, &[]), serde_json::json!({ "wait": true }))
            .unwrap();
        for n in 0..5 {
            registry
                .submit(new(None, &[]), serde_json::json!({ "n": n }))
                .unwrap();
        }
        release.send(()).unwrap();
        assert!(registry.wait_idle(Duration::from_secs(5)));
        assert_eq!(*recorder.ran.lock().unwrap(), [0, 1, 2, 3, 4]);
        registry.close();
    }

    /// A database path in a fresh directory, removed when the returned
    /// guard is dropped.
    fn temp_db(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("branchyard-ops-{name}-"))
            .tempdir()
            .unwrap();
        let db = dir.path().join("state.db");
        (dir, db)
    }

    #[test]
    fn close_interrupts_what_runs_and_a_restart_runs_what_was_queued() {
        let (_dir, db) = temp_db("close");
        let (registry, recorder) = started(Box::new(SqliteStore::open(&db, None).unwrap()), 1);
        let (release, gate) = mpsc::channel::<()>();
        *recorder.gate.lock().unwrap() = Some(gate);
        let (running, _) = registry
            .submit(new(None, &["a"]), serde_json::json!({ "wait": true }))
            .unwrap();
        let (queued, _) = registry
            .submit(new(None, &["b"]), serde_json::json!({ "n": 7 }))
            .unwrap();
        branchyard_testkit::wait::until("the first operation to run", || {
            state(&registry, &running.id) == OperationState::Running
        });
        registry.stop_accepting();
        assert!(!registry.wait_running(Duration::from_millis(50)));
        assert_eq!(registry.close(), 1);
        release.send(()).unwrap();
        branchyard_testkit::wait::settle(
            "a closed registry would have started the queued operation by now",
            Duration::from_millis(50),
        );
        assert_eq!(state(&registry, &queued.id), OperationState::Queued);
        drop(registry);

        // The restarted server (the store is its own) runs the queued one,
        // and the interrupted one stays so, its locks released.
        let (reopened, recorder) = started(Box::new(SqliteStore::open(&db, None).unwrap()), 1);
        assert!(reopened.wait_idle(Duration::from_secs(5)));
        let op = reopened.get(&running.id).unwrap().unwrap();
        assert_eq!(op.state, OperationState::Interrupted);
        assert_eq!(op.error.unwrap().message, STOPPED);
        assert_eq!(state(&reopened, &queued.id), OperationState::Succeeded);
        assert_eq!(*recorder.ran.lock().unwrap(), [7]);
        assert!(reopened.submit(new(None, &["a", "b"]), Value::Null).is_ok());
        reopened.close();
    }

    #[test]
    fn a_crash_between_admission_and_execution_runs_it_once_elsewhere() {
        let (_dir, db) = temp_db("crash");
        // Admitted by a registry that never ran anything, then gone.
        let admitting =
            Registry::open(Box::new(SqliteStore::open(&db, None).unwrap()), options(1)).unwrap();
        let (op, _) = admitting
            .submit(new(Some("k"), &["b"]), serde_json::json!({ "n": 3 }))
            .unwrap();
        drop(admitting);
        let (other, recorder) = started(Box::new(SqliteStore::open(&db, None).unwrap()), 2);
        // A retry of the lost response maps to the same operation.
        let (again, replayed) = other
            .submit(new(Some("k"), &["b"]), serde_json::json!({ "n": 3 }))
            .unwrap();
        assert!(replayed);
        assert_eq!(again.id, op.id);
        assert!(other.wait_idle(Duration::from_secs(5)));
        assert_eq!(state(&other, &op.id), OperationState::Succeeded);
        assert_eq!(*recorder.ran.lock().unwrap(), [3]);
        other.close();
    }

    #[test]
    fn an_expired_claim_is_taken_over_and_a_started_one_is_never_rerun() {
        let (_dir, db) = temp_db("takeover");
        let open =
            || -> Box<dyn OperationStore> { Box::new(SqliteStore::open(&db, None).unwrap()) };
        let shared = Options {
            exclusive: false,
            ..options(2)
        };
        let admitting = Registry::open(open(), shared.clone()).unwrap();
        let (unstarted, _) = admitting
            .submit(new(None, &["a"]), serde_json::json!({ "n": 1 }))
            .unwrap();
        let (started_op, _) = admitting
            .submit(new(None, &["b"]), serde_json::json!({ "n": 2 }))
            .unwrap();
        // A worker that claims both, starts the second, then dies: its
        // claims expire.
        let ghost = Worker {
            id: "ghost".into(),
            host: "elsewhere".into(),
            ..Worker::current()
        };
        let store = open();
        // Long enough that the first claim is still held when the second is
        // made on a slow runner (else the second claim takes the first one
        // over again), short enough for the takeover below to come quickly.
        let lease = Duration::from_millis(750);
        let repos = ["r".to_owned()];
        let first = store.claim(&ghost, &repos, &[], lease).unwrap().unwrap();
        let second = store.claim(&ghost, &repos, &[], lease).unwrap().unwrap();
        assert_eq!(first.operation.operation.id, unstarted.id);
        assert_eq!(second.operation.operation.id, started_op.id);
        let mut running = second.operation.clone();
        running.operation.state = OperationState::Running;
        assert!(store.start(&ghost, second.fence, &running, lease).unwrap());

        let registry = Registry::open(open(), shared).unwrap();
        let recorder = Arc::new(Recorder::default());
        registry.start(recorder.clone()).unwrap();
        assert!(registry.wait_idle(Duration::from_secs(5)));
        assert_eq!(state(&registry, &unstarted.id), OperationState::Succeeded);
        let taken = registry.get(&started_op.id).unwrap().unwrap();
        assert_eq!(taken.state, OperationState::Interrupted);
        assert_eq!(taken.error.unwrap().message, WORKER_LOST);
        assert_eq!(*recorder.ran.lock().unwrap(), [1], "never run twice");
        // The dead worker is fenced out.
        let mut late = second.operation.clone();
        late.operation.state = OperationState::Succeeded;
        assert!(!store.finish(&ghost, second.fence, &late).unwrap());
        assert!(registry.submit(new(None, &["a", "b"]), Value::Null).is_ok());
        registry.close();
        drop(admitting);
    }
}
