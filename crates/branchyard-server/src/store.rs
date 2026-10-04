//! Where operations persist, and the durable queue they are dispatched
//! through.
//!
//! [`OperationStore`] keeps four things in one database, so that each
//! change to them is one transaction:
//!
//! - operation records, with a unique index on the caller's idempotency
//!   key, so a retry anywhere maps to the same operation;
//! - the dispatch queue: one row per accepted, unfinished operation, with
//!   the serializable description of its work, claimed by a worker under a
//!   lease and a fence;
//! - branch locks, so two operations never change one branch at once, on
//!   this server or another sharing the database;
//! - webhook cursors.
//!
//! Admission ([`OperationStore::admit`]) writes the record, its idempotency
//! binding, its branch locks and its queue row in one transaction, before
//! the server answers `202 Accepted`: a committed admission is a durable
//! enqueue, and a failed one leaves nothing behind. Finishing
//! ([`OperationStore::finish`]) writes the outcome, deletes the queue row
//! and releases the locks in one transaction, only while the worker's
//! claim still holds.
//!
//! [`SqliteStore`] is what a server with a data directory uses, and
//! [`MemoryStore`] is the same on an in-memory database. With the
//! `postgres` feature, `PostgresStore` keeps them in PostgreSQL, where
//! several servers may share them: claims use `FOR UPDATE SKIP LOCKED`.
//! The queue is plain tables; PGMQ could replace the queue table later
//! without changing the transaction's shape. [`FileStore`], the JSON-lines
//! file earlier versions used, is only read, to import it once.

#![allow(
    clippy::expect_used,
    clippy::let_underscore_must_use,
    clippy::map_unwrap_or,
    clippy::unwrap_used
)] // ratchet: branchyard-server
use branchyard_support::LockExt as _;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use branchyard_client::api::Operation;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use branchyard::store_codec::{
    deadline, from_db_i32, from_db_opt, from_db_u32, from_db_usize, millis_saturating, to_db,
    to_db_opt,
};

use crate::config::{Principal, DEFAULT_TENANT};

/// An idempotency key as the server scopes it: per authenticated caller and
/// per request route, with a fingerprint of the request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Idempotency {
    /// The token's configured name.
    pub caller: String,
    pub key: String,
    /// Method, route and canonical body, hashed.
    pub fingerprint: String,
}

/// An operation and what the server needs to deduplicate and lock it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredOperation {
    pub operation: Operation,
    #[serde(default)]
    pub idempotency: Option<Idempotency>,
    /// Branch names no other operation may change while this one runs.
    #[serde(default)]
    pub locks: Vec<String>,
    /// The tenant of the principal that submitted this operation. Absent
    /// (default) on a record from before tenants existed, which reads as
    /// [`crate::config::DEFAULT_TENANT`] everywhere this is used: not part
    /// of the wire `Operation`, since it is for the server's own isolation
    /// and quota bookkeeping, not something a caller needs echoed back.
    /// Admission counts a tenant's queued and running operations by this
    /// field of the durable record, so `max_running` holds across servers
    /// sharing the store and across restarts.
    #[serde(default)]
    pub tenant: String,
    /// The principal that admitted this operation, as its credential
    /// verified at admission: the worker that runs it, on any server or a
    /// `by worker` process with no credentials of its own, acts with this
    /// principal's tenant, scopes and repositories, never its own. Absent
    /// on a record from before tenants existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<Principal>,
    /// Branches of `operation.repo` this operation will create, as planned
    /// at admission: counted against its tenant's `max_branches` while it
    /// is queued or running, before they exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub creates: Vec<String>,
    /// The W3C `traceparent` of the operation's admission span, when it is
    /// traced: the worker that runs it, on any server, continues its trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
}

impl StoredOperation {
    /// The tenant this operation belongs to: a record from before tenants
    /// existed belongs to [`DEFAULT_TENANT`].
    pub fn tenant(&self) -> &str {
        match self.tenant.is_empty() {
            true => DEFAULT_TENANT,
            false => &self.tenant,
        }
    }
}

/// A tenant's ceilings that admission checks inside its transaction,
/// against the operation store's own durable rows: what makes them hold
/// across every server sharing the store and across restarts.
#[derive(Default)]
pub struct AdmissionQuota {
    /// Queued and running operations of the tenant at once.
    pub max_running: Option<usize>,
    /// Branches the tenant has, or will have once its queued and running
    /// operations create theirs, across its repositories.
    pub max_branches: Option<usize>,
    /// With `max_branches`: the branches (repository, name) that exist in
    /// the tenant's repositories now. Called inside the admission's
    /// transaction, after the tenant's unfinished operations were read, so
    /// an operation that finishes in between is counted by one or the
    /// other.
    pub existing_branches: Option<ExistingBranches>,
}

/// Reads the branches that exist in a tenant's repositories.
pub type ExistingBranches = Box<dyn Fn() -> io::Result<BTreeSet<(String, String)>> + Send + Sync>;

impl AdmissionQuota {
    fn is_empty(&self) -> bool {
        self.max_running.is_none() && self.max_branches.is_none()
    }

    /// The refusal, if admitting `operation` next to the tenant's
    /// `unfinished` operations would exceed a ceiling.
    fn check(
        &self,
        operation: &StoredOperation,
        unfinished: &[StoredOperation],
    ) -> io::Result<Option<Admission>> {
        if let Some(max) = self.max_running {
            if unfinished.len() >= max {
                return Ok(Some(Admission::Quota {
                    limit: "max_running",
                    max,
                    reserved: unfinished.len(),
                }));
            }
        }
        let Some(max) = self.max_branches else {
            return Ok(None);
        };
        if operation.creates.is_empty() {
            return Ok(None);
        }
        let mut branches: BTreeSet<(String, String)> = unfinished
            .iter()
            .flat_map(|o| {
                o.creates
                    .iter()
                    .map(|b| (o.operation.repo.clone(), b.clone()))
            })
            .collect();
        if let Some(existing) = &self.existing_branches {
            branches.extend(existing()?);
        }
        let reserved = branches.len();
        let after = operation
            .creates
            .iter()
            .filter(|b| !branches.contains(&(operation.operation.repo.clone(), (*b).clone())))
            .count()
            + reserved;
        Ok((after > max).then_some(Admission::Quota {
            limit: "max_branches",
            max,
            reserved,
        }))
    }
}

/// A process that claims queued operations: named like the engine names a
/// lease's holder, so a claim whose process is gone from this host is
/// taken over at once, and any other when its lease expires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worker {
    /// Unique per registry.
    pub id: String,
    /// Host and boot.
    pub host: String,
    pub pid: u32,
    /// The process's start time.
    pub start: String,
}

impl Worker {
    /// This process, under a fresh ID.
    pub fn current() -> Worker {
        let (host, pid, start) = branchyard::process_identity();
        Worker {
            id: format!("w_{}", &branchyard_client::new_key()[..20]),
            host,
            pid,
            start,
        }
    }
}

/// A queued operation claimed by a worker. `fence` is the claim's attempt
/// number: every later write for the claim names it, and is refused once
/// another worker has claimed the operation since.
#[derive(Clone, Debug, PartialEq)]
pub struct Claim {
    pub operation: StoredOperation,
    /// The serializable description of the work (`crate::work::Work`).
    pub work: Value,
    pub fence: i64,
    /// Another worker's claim on it lapsed (its lease expired, or its
    /// process is gone from this host) and this claim took it over.
    pub took_over: bool,
}

/// The lowest priority an operation may carry.
pub const MIN_PRIORITY: i32 = -10;
/// The highest priority an operation may carry; a tenant's
/// `max_priority` may lower it.
pub const MAX_PRIORITY: i32 = 10;
/// How long, by default, an operation waits queued before its effective
/// priority rises by one ([`Scheduling::aging`]).
pub const DEFAULT_AGING: Duration = Duration::from_secs(60);
/// How long, by default, a tenant's claims keep counting toward its usage
/// ([`Scheduling::window`]): each with weight `exp(-age / window)`.
pub const DEFAULT_FAIR_SHARE_WINDOW: Duration = Duration::from_secs(300);

/// How [`OperationStore::claim_next`] chooses among the queued operations a
/// worker may claim (see `docs/server.md#scheduling`):
///
/// 1. the highest *effective priority* first: the operation's priority,
///    plus one for every [`Scheduling::aging`] it has waited, so
///    low-priority work cannot starve;
/// 2. among equal effective priorities, the tenant with the lowest usage
///    per unit of weight: its operations running now (live claims) plus
///    its recent claims, each weighing `exp(-age / window)`, divided by its
///    [`Scheduling::weights`] entry (1 when absent);
/// 3. then the oldest, which also breaks a tie between tenants.
///
/// Worker labels and repositories still filter first. The order is one
/// statement on PostgreSQL and one immediate transaction on SQLite, so two
/// workers racing never claim one operation.
#[derive(Clone, Debug, PartialEq)]
pub struct Scheduling {
    /// Each tenant's weight; a tenant not named weighs 1.
    pub weights: BTreeMap<String, f64>,
    /// Waiting this long raises an operation's effective priority by one;
    /// `None` turns aging off.
    pub aging: Option<Duration>,
    /// How long a claim keeps counting toward its tenant's usage, as the
    /// time constant of its exponential decay; zero counts only what runs.
    pub window: Duration,
    /// A fixed clock, in milliseconds since the Unix epoch, that ages and
    /// decay are measured against; `None` reads the claiming process's
    /// clock. For tests.
    pub clock_ms: Option<u64>,
}

impl Default for Scheduling {
    fn default() -> Scheduling {
        Scheduling {
            weights: BTreeMap::new(),
            aging: Some(DEFAULT_AGING),
            window: DEFAULT_FAIR_SHARE_WINDOW,
            clock_ms: None,
        }
    }
}

impl Scheduling {
    /// The time claims are measured at.
    pub fn at_ms(&self) -> i64 {
        self.clock_ms
            .unwrap_or_else(branchyard_support::time::now_ms) as i64
    }

    fn weight(&self, tenant: &str) -> f64 {
        self.weights
            .get(tenant)
            .copied()
            .filter(|w| w.is_finite() && *w > 0.0)
            .unwrap_or(1.0)
    }

    /// Milliseconds per step of aging; 0 when off.
    fn aging_ms(&self) -> i64 {
        self.aging.map(ms).unwrap_or(0)
    }

    /// `used` claims recorded at `at_ms`, decayed to now.
    fn decayed(&self, used: f64, at_ms: i64, now: i64) -> f64 {
        let window = ms(self.window);
        if window <= 0 {
            return 0.0;
        }
        let age = (now - at_ms).max(0) as f64 / window as f64;
        used * (-age.min(700.0)).exp()
    }

    /// An operation's effective priority: its own, aged.
    pub fn effective(&self, priority: i32, enqueued_ms: i64, now: i64) -> i64 {
        let aging = self.aging_ms();
        let bonus = match aging > 0 {
            true => (now - enqueued_ms).max(0) / aging,
            false => 0,
        };
        i64::from(priority) + bonus
    }

    /// The tenants' weights as the claim's parameters.
    #[cfg(feature = "postgres")]
    fn weight_arrays(&self) -> (Vec<String>, Vec<f64>) {
        self.weights
            .keys()
            .map(|t| (t.clone(), self.weight(t)))
            .unzip()
    }
}

/// An operation as it waits in the queue, for metrics and `by stats`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Queued {
    pub id: String,
    pub repo: String,
    pub tenant: String,
    pub priority: i32,
    pub requires: Vec<String>,
    /// When it was admitted, in milliseconds since the Unix epoch.
    pub enqueued_ms: i64,
    /// A worker holds a live claim on it.
    pub claimed: bool,
}

/// A worker as its last [`OperationStore::beat`] recorded it.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveWorker {
    pub id: String,
    pub host: String,
    pub labels: Vec<String>,
    pub repos: Vec<String>,
    /// Milliseconds since its last beat.
    pub seen_ms_ago: u64,
    /// The harnesses its machine has, as it last advertised them
    /// (docs/harness-lifecycle.md); `None` from a worker that advertises
    /// none.
    pub inventory: Option<branchyard::inventory::Inventory>,
}

/// An inventory as a worker row stores it.
fn inventory_text(
    inventory: Option<&branchyard::inventory::Inventory>,
) -> io::Result<Option<String>> {
    inventory
        .map(serde_json::to_string)
        .transpose()
        .map_err(io::Error::from)
}

/// A stored inventory; one that no longer parses reads as none.
fn inventory_of(text: Option<String>) -> Option<branchyard::inventory::Inventory> {
    text.and_then(|t| serde_json::from_str(&t).ok())
}

/// The labels to add to an operation that runs `harnesses` (catalog IDs)
/// by name on `repo`: `harness:<id>` for each, when some live worker
/// serving the repository advertises an inventory in which all of them
/// can run. Otherwise none: work is steered toward a worker that has its
/// harnesses, never held back because no worker advertises them.
pub fn harness_requirement(
    harnesses: &[String],
    repo: &str,
    workers: &[LiveWorker],
) -> Vec<String> {
    if harnesses.is_empty() {
        return Vec::new();
    }
    let capable = workers
        .iter()
        .filter(|w| w.repos.iter().any(|r| r == repo))
        .filter_map(|w| w.inventory.as_ref())
        .any(|inventory| harnesses.iter().all(|h| inventory.ready(h).is_ok()));
    match capable {
        true => harnesses
            .iter()
            .map(|h| format!("{}{h}", branchyard::inventory::LABEL_PREFIX))
            .collect(),
        false => Vec::new(),
    }
}

/// The router's view of the live workers serving one repository
/// (docs/harness-lifecycle.md): a harness can run when a worker that
/// advertises an inventory says so, or when none advertises one. Workers
/// never install on demand.
#[derive(Debug)]
pub struct WorkersGate {
    repo: String,
    workers: Vec<LiveWorker>,
}

impl WorkersGate {
    pub fn new(repo: &str, workers: Vec<LiveWorker>) -> WorkersGate {
        let workers = workers
            .into_iter()
            .filter(|w| w.repos.iter().any(|r| r == repo) && w.inventory.is_some())
            .collect();
        WorkersGate {
            repo: repo.to_owned(),
            workers,
        }
    }

    pub fn into_arc(self) -> std::sync::Arc<dyn branchyard::inventory::HarnessGate> {
        std::sync::Arc::new(self)
    }
}

impl branchyard::inventory::HarnessGate for WorkersGate {
    fn check(&self, harness: &str) -> Result<(), String> {
        if self.workers.is_empty() {
            return Ok(());
        }
        let mut why = Vec::new();
        for worker in &self.workers {
            let inventory = worker.inventory.as_ref().expect("filtered");
            match inventory.ready(harness) {
                Ok(_) => return Ok(()),
                Err(reason) => why.push(format!("{}: {reason}", worker.id)),
            }
        }
        Err(format!(
            "no live worker serving {} can run it ({})",
            self.repo,
            why.join("; ")
        ))
    }
}

/// Why a queued operation requiring `requires` of `repo` cannot be claimed
/// by any of `workers`, if none of them serving the repository carries
/// every label; `None` when one can (it is only busy).
pub fn unclaimable(requires: &[String], repo: &str, workers: &[LiveWorker]) -> Option<String> {
    if requires.is_empty() {
        return None;
    }
    let serving: Vec<&LiveWorker> = workers
        .iter()
        .filter(|w| w.repos.iter().any(|r| r == repo))
        .collect();
    if serving
        .iter()
        .any(|w| requires.iter().all(|l| w.labels.contains(l)))
    {
        return None;
    }
    let live = match serving.is_empty() {
        true => format!("no live worker serves {repo}"),
        false => format!(
            "live workers serving {repo}: {}",
            serving
                .iter()
                .map(|w| format!("{} on {} [{}]", w.id, w.host, w.labels.join(", ")))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    };
    Some(format!(
        "no live worker carries the labels it requires ({}); {live}",
        requires.join(", ")
    ))
}

/// Whether `label` may name a worker label: 1 to 63 of lowercase letters,
/// digits, `.`, `_`, `-` and `:` (as in `harness:codex`), starting with a
/// letter or digit.
pub fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && label.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | ':')
        })
}

/// What [`OperationStore::admit`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum Admission {
    /// Recorded and enqueued, with its locks taken, in one transaction.
    Admitted,
    /// The caller's key already names this operation; nothing was written.
    Replayed(Box<StoredOperation>),
    /// A branch is held; nothing was written.
    Busy { branch: String, holder: String },
    /// The tenant is at a ceiling of its [`AdmissionQuota`]; nothing was
    /// written.
    Quota {
        limit: &'static str,
        max: usize,
        reserved: usize,
    },
}

/// Durable operation records, dispatch queue and branch locks.
///
/// Every method that changes something commits before it returns, and
/// the commit would survive a crash: the server answers `202 Accepted`
/// only after [`OperationStore::admit`], which is what makes an idempotent
/// retry safe (invariant 4).
pub trait OperationStore: Send + Sync {
    /// Every operation as last saved, oldest first.
    fn load(&self) -> io::Result<Vec<StoredOperation>>;
    /// One operation.
    fn get(&self, id: &str) -> io::Result<Option<StoredOperation>>;
    /// The operation `caller` created with `key`.
    fn by_key(&self, caller: &str, key: &str) -> io::Result<Option<StoredOperation>>;
    /// Replace an operation's record. Only for records without a queue row
    /// (an unfinished record left by a version without the queue); a queued
    /// operation changes through [`OperationStore::start`] and
    /// [`OperationStore::finish`].
    fn save(&self, operation: &StoredOperation) -> io::Result<()>;
    /// Unfinished operations with no queue row: left by a version of the
    /// server that ran operations in memory.
    fn orphans(&self) -> io::Result<Vec<StoredOperation>>;

    /// In one transaction: the operation's record and idempotency binding,
    /// its tenant's `quota` checked against the tenant's queued and running
    /// operations, its branch locks, and its queue row carrying `work`.
    /// Nothing is written when the key already names an operation, when
    /// the tenant is at a ceiling, when a branch is held, or when any write
    /// fails. Admissions of one tenant with a quota take turns.
    fn admit(
        &self,
        operation: &StoredOperation,
        work: &Value,
        quota: &AdmissionQuota,
    ) -> io::Result<Admission>;
    /// The tenant's queued and running operations.
    fn unfinished(&self, tenant: &str) -> io::Result<Vec<StoredOperation>>;
    /// [`OperationStore::claim_next`] with the default [`Scheduling`].
    fn claim(
        &self,
        worker: &Worker,
        repos: &[String],
        labels: &[String],
        lease: Duration,
    ) -> io::Result<Option<Claim>> {
        self.claim_next(worker, repos, labels, lease, &Scheduling::default())
    }
    /// Claim, for `lease`, the queued operation of one of `repos` that no
    /// live claim holds, whose required labels are all among `labels`, and
    /// that `scheduling` puts first: the highest effective priority, then
    /// the tenant with the lowest usage per unit of weight, then the
    /// oldest. A claim whose lease expired, or whose process is gone from
    /// this host, is claimed again under a new fence. Records the claim
    /// toward its tenant's usage in the same transaction.
    fn claim_next(
        &self,
        worker: &Worker,
        repos: &[String],
        labels: &[String],
        lease: Duration,
        scheduling: &Scheduling,
    ) -> io::Result<Option<Claim>>;
    /// Every queued operation, claimed or not, oldest first.
    fn queue(&self) -> io::Result<Vec<Queued>>;
    /// The priority of the latest operation of `repo` that worked on
    /// `branch`, if any: what a child spawned from it inherits.
    fn branch_priority(&self, repo: &str, branch: &str) -> io::Result<Option<i32>>;
    /// Record that `worker`, carrying `labels`, serves `repos` now, with
    /// the harness inventory of its machine when it has one.
    fn beat(
        &self,
        worker: &Worker,
        labels: &[String],
        repos: &[String],
        inventory: Option<&branchyard::inventory::Inventory>,
    ) -> io::Result<()>;
    /// Workers that beat within `within`.
    fn workers(&self, within: Duration) -> io::Result<Vec<LiveWorker>>;
    /// Forget `worker`, which is stopping.
    fn leave(&self, worker: &Worker) -> io::Result<()>;
    /// Extend a claim's lease; false when the claim was lost.
    fn renew(&self, worker: &Worker, id: &str, fence: i64, lease: Duration) -> io::Result<bool>;
    /// Record `operation` (now running) and extend the lease, if the claim
    /// still holds; false otherwise, and nothing is written.
    fn start(
        &self,
        worker: &Worker,
        fence: i64,
        operation: &StoredOperation,
        lease: Duration,
    ) -> io::Result<bool>;
    /// Record `operation`'s outcome, delete its queue row and release its
    /// branch locks, in one transaction, if the claim still holds; false
    /// otherwise, and nothing is written.
    fn finish(&self, worker: &Worker, fence: i64, operation: &StoredOperation) -> io::Result<bool>;
    /// Give a claim back unstarted, for another worker to claim at once.
    fn release(&self, worker: &Worker, id: &str, fence: i64) -> io::Result<()>;
    /// Queued operations of `repos`, claimed or not.
    fn pending(&self, repos: &[String]) -> io::Result<usize>;

    /// Hold `branch` of `repo` for a short synchronous change, under
    /// `token`, until [`OperationStore::unhold`] or `ttl` passes. The
    /// holder's name when the branch is already held.
    fn hold(
        &self,
        repo: &str,
        branch: &str,
        holder: &str,
        token: &str,
        ttl: Duration,
    ) -> io::Result<Option<String>>;
    fn unhold(&self, repo: &str, branch: &str, token: &str) -> io::Result<()>;
    /// Only for a store no other process uses: every claim and hold was
    /// this process's predecessor's, so release them all.
    fn reset(&self) -> io::Result<()>;

    /// A webhook's last delivered feed position (`repo:webhook_id`); `None`
    /// before its first delivery.
    fn load_webhook_cursor(&self, id: &str) -> io::Result<Option<u64>>;
    /// Advance a webhook's cursor. Only ever moves forward; the caller
    /// guarantees that.
    fn save_webhook_cursor(&self, id: &str, cursor: u64) -> io::Result<()>;
    /// Move a cursor from `expected` (`None`: it has none yet) to `next`, in
    /// one compare-and-set; false when another server moved it first. What
    /// lies between is the caller's: servers sharing the store claim each
    /// feed entry once.
    fn claim_webhook_cursor(&self, id: &str, expected: Option<u64>, next: u64) -> io::Result<bool>;

    /// The fleet's service registry, in the same database
    /// (`docs/registry.md`): servers, gateways and whatever registers over
    /// the API. Workers are the workers table's, read as services by
    /// [`worker_services`].
    fn services(&self) -> &dyn branchyard::services::ServiceStore;
}

/// Live workers as service records of kind `worker`: what
/// [`OperationStore::beat`] recorded, leased until `within` after their last
/// beat. A worker is a service; its row is the one place it is kept.
pub fn worker_services(
    workers: &[LiveWorker],
    within: Duration,
    now_ms: u64,
) -> Vec<branchyard::services::Service> {
    use branchyard::services::{Service, ServiceOwner, KIND_WORKER};
    workers
        .iter()
        .map(|w| {
            let seen = now_ms.saturating_sub(w.seen_ms_ago);
            let mut owner = ServiceOwner::remote(w.id.clone(), w.id.clone());
            owner.host = w.host.clone();
            owner.principal = None;
            let mut service = Service::new(KIND_WORKER, owner)
                .with_id(w.id.clone())
                .with("host", w.host.clone())
                .with("labels", w.labels.clone())
                .with("repos", w.repos.clone());
            if let Some(inventory) = &w.inventory {
                let harnesses: Vec<String> = inventory
                    .labels()
                    .iter()
                    .filter_map(|l| l.strip_prefix("harness:").map(str::to_owned))
                    .collect();
                service = service.with("harnesses", harnesses);
            }
            service.registered_ms = seen;
            service.renewed_ms = seen;
            service.changed_ms = seen;
            service.lease_until_ms = deadline(seen, within);
            service
        })
        .collect()
}

/// A duration in milliseconds for scheduling arithmetic, where one past
/// `i64::MAX` just means "effectively forever". Stored values go through
/// `branchyard::store_codec` instead.
fn ms(duration: Duration) -> i64 {
    millis_saturating(duration)
}

fn parse_op(id: &str, body: &str, place: &str) -> io::Result<StoredOperation> {
    serde_json::from_str(body).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{place}operation {id}: {e}"),
        )
    })
}

/// Branch names to lock, each once, in a fixed order so two admissions
/// never wait on each other's locks in opposite orders.
fn lock_order(locks: &[String]) -> Vec<String> {
    let mut locks = locks.to_vec();
    locks.sort();
    locks.dedup();
    locks
}

/// Operations as JSON lines in one append-only file, each save fsynced.
/// The latest line for an ID wins. Compacted when opened.
///
/// What earlier versions of the server kept; [`SqliteStore::open`] imports
/// it once. It has no queue and no locks, so it is not an
/// [`OperationStore`].
pub struct FileStore {
    path: PathBuf,
    file: Mutex<File>,
}

impl FileStore {
    pub fn open(path: impl Into<PathBuf>) -> io::Result<FileStore> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let latest = read_latest(&path)?;
        // Rewrite atomically with one line per operation, dropping any torn
        // final line.
        let temp = path.with_extension("jsonl.tmp");
        {
            let mut out = File::create(&temp)?;
            for op in &latest {
                serde_json::to_writer(&mut out, op)?;
                out.write_all(b"\n")?;
            }
            out.sync_all()?;
        }
        fs::rename(&temp, &path)?;
        if let Some(parent) = path.parent() {
            // Make the rename durable.
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(FileStore {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every operation as last saved, oldest first.
    pub fn load(&self) -> io::Result<Vec<StoredOperation>> {
        read_latest(&self.path)
    }

    /// Append one operation.
    pub fn save(&self, operation: &StoredOperation) -> io::Result<()> {
        let mut line = serde_json::to_vec(operation)?;
        line.push(b'\n');
        let mut file = self.file.lock_recovering("file");
        file.write_all(&line)?;
        file.sync_data()
    }
}

fn read_latest(path: &Path) -> io::Result<Vec<StoredOperation>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut order = Vec::new();
    let mut latest: HashMap<String, StoredOperation> = HashMap::new();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut number = 0;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        number += 1;
        if line.last() != Some(&b'\n') {
            eprintln!(
                "branchyard-server: ignoring a torn final line in {}",
                path.display()
            );
            break;
        }
        let op: StoredOperation = serde_json::from_slice(&line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} line {number}: {e}", path.display()),
            )
        })?;
        let id = op.operation.id.clone();
        if latest.insert(id.clone(), op).is_none() {
            order.push(id);
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|id| latest.remove(&id))
        .collect())
}

/// Operations, queue and locks in SQLite at `DATA-DIR/state.db`, in
/// write-ahead-log mode, each change committed with `synchronous=FULL`
/// before it returns. Changes that read before they write run in
/// `BEGIN IMMEDIATE` transactions, so writers take turns: the single-writer
/// equivalent of PostgreSQL's row locks.
///
/// This is the same embedded database the engine keeps per repository, in
/// its own file: the registry spans every served repository and lives in
/// the server's data directory, not in any one of them.
pub struct SqliteStore {
    path: PathBuf,
    conn: Mutex<rusqlite::Connection>,
}

fn sql(error: rusqlite::Error) -> io::Error {
    io::Error::other(error)
}

const SQLITE_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS operations (
        id TEXT PRIMARY KEY,
        seq INTEGER NOT NULL,
        body TEXT NOT NULL
    );
    CREATE UNIQUE INDEX IF NOT EXISTS operations_idempotency ON operations (
        json_extract(body, '$.idempotency.caller'),
        json_extract(body, '$.idempotency.key')
    );
    CREATE TABLE IF NOT EXISTS operation_queue (
        id TEXT PRIMARY KEY,
        seq INTEGER NOT NULL,
        repo TEXT NOT NULL,
        work TEXT NOT NULL,
        attempt INTEGER NOT NULL DEFAULT 0,
        worker TEXT,
        host TEXT,
        pid INTEGER,
        start TEXT,
        lease_until INTEGER
    );
    CREATE INDEX IF NOT EXISTS operation_queue_seq ON operation_queue (seq);
    CREATE TABLE IF NOT EXISTS branch_locks (
        repo TEXT NOT NULL,
        branch TEXT NOT NULL,
        holder TEXT NOT NULL,
        token TEXT NOT NULL,
        expires_at INTEGER,
        PRIMARY KEY (repo, branch)
    );
    CREATE INDEX IF NOT EXISTS branch_locks_token ON branch_locks (token);
    CREATE TABLE IF NOT EXISTS webhook_cursors (
        id TEXT PRIMARY KEY,
        cursor INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS workers (
        id TEXT PRIMARY KEY,
        host TEXT NOT NULL,
        pid INTEGER NOT NULL,
        labels TEXT NOT NULL,
        repos TEXT NOT NULL,
        seen INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS tenant_usage (
        tenant TEXT PRIMARY KEY,
        used REAL NOT NULL,
        at_ms INTEGER NOT NULL
    );";

/// Columns added to the queue since it was first created, and how: a row
/// from before one existed requires nothing, has priority 0, belongs to the
/// default tenant and counts as admitted at the epoch (so it ages first).
const SQLITE_QUEUE_COLUMNS: &[(&str, &str)] = &[
    (
        "requires",
        "ALTER TABLE operation_queue ADD COLUMN requires TEXT NOT NULL DEFAULT '[]'",
    ),
    (
        "priority",
        "ALTER TABLE operation_queue ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "tenant",
        "ALTER TABLE operation_queue ADD COLUMN tenant TEXT NOT NULL DEFAULT 'default'",
    ),
    (
        "enqueued_ms",
        "ALTER TABLE operation_queue ADD COLUMN enqueued_ms INTEGER NOT NULL DEFAULT 0",
    ),
];

/// Columns added to the workers table since it was first created: a
/// worker that advertises no harness inventory (docs/harness-lifecycle.md)
/// has none.
const SQLITE_WORKER_COLUMNS: &[(&str, &str)] =
    &[("inventory", "ALTER TABLE workers ADD COLUMN inventory TEXT")];

/// Columns added since the tables were first created. Run inside the
/// opener's `BEGIN IMMEDIATE`, so openers take turns reading a column's
/// absence and adding it.
fn sqlite_migrate(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    for (table, columns) in [
        ("operation_queue", SQLITE_QUEUE_COLUMNS),
        ("workers", SQLITE_WORKER_COLUMNS),
    ] {
        for (column, statement) in columns {
            let has: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
                [table, column],
                |r| r.get(0),
            )?;
            if has == 0 {
                conn.execute_batch(statement)?;
            }
        }
    }
    Ok(())
}

/// Switch to write-ahead logging. A file still in rollback mode that
/// another connection is switching too answers busy at once, without
/// waiting on the busy timeout, so it is asked again for as long.
fn sqlite_wal(conn: &rusqlite::Connection) -> rusqlite::Result<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)) {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            mode => return mode,
        }
    }
}

fn sqlite_now() -> i64 {
    branchyard_support::time::now_ms() as i64
}

type Conn = rusqlite::Connection;

fn sqlite_op(conn: &Conn, sql_text: &str, param: &str) -> io::Result<Option<StoredOperation>> {
    let row: Option<(String, String)> = conn
        .query_row(sql_text, [param], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .map_err(sql)?;
    row.map(|(id, body)| parse_op(&id, &body, "")).transpose()
}

fn sqlite_by_key(conn: &Conn, caller: &str, key: &str) -> io::Result<Option<StoredOperation>> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT id, body FROM operations \
             WHERE json_extract(body, '$.idempotency.caller') = ?1 \
               AND json_extract(body, '$.idempotency.key') = ?2",
            [caller, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql)?;
    row.map(|(id, body)| parse_op(&id, &body, "")).transpose()
}

impl SqliteStore {
    /// Open or create the database at `path`. If `legacy` names an
    /// `operations.jsonl` left by an earlier version, its operations are
    /// imported once and the file is renamed to `operations.jsonl.imported`.
    pub fn open(path: impl Into<PathBuf>, legacy: Option<&Path>) -> io::Result<SqliteStore> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut conn = rusqlite::Connection::open(&path).map_err(sql)?;
        conn.busy_timeout(Duration::from_secs(30)).map_err(sql)?;
        let mode = sqlite_wal(&conn).map_err(sql)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(io::Error::other(format!(
                "{} could not use write-ahead logging (journal mode {mode})",
                path.display()
            )));
        }
        conn.execute_batch("PRAGMA synchronous = FULL;")
            .map_err(sql)?;
        // Servers opening one file at once take turns: the migration
        // reads a column's absence, then adds it.
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql)?;
        tx.execute_batch(SQLITE_SCHEMA).map_err(sql)?;
        tx.execute_batch(branchyard::services::sqlite::SCHEMA)
            .map_err(sql)?;
        sqlite_migrate(&tx).map_err(sql)?;
        tx.commit().map_err(sql)?;
        let store = SqliteStore {
            path,
            conn: Mutex::new(conn),
        };
        if let Some(legacy) = legacy.filter(|p| p.is_file()) {
            for op in read_latest(legacy)? {
                store.insert(&op, false)?;
            }
            let imported = legacy.with_extension("jsonl.imported");
            fs::rename(legacy, &imported)?;
        }
        Ok(store)
    }

    /// An in-memory database: for tests and embedding.
    pub fn memory() -> SqliteStore {
        let conn = rusqlite::Connection::open_in_memory().expect("an in-memory database");
        conn.execute_batch(SQLITE_SCHEMA)
            .expect("the schema on an in-memory database");
        conn.execute_batch(branchyard::services::sqlite::SCHEMA)
            .expect("the schema on an in-memory database");
        sqlite_migrate(&conn).expect("the schema on an in-memory database");
        SqliteStore {
            path: PathBuf::from(":memory:"),
            conn: Mutex::new(conn),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Conn> {
        self.conn.lock_recovering("conn")
    }

    /// Run `f` in a `BEGIN IMMEDIATE` transaction, committed when it
    /// returns `Ok((value, true))` and rolled back otherwise.
    fn immediate<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> io::Result<(T, bool)>,
    ) -> io::Result<T> {
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql)?;
        let (value, commit) = f(&tx)?;
        match commit {
            true => tx.commit().map_err(sql)?,
            false => tx.rollback().map_err(sql)?,
        }
        Ok(value)
    }

    /// Save `operation`; `replace` keeps an existing one otherwise.
    fn insert(&self, operation: &StoredOperation, replace: bool) -> io::Result<()> {
        sqlite_insert(&self.conn(), operation, replace).map(|_| ())
    }
}

/// Insert or replace an operation's record; the number of rows written.
fn sqlite_insert(conn: &Conn, operation: &StoredOperation, replace: bool) -> io::Result<usize> {
    let body = serde_json::to_string(operation)?;
    let conflict = match replace {
        true => "ON CONFLICT (id) DO UPDATE SET body = excluded.body",
        false => "ON CONFLICT DO NOTHING",
    };
    conn.execute(
        &format!(
            "INSERT INTO operations (id, seq, body) \
             VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM operations), ?2) {conflict}"
        ),
        rusqlite::params![operation.operation.id, body],
    )
    .map_err(sql)
}

/// The tenant's operations with a queue row: queued or running. A record
/// without a tenant (from before tenants existed) is the default tenant's.
fn sqlite_unfinished(conn: &Conn, tenant: &str) -> io::Result<Vec<StoredOperation>> {
    let mut statement = conn
        .prepare(
            "SELECT o.id, o.body FROM operation_queue q JOIN operations o ON o.id = q.id \
             WHERE COALESCE(NULLIF(json_extract(o.body, '$.tenant'), ''), ?2) = ?1 \
             ORDER BY q.seq",
        )
        .map_err(sql)?;
    let rows: Vec<(String, String)> = statement
        .query_map([tenant, DEFAULT_TENANT], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    rows.iter()
        .map(|(id, body)| parse_op(id, body, ""))
        .collect()
}

/// Release claims held by processes gone from this host.
fn sqlite_reap(conn: &Conn, worker: &Worker, now: i64) -> io::Result<()> {
    let mut statement = conn
        .prepare(
            "SELECT id, attempt, pid, start FROM operation_queue \
             WHERE host = ?1 AND worker IS NOT NULL AND worker <> ?2 AND lease_until > ?3",
        )
        .map_err(sql)?;
    let rows: Vec<(String, i64, i64, String)> = statement
        .query_map(rusqlite::params![worker.host, worker.id, now], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    for (id, attempt, pid, start) in rows {
        if branchyard::process_gone(&worker.host, from_db_u32("pid", pid)?, &start) {
            conn.execute(
                "UPDATE operation_queue SET worker = NULL, lease_until = NULL \
                 WHERE id = ?1 AND attempt = ?2",
                rusqlite::params![id, attempt],
            )
            .map_err(sql)?;
        }
    }
    Ok(())
}

/// Each tenant's rank by usage per unit of weight (see [`Scheduling`]):
/// its live claims at `clock` (the database's time, as leases are measured)
/// plus its recorded claims decayed to `now`, over its weight.
fn sqlite_shares(
    conn: &Conn,
    scheduling: &Scheduling,
    clock: i64,
    now: i64,
) -> io::Result<BTreeMap<String, i64>> {
    let mut usage: BTreeMap<String, f64> = BTreeMap::new();
    let mut statement = conn
        .prepare("SELECT tenant, used, at_ms FROM tenant_usage")
        .map_err(sql)?;
    let recorded: Vec<(String, f64, i64)> = statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    for (tenant, used, at_ms) in recorded {
        *usage.entry(tenant).or_default() += scheduling.decayed(used, at_ms, now);
    }
    let mut statement = conn
        .prepare(
            "SELECT tenant, COUNT(*) FROM operation_queue \
             WHERE worker IS NOT NULL AND lease_until > ?1 GROUP BY tenant",
        )
        .map_err(sql)?;
    let running: Vec<(String, i64)> = statement
        .query_map([clock], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    for (tenant, count) in running {
        *usage.entry(tenant).or_default() += count as f64;
    }
    let shares: Vec<(String, f64)> = usage
        .into_iter()
        .map(|(tenant, used)| {
            let share = used / scheduling.weight(&tenant);
            (tenant, share)
        })
        .collect();
    Ok(share_ranks(&shares))
}

/// Tenants ranked by share, lowest first, equal shares sharing a rank: what
/// the SQLite claim orders by, so no share crosses into SQL as a decimal.
/// A tenant with no usage has share 0, ranked under the empty name.
fn share_ranks(shares: &[(String, f64)]) -> BTreeMap<String, i64> {
    let mut values: Vec<f64> = shares.iter().map(|(_, s)| *s).collect();
    values.push(0.0);
    values.sort_by(|a, b| a.total_cmp(b));
    values.dedup();
    let rank = |share: f64| values.partition_point(|v| v.total_cmp(&share).is_lt()) as i64;
    shares
        .iter()
        .map(|(tenant, share)| (tenant.clone(), rank(*share)))
        .chain(std::iter::once((String::new(), rank(0.0))))
        .collect()
}

/// Count a claim toward `tenant`'s usage: its recorded claims decayed to
/// `now`, plus one.
fn sqlite_count_claim(
    conn: &Conn,
    scheduling: &Scheduling,
    tenant: &str,
    now: i64,
) -> io::Result<()> {
    let recorded: Option<(f64, i64)> = conn
        .query_row(
            "SELECT used, at_ms FROM tenant_usage WHERE tenant = ?1",
            [tenant],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql)?;
    let (used, at_ms) = match recorded {
        Some((used, at_ms)) => (scheduling.decayed(used, at_ms, now) + 1.0, at_ms.max(now)),
        None => (1.0, now),
    };
    conn.execute(
        "INSERT INTO tenant_usage (tenant, used, at_ms) VALUES (?1, ?2, ?3) \
         ON CONFLICT (tenant) DO UPDATE SET used = excluded.used, at_ms = excluded.at_ms",
        rusqlite::params![tenant, used, at_ms],
    )
    .map_err(sql)?;
    Ok(())
}

impl branchyard::services::ServiceStore for SqliteStore {
    fn transact(
        &self,
        f: &mut (dyn FnMut(&mut dyn branchyard::services::Rows) -> io::Result<()> + Send),
    ) -> io::Result<()> {
        branchyard::services::sqlite::transact(&mut self.conn(), f)
    }
}

impl OperationStore for SqliteStore {
    fn services(&self) -> &dyn branchyard::services::ServiceStore {
        self
    }

    fn load(&self) -> io::Result<Vec<StoredOperation>> {
        let conn = self.conn();
        let mut statement = conn
            .prepare("SELECT id, body FROM operations ORDER BY seq")
            .map_err(sql)?;
        let rows = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(sql)?;
        let place = format!("{} ", self.path.display());
        let mut ops = Vec::new();
        for row in rows {
            let (id, body) = row.map_err(sql)?;
            ops.push(parse_op(&id, &body, &place)?);
        }
        Ok(ops)
    }

    fn get(&self, id: &str) -> io::Result<Option<StoredOperation>> {
        sqlite_op(
            &self.conn(),
            "SELECT id, body FROM operations WHERE id = ?1",
            id,
        )
    }

    fn by_key(&self, caller: &str, key: &str) -> io::Result<Option<StoredOperation>> {
        sqlite_by_key(&self.conn(), caller, key)
    }

    fn save(&self, operation: &StoredOperation) -> io::Result<()> {
        self.insert(operation, true)
    }

    fn orphans(&self) -> io::Result<Vec<StoredOperation>> {
        let conn = self.conn();
        let mut statement = conn
            .prepare(
                "SELECT id, body FROM operations \
                 WHERE json_extract(body, '$.operation.state') IN ('queued', 'running') \
                   AND id NOT IN (SELECT id FROM operation_queue) ORDER BY seq",
            )
            .map_err(sql)?;
        let rows: Vec<(String, String)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)?;
        rows.iter()
            .map(|(id, body)| parse_op(id, body, ""))
            .collect()
    }

    fn admit(
        &self,
        operation: &StoredOperation,
        work: &Value,
        quota: &AdmissionQuota,
    ) -> io::Result<Admission> {
        let work = serde_json::to_string(work)?;
        let op = &operation.operation;
        // `BEGIN IMMEDIATE` makes every admission take its turn, so the
        // tenant's count below cannot change before this one commits.
        self.immediate(|tx| {
            if let Some(idem) = &operation.idempotency {
                if let Some(existing) = sqlite_by_key(tx, &idem.caller, &idem.key)? {
                    return Ok((Admission::Replayed(Box::new(existing)), false));
                }
            }
            if !quota.is_empty() {
                let unfinished = sqlite_unfinished(tx, operation.tenant())?;
                if let Some(refused) = quota.check(operation, &unfinished)? {
                    return Ok((refused, false));
                }
            }
            let now = sqlite_now();
            for branch in lock_order(&operation.locks) {
                tx.execute(
                    "DELETE FROM branch_locks WHERE repo = ?1 AND branch = ?2 \
                     AND expires_at IS NOT NULL AND expires_at <= ?3",
                    rusqlite::params![op.repo, branch, now],
                )
                .map_err(sql)?;
                let holder: Option<String> = tx
                    .query_row(
                        "SELECT holder FROM branch_locks WHERE repo = ?1 AND branch = ?2",
                        [&op.repo, &branch],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sql)?;
                if let Some(holder) = holder {
                    return Ok((Admission::Busy { branch, holder }, false));
                }
                tx.execute(
                    "INSERT INTO branch_locks (repo, branch, holder, token) VALUES (?1, ?2, ?3, ?3)",
                    rusqlite::params![op.repo, branch, op.id],
                )
                .map_err(sql)?;
            }
            sqlite_insert(tx, operation, false)?;
            tx.execute(
                "INSERT INTO operation_queue \
                     (id, seq, repo, work, requires, priority, tenant, enqueued_ms) \
                 VALUES (?1, (SELECT seq FROM operations WHERE id = ?1), ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    op.id,
                    op.repo,
                    work,
                    serde_json::to_string(&op.requires)?,
                    op.priority,
                    operation.tenant(),
                    to_db("created_at_ms", op.created_at_ms)?
                ],
            )
            .map_err(sql)?;
            Ok((Admission::Admitted, true))
        })
    }

    fn unfinished(&self, tenant: &str) -> io::Result<Vec<StoredOperation>> {
        sqlite_unfinished(&self.conn(), tenant)
    }

    fn claim_next(
        &self,
        worker: &Worker,
        repos: &[String],
        labels: &[String],
        lease: Duration,
        scheduling: &Scheduling,
    ) -> io::Result<Option<Claim>> {
        let repos = serde_json::to_string(repos)?;
        let labels = serde_json::to_string(labels)?;
        self.immediate(|tx| {
            let clock = sqlite_now();
            let now = scheduling.at_ms();
            sqlite_reap(tx, worker, clock)?;
            // Each tenant's usage per unit of weight, from its live claims
            // and its decayed recent ones: computed here, inside the
            // immediate transaction, and looked up by the order below.
            let shares = sqlite_shares(tx, scheduling, clock, now)?;
            let next: Option<(String, i64, String, Option<String>, String)> = tx
                .query_row(
                    "SELECT id, attempt, work, worker, tenant FROM operation_queue \
                     WHERE repo IN (SELECT value FROM json_each(?1)) \
                       AND (lease_until IS NULL OR lease_until <= ?2) \
                       AND NOT EXISTS (SELECT 1 FROM json_each(requires) r \
                           WHERE r.value NOT IN (SELECT value FROM json_each(?3))) \
                     ORDER BY priority \
                         + COALESCE(MAX(?4 - enqueued_ms, 0) / NULLIF(?5, 0), 0) DESC, \
                       COALESCE((SELECT s.value FROM json_each(?6) s WHERE s.key = tenant), \
                           json_extract(?6, '$.\"\"')), \
                       seq \
                     LIMIT 1",
                    rusqlite::params![
                        repos,
                        clock,
                        labels,
                        now,
                        scheduling.aging_ms(),
                        serde_json::to_string(&shares)?
                    ],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()
                .map_err(sql)?;
            let Some((id, attempt, work, prior, tenant)) = next else {
                return Ok((None, false));
            };
            let fence = attempt + 1;
            tx.execute(
                "UPDATE operation_queue SET attempt = ?2, worker = ?3, host = ?4, pid = ?5, \
                 start = ?6, lease_until = ?7 WHERE id = ?1",
                rusqlite::params![
                    id,
                    fence,
                    worker.id,
                    worker.host,
                    worker.pid,
                    worker.start,
                    clock.saturating_add(ms(lease))
                ],
            )
            .map_err(sql)?;
            sqlite_count_claim(tx, scheduling, &tenant, now)?;
            let operation = sqlite_op(tx, "SELECT id, body FROM operations WHERE id = ?1", &id)?
                .ok_or_else(|| io::Error::other(format!("queued operation {id} has no record")))?;
            let work = serde_json::from_str(&work)?;
            Ok((
                Some(Claim {
                    operation,
                    work,
                    fence,
                    took_over: prior.is_some(),
                }),
                true,
            ))
        })
    }

    fn queue(&self) -> io::Result<Vec<Queued>> {
        let conn = self.conn();
        let mut statement = conn
            .prepare(
                "SELECT id, repo, tenant, priority, requires, enqueued_ms, \
                     (worker IS NOT NULL AND lease_until > ?1) \
                 FROM operation_queue ORDER BY seq",
            )
            .map_err(sql)?;
        let rows: Vec<(String, String, String, i32, String, i64, bool)> = statement
            .query_map([sqlite_now()], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)?;
        rows.into_iter()
            .map(
                |(id, repo, tenant, priority, requires, enqueued_ms, claimed)| {
                    Ok(Queued {
                        id,
                        repo,
                        tenant,
                        priority,
                        requires: serde_json::from_str(&requires)?,
                        enqueued_ms,
                        claimed,
                    })
                },
            )
            .collect()
    }

    fn branch_priority(&self, repo: &str, branch: &str) -> io::Result<Option<i32>> {
        let found: Option<Option<i64>> = self
            .conn()
            .query_row(
                "SELECT json_extract(body, '$.operation.priority') FROM operations \
                 WHERE json_extract(body, '$.operation.repo') = ?1 \
                   AND EXISTS (SELECT 1 FROM json_each(body, '$.operation.branches') b \
                       WHERE b.value = ?2) \
                 ORDER BY seq DESC LIMIT 1",
                [repo, branch],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql)?;
        // No priority recorded is the default, `0`; a stored one outside
        // `i32` is an error rather than a wrapped value.
        Ok(found
            .map(|p| p.map_or(Ok(0), |p| from_db_i32("priority", p)))
            .transpose()?)
    }

    fn beat(
        &self,
        worker: &Worker,
        labels: &[String],
        repos: &[String],
        inventory: Option<&branchyard::inventory::Inventory>,
    ) -> io::Result<()> {
        self.conn()
            .execute(
                "INSERT INTO workers (id, host, pid, labels, repos, seen, inventory) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT (id) DO UPDATE SET \
                 labels = excluded.labels, repos = excluded.repos, seen = excluded.seen, \
                 inventory = excluded.inventory",
                rusqlite::params![
                    worker.id,
                    worker.host,
                    worker.pid,
                    serde_json::to_string(labels)?,
                    serde_json::to_string(repos)?,
                    sqlite_now(),
                    inventory_text(inventory)?
                ],
            )
            .map_err(sql)?;
        Ok(())
    }

    fn workers(&self, within: Duration) -> io::Result<Vec<LiveWorker>> {
        let now = sqlite_now();
        let conn = self.conn();
        let mut statement = conn
            .prepare(
                "SELECT id, host, labels, repos, seen, inventory FROM workers \
                 WHERE seen > ?1 ORDER BY id",
            )
            .map_err(sql)?;
        let rows = statement
            .query_map([now.saturating_sub(ms(within))], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })
            .map_err(sql)?;
        let mut live = Vec::new();
        for row in rows {
            let (id, host, labels, repos, seen, inventory) = row.map_err(sql)?;
            live.push(LiveWorker {
                id,
                host,
                labels: serde_json::from_str(&labels)?,
                repos: serde_json::from_str(&repos)?,
                seen_ms_ago: (now - seen).max(0) as u64,
                inventory: inventory_of(inventory),
            });
        }
        Ok(live)
    }

    fn leave(&self, worker: &Worker) -> io::Result<()> {
        self.conn()
            .execute("DELETE FROM workers WHERE id = ?1", [&worker.id])
            .map_err(sql)?;
        Ok(())
    }

    fn renew(&self, worker: &Worker, id: &str, fence: i64, lease: Duration) -> io::Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE operation_queue SET lease_until = ?4 \
                 WHERE id = ?1 AND attempt = ?2 AND worker = ?3",
                rusqlite::params![id, fence, worker.id, sqlite_now().saturating_add(ms(lease))],
            )
            .map_err(sql)?;
        Ok(changed == 1)
    }

    fn start(
        &self,
        worker: &Worker,
        fence: i64,
        operation: &StoredOperation,
        lease: Duration,
    ) -> io::Result<bool> {
        self.immediate(|tx| {
            let held = tx
                .execute(
                    "UPDATE operation_queue SET lease_until = ?4 \
                     WHERE id = ?1 AND attempt = ?2 AND worker = ?3",
                    rusqlite::params![
                        operation.operation.id,
                        fence,
                        worker.id,
                        sqlite_now().saturating_add(ms(lease))
                    ],
                )
                .map_err(sql)?;
            if held != 1 {
                return Ok((false, false));
            }
            sqlite_insert(tx, operation, true)?;
            Ok((true, true))
        })
    }

    fn finish(&self, worker: &Worker, fence: i64, operation: &StoredOperation) -> io::Result<bool> {
        let id = &operation.operation.id;
        self.immediate(|tx| {
            let held = tx
                .execute(
                    "DELETE FROM operation_queue WHERE id = ?1 AND attempt = ?2 AND worker = ?3",
                    rusqlite::params![id, fence, worker.id],
                )
                .map_err(sql)?;
            if held != 1 {
                return Ok((false, false));
            }
            sqlite_insert(tx, operation, true)?;
            tx.execute("DELETE FROM branch_locks WHERE token = ?1", [id])
                .map_err(sql)?;
            Ok((true, true))
        })
    }

    fn release(&self, worker: &Worker, id: &str, fence: i64) -> io::Result<()> {
        self.conn()
            .execute(
                "UPDATE operation_queue SET worker = NULL, lease_until = NULL \
                 WHERE id = ?1 AND attempt = ?2 AND worker = ?3",
                rusqlite::params![id, fence, worker.id],
            )
            .map_err(sql)?;
        Ok(())
    }

    fn pending(&self, repos: &[String]) -> io::Result<usize> {
        let repos = serde_json::to_string(repos)?;
        let count: i64 = self
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM operation_queue \
                 WHERE repo IN (SELECT value FROM json_each(?1))",
                [repos],
                |r| r.get(0),
            )
            .map_err(sql)?;
        Ok(from_db_usize("pending", count)?)
    }

    fn hold(
        &self,
        repo: &str,
        branch: &str,
        holder: &str,
        token: &str,
        ttl: Duration,
    ) -> io::Result<Option<String>> {
        self.immediate(|tx| {
            let now = sqlite_now();
            tx.execute(
                "DELETE FROM branch_locks WHERE repo = ?1 AND branch = ?2 \
                 AND expires_at IS NOT NULL AND expires_at <= ?3",
                rusqlite::params![repo, branch, now],
            )
            .map_err(sql)?;
            let existing: Option<String> = tx
                .query_row(
                    "SELECT holder FROM branch_locks WHERE repo = ?1 AND branch = ?2",
                    [repo, branch],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            if existing.is_some() {
                return Ok((existing, false));
            }
            tx.execute(
                "INSERT INTO branch_locks (repo, branch, holder, token, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![repo, branch, holder, token, now.saturating_add(ms(ttl))],
            )
            .map_err(sql)?;
            Ok((None, true))
        })
    }

    fn unhold(&self, repo: &str, branch: &str, token: &str) -> io::Result<()> {
        self.conn()
            .execute(
                "DELETE FROM branch_locks WHERE repo = ?1 AND branch = ?2 AND token = ?3",
                [repo, branch, token],
            )
            .map_err(sql)?;
        Ok(())
    }

    fn reset(&self) -> io::Result<()> {
        self.conn()
            .execute_batch(
                "BEGIN IMMEDIATE;
                 UPDATE operation_queue SET worker = NULL, lease_until = NULL;
                 DELETE FROM branch_locks WHERE expires_at IS NOT NULL;
                 COMMIT;",
            )
            .map_err(sql)
    }

    fn load_webhook_cursor(&self, id: &str) -> io::Result<Option<u64>> {
        let found: Option<i64> = self
            .conn()
            .query_row(
                "SELECT cursor FROM webhook_cursors WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql)?;
        Ok(from_db_opt("cursor", found)?)
    }

    fn save_webhook_cursor(&self, id: &str, cursor: u64) -> io::Result<()> {
        let cursor = to_db("cursor", cursor)?;
        self.conn()
            .execute(
                "INSERT INTO webhook_cursors (id, cursor) VALUES (?1, ?2) \
                 ON CONFLICT (id) DO UPDATE SET cursor = excluded.cursor",
                rusqlite::params![id, cursor],
            )
            .map_err(sql)?;
        Ok(())
    }

    fn claim_webhook_cursor(&self, id: &str, expected: Option<u64>, next: u64) -> io::Result<bool> {
        let next = to_db("next", next)?;
        let expected = to_db_opt("expected", expected)?;
        let conn = self.conn();
        let changed = match expected {
            None => conn.execute(
                "INSERT INTO webhook_cursors (id, cursor) VALUES (?1, ?2) \
                 ON CONFLICT (id) DO NOTHING",
                rusqlite::params![id, next],
            ),
            Some(from) => conn.execute(
                "UPDATE webhook_cursors SET cursor = ?3 WHERE id = ?1 AND cursor = ?2",
                rusqlite::params![id, from, next],
            ),
        }
        .map_err(sql)?;
        Ok(changed == 1)
    }
}

/// Operations, queue and locks in a PostgreSQL database, in the
/// connection's `search_path` schema, each change committed with
/// `synchronous_commit = on` before it returns.
///
/// Several servers may share one schema. Claims take the oldest claimable
/// row with `FOR UPDATE SKIP LOCKED`, so two workers never claim one row;
/// leases are measured by the database's clock, so servers' clocks need
/// not agree. Two admissions with one idempotency key, or for one branch,
/// meet at a unique index and the second waits for the first to commit.
///
/// Every call runs on a thread of its own, since the registry is called
/// from the server's asynchronous handlers and the client blocks.
#[cfg(feature = "postgres")]
pub struct PostgresStore {
    url: String,
    conn: Mutex<Option<postgres::Client>>,
}

/// The registry's schema, one object per step: its name (a relation, or
/// `table.column` for a column added since the table was first created)
/// and the statement that makes it.
///
/// [`PostgresStore::open`] runs only the steps whose object is missing,
/// each in a transaction of its own. A statement here locks its table even
/// when the object exists already (`CREATE INDEX IF NOT EXISTS` takes a
/// `SHARE` lock, `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` an `ACCESS
/// EXCLUSIVE` one), and a server opening the registry while another works
/// on it must not hold one table's lock while it waits for another's: the
/// other's `start` and `finish` write the queue, then the operation, and
/// the two would wait for each other.
#[cfg(feature = "postgres")]
const PG_SCHEMA: &[(&str, &str)] = &[
    (
        "by_operations",
        "CREATE TABLE IF NOT EXISTS by_operations (
            id TEXT PRIMARY KEY,
            seq BIGINT GENERATED ALWAYS AS IDENTITY,
            body TEXT NOT NULL
        )",
    ),
    (
        "by_operations_idempotency",
        "CREATE UNIQUE INDEX IF NOT EXISTS by_operations_idempotency ON by_operations (
            ((body::jsonb) #>> '{idempotency,caller}'),
            ((body::jsonb) #>> '{idempotency,key}')
        )",
    ),
    (
        "by_operation_queue",
        "CREATE TABLE IF NOT EXISTS by_operation_queue (
            id TEXT PRIMARY KEY REFERENCES by_operations (id),
            seq BIGINT GENERATED ALWAYS AS IDENTITY,
            repo TEXT NOT NULL,
            work TEXT NOT NULL,
            attempt BIGINT NOT NULL DEFAULT 0,
            worker TEXT,
            host TEXT,
            pid BIGINT,
            start TEXT,
            lease_until TIMESTAMPTZ
        )",
    ),
    (
        "by_operation_queue_seq",
        "CREATE INDEX IF NOT EXISTS by_operation_queue_seq ON by_operation_queue (seq)",
    ),
    (
        "by_branch_locks",
        "CREATE TABLE IF NOT EXISTS by_branch_locks (
            repo TEXT NOT NULL,
            branch TEXT NOT NULL,
            holder TEXT NOT NULL,
            token TEXT NOT NULL,
            expires_at TIMESTAMPTZ,
            PRIMARY KEY (repo, branch)
        )",
    ),
    (
        "by_branch_locks_token",
        "CREATE INDEX IF NOT EXISTS by_branch_locks_token ON by_branch_locks (token)",
    ),
    (
        "by_webhook_cursors",
        "CREATE TABLE IF NOT EXISTS by_webhook_cursors (
            id TEXT PRIMARY KEY,
            cursor BIGINT NOT NULL
        )",
    ),
    (
        "by_operation_queue.requires",
        "ALTER TABLE by_operation_queue \
         ADD COLUMN IF NOT EXISTS requires TEXT[] NOT NULL DEFAULT '{}'",
    ),
    (
        "by_workers",
        "CREATE TABLE IF NOT EXISTS by_workers (
            id TEXT PRIMARY KEY,
            host TEXT NOT NULL,
            pid BIGINT NOT NULL,
            labels TEXT[] NOT NULL,
            repos TEXT[] NOT NULL,
            seen TIMESTAMPTZ NOT NULL
        )",
    ),
    // Scheduling (docs/server.md#scheduling). A queue row from before
    // these has priority 0, belongs to the default tenant and counts as
    // admitted at the epoch, so it ages first.
    (
        "by_operation_queue.priority",
        "ALTER TABLE by_operation_queue \
         ADD COLUMN IF NOT EXISTS priority INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "by_operation_queue.tenant",
        "ALTER TABLE by_operation_queue \
         ADD COLUMN IF NOT EXISTS tenant TEXT NOT NULL DEFAULT 'default'",
    ),
    (
        "by_operation_queue.enqueued_ms",
        "ALTER TABLE by_operation_queue \
         ADD COLUMN IF NOT EXISTS enqueued_ms BIGINT NOT NULL DEFAULT 0",
    ),
    (
        "by_tenant_usage",
        "CREATE TABLE IF NOT EXISTS by_tenant_usage (
            tenant TEXT PRIMARY KEY,
            used DOUBLE PRECISION NOT NULL,
            at_ms BIGINT NOT NULL
        )",
    ),
    // A worker's harness inventory (docs/harness-lifecycle.md), as JSON;
    // a row from before it, or a worker that advertises none, has NULL.
    (
        "by_workers.inventory",
        "ALTER TABLE by_workers ADD COLUMN IF NOT EXISTS inventory TEXT",
    ),
    // The fleet's service registry (docs/registry.md).
    branchyard::services::pg::SCHEMA[0],
    branchyard::services::pg::SCHEMA[1],
    branchyard::services::pg::SCHEMA[2],
];

/// The claim: one statement, so it is atomic on its own.
///
/// `share` is each tenant's usage per unit of weight: its recorded claims
/// decayed to now (`$9`) with time constant `$10` ms (0: none count), plus
/// its live claims, over its weight (`$11`/`$12`, else 1). `next` orders the
/// claimable rows by effective priority (priority plus one per `$8` ms
/// waited; 0: no aging), then share, then age, and locks the first one no
/// other worker has locked (`FOR UPDATE OF q SKIP LOCKED`): a row another
/// claim took meanwhile fails the lease condition when it is rechecked on
/// the locked row's latest version, so it is never claimed twice. `claimed`
/// takes it under the next fence, and `counted` adds the claim to its
/// tenant's usage, decayed first, in the same statement.
#[cfg(feature = "postgres")]
const PG_CLAIM: &str = "
    WITH share AS (
        SELECT t.tenant,
            (COALESCE((SELECT u.used * CASE WHEN $10::bigint > 0 THEN exp(-LEAST(
                    GREATEST($9::bigint - u.at_ms, 0)::float8 / $10::bigint, 700)) ELSE 0 END
                 FROM by_tenant_usage u WHERE u.tenant = t.tenant), 0)
             + (SELECT count(*) FROM by_operation_queue r
                WHERE r.tenant = t.tenant AND r.worker IS NOT NULL
                  AND r.lease_until > clock_timestamp()))
            / COALESCE((SELECT w.weight FROM unnest($11::text[], $12::float8[]) AS w (tenant, weight)
                        WHERE w.tenant = t.tenant), 1) AS share
        FROM (SELECT DISTINCT tenant FROM by_operation_queue) t
    ),
    next AS (
        SELECT q.id, q.worker AS prior FROM by_operation_queue q
        JOIN share s ON s.tenant = q.tenant
        WHERE q.repo = ANY($1)
          AND (q.lease_until IS NULL OR q.lease_until <= clock_timestamp())
          AND q.requires <@ $7::text[]
        ORDER BY q.priority
                 + COALESCE(GREATEST($9::bigint - q.enqueued_ms, 0) / NULLIF($8::bigint, 0), 0) DESC,
            s.share, q.seq
        LIMIT 1
        FOR UPDATE OF q SKIP LOCKED
    ),
    claimed AS (
        UPDATE by_operation_queue q SET attempt = q.attempt + 1, worker = $2,
            host = $3, pid = $4, start = $5,
            lease_until = clock_timestamp() + $6::float8 * interval '1 millisecond'
        FROM next WHERE q.id = next.id
        RETURNING q.id, q.attempt, q.work, q.tenant, next.prior IS NOT NULL AS took_over
    ),
    counted AS (
        INSERT INTO by_tenant_usage AS u (tenant, used, at_ms)
        SELECT tenant, 1, $9::bigint FROM claimed
        ON CONFLICT (tenant) DO UPDATE SET
            used = u.used * CASE WHEN $10::bigint > 0 THEN exp(-LEAST(
                GREATEST(EXCLUDED.at_ms - u.at_ms, 0)::float8 / $10::bigint, 700)) ELSE 0 END + 1,
            at_ms = GREATEST(u.at_ms, EXCLUDED.at_ms)
    )
    SELECT c.id, c.attempt, c.work, (SELECT body FROM by_operations o WHERE o.id = c.id),
        c.took_over
    FROM claimed c";

/// Which of `names` (see [`PG_SCHEMA`]) are missing, read from the
/// catalogs on the connection's search path without locking any table.
#[cfg(feature = "postgres")]
fn pg_missing(
    c: &mut impl postgres::GenericClient,
    names: &[&str],
) -> Result<Vec<String>, postgres::Error> {
    let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    let rows = c.query(
        "SELECT n FROM unnest($1::text[]) WITH ORDINALITY AS t (n, i) \
         WHERE CASE WHEN strpos(n, '.') = 0 THEN to_regclass(n) IS NULL \
             ELSE NOT EXISTS (SELECT 1 FROM pg_attribute \
                 WHERE attrelid = to_regclass(split_part(n, '.', 1)) \
                   AND attname = split_part(n, '.', 2) AND NOT attisdropped) END \
         ORDER BY i",
        &[&names],
    )?;
    Ok(rows.iter().map(|row| row.get(0)).collect())
}

#[cfg(feature = "postgres")]
const PG_BY_KEY: &str = "SELECT id, body FROM by_operations \
     WHERE (body::jsonb) #>> '{idempotency,caller}' = $1 \
       AND (body::jsonb) #>> '{idempotency,key}' = $2";

/// The tenant's operations with a queue row (`$1`), a record without a
/// tenant being the default tenant's (`$2`).
#[cfg(feature = "postgres")]
const PG_UNFINISHED: &str = "SELECT o.id, o.body FROM by_operation_queue q \
     JOIN by_operations o ON o.id = q.id \
     WHERE COALESCE(NULLIF((o.body::jsonb) ->> 'tenant', ''), $2) = $1 ORDER BY q.seq";

/// Why a PostgreSQL admission wrote nothing.
#[cfg(feature = "postgres")]
enum Refused {
    /// The key is bound already.
    Replayed,
    /// A branch is held.
    Busy(String),
    /// A quota's refusal.
    Admission(Admission),
    Io(io::Error),
}

#[cfg(feature = "postgres")]
fn pg_ops(rows: &[postgres::Row]) -> io::Result<Vec<StoredOperation>> {
    rows.iter()
        .map(|row| {
            let (id, body): (String, String) = (row.get(0), row.get(1));
            parse_op(&id, &body, "")
        })
        .collect()
}

#[cfg(feature = "postgres")]
fn pg_op(rows: &[postgres::Row]) -> io::Result<Option<StoredOperation>> {
    rows.first()
        .map(|row| {
            let (id, body): (String, String) = (row.get(0), row.get(1));
            parse_op(&id, &body, "")
        })
        .transpose()
}

#[cfg(feature = "postgres")]
impl PostgresStore {
    /// Connect and create the tables if they are missing.
    pub fn open(url: &str) -> io::Result<PostgresStore> {
        let store = PostgresStore {
            url: url.to_owned(),
            conn: Mutex::new(None),
        };
        store.with(|client| {
            let names: Vec<&str> = PG_SCHEMA.iter().map(|(name, _)| *name).collect();
            // A schema already made, the usual case, is left alone: no
            // statement that would lock a table another server is using.
            let missing = pg_missing(client, &names)?;
            for (name, statement) in PG_SCHEMA {
                if !missing.iter().any(|m| m == name) {
                    continue;
                }
                // One opener at a time makes or migrates the schema; each
                // step locks at most one existing table, so it only ever
                // waits behind another server's transaction, never with it.
                let mut tx = client.transaction()?;
                tx.execute("SELECT pg_advisory_xact_lock(7390184326)", &[])?;
                if !pg_missing(&mut tx, &[name])?.is_empty() {
                    tx.batch_execute(statement)?;
                }
                tx.commit()?;
            }
            Ok(())
        })?;
        Ok(store)
    }

    /// Run `f` with a connection, on another thread, reconnecting after
    /// the connection closed.
    fn with<T: Send>(
        &self,
        f: impl FnOnce(&mut postgres::Client) -> Result<T, postgres::Error> + Send,
    ) -> io::Result<T> {
        let mut guard = self.conn.lock_recovering("conn");
        let conn: &mut Option<postgres::Client> = &mut guard;
        let url = &self.url;
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    if conn.as_ref().is_none_or(postgres::Client::is_closed) {
                        // Dropped here, on this thread, off the runtime.
                        *conn = Some(postgres::Client::connect(url, postgres::NoTls)?);
                    }
                    f(conn.as_mut().expect("connected above"))
                })
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
        .map_err(|e| match e.as_db_error() {
            Some(db) => io::Error::other(format!("operation registry: {}", db.message())),
            None => io::Error::other(format!("operation registry: {e}")),
        })
    }
}

#[cfg(feature = "postgres")]
impl Drop for PostgresStore {
    /// The client blocks to close its connection, which a Tokio runtime's
    /// thread may not.
    fn drop(&mut self) {
        let conn = self.conn.get_mut_recovering("conn");
        if let Some(client) = conn.take() {
            std::thread::scope(|scope| {
                scope.spawn(move || drop(client));
            });
        }
    }
}

#[cfg(feature = "postgres")]
fn pg_lease(lease: Duration) -> i64 {
    ms(lease)
}

#[cfg(feature = "postgres")]
impl branchyard::services::ServiceStore for PostgresStore {
    fn transact(
        &self,
        f: &mut (dyn FnMut(&mut dyn branchyard::services::Rows) -> io::Result<()> + Send),
    ) -> io::Result<()> {
        self.with(move |c| branchyard::services::pg::transact(c, f))?
    }
}

#[cfg(feature = "postgres")]
impl OperationStore for PostgresStore {
    fn services(&self) -> &dyn branchyard::services::ServiceStore {
        self
    }

    fn load(&self) -> io::Result<Vec<StoredOperation>> {
        let rows =
            self.with(|c| c.query("SELECT id, body FROM by_operations ORDER BY seq", &[]))?;
        rows.iter()
            .map(|row| {
                let (id, body): (String, String) = (row.get(0), row.get(1));
                parse_op(&id, &body, "")
            })
            .collect()
    }

    fn get(&self, id: &str) -> io::Result<Option<StoredOperation>> {
        let id = id.to_owned();
        let rows = self
            .with(move |c| c.query("SELECT id, body FROM by_operations WHERE id = $1", &[&id]))?;
        pg_op(&rows)
    }

    fn by_key(&self, caller: &str, key: &str) -> io::Result<Option<StoredOperation>> {
        let (caller, key) = (caller.to_owned(), key.to_owned());
        let rows = self.with(move |c| c.query(PG_BY_KEY, &[&caller, &key]))?;
        pg_op(&rows)
    }

    fn save(&self, operation: &StoredOperation) -> io::Result<()> {
        let body = serde_json::to_string(operation)?;
        let id = operation.operation.id.clone();
        self.with(move |c| {
            c.execute(
                "INSERT INTO by_operations (id, body) VALUES ($1, $2) \
                 ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body",
                &[&id, &body],
            )
        })?;
        Ok(())
    }

    fn orphans(&self) -> io::Result<Vec<StoredOperation>> {
        let rows = self.with(|c| {
            c.query(
                "SELECT o.id, o.body FROM by_operations o \
                 WHERE (o.body::jsonb) #>> '{operation,state}' IN ('queued', 'running') \
                   AND NOT EXISTS (SELECT 1 FROM by_operation_queue q WHERE q.id = o.id) \
                 ORDER BY o.seq",
                &[],
            )
        })?;
        rows.iter()
            .map(|row| {
                let (id, body): (String, String) = (row.get(0), row.get(1));
                parse_op(&id, &body, "")
            })
            .collect()
    }

    fn admit(
        &self,
        operation: &StoredOperation,
        work: &Value,
        quota: &AdmissionQuota,
    ) -> io::Result<Admission> {
        let body = serde_json::to_string(operation)?;
        let work = serde_json::to_string(work)?;
        let op = &operation.operation;
        let (id, repo) = (op.id.clone(), op.repo.clone());
        let idem = operation.idempotency.clone();
        let locks = lock_order(&operation.locks);
        let tenant = operation.tenant().to_owned();
        let requires = op.requires.clone();
        let (priority, enqueued_ms) = (op.priority, to_db("created_at_ms", op.created_at_ms)?);
        let admitted = self.with(move |c| {
            let mut tx = c.transaction()?;
            if !quota.is_empty() {
                // Admissions of one tenant with a quota take turns, on
                // every server sharing the schema: the count below then
                // cannot change before this one commits or rolls back.
                // Taken before any row, so it never waits behind one.
                tx.execute(
                    "SELECT pg_advisory_xact_lock(hashtextextended('branchyard tenant ' || $1, 0))",
                    &[&tenant],
                )?;
            }
            // The idempotency binding first: a second admission with this
            // key waits here for the first to commit, then replays it.
            let inserted = tx.execute(
                "INSERT INTO by_operations (id, body) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                &[&id, &body],
            )?;
            if inserted == 0 {
                tx.rollback()?;
                return Ok(Err(Refused::Replayed));
            }
            if !quota.is_empty() {
                let rows = tx.query(PG_UNFINISHED, &[&tenant, &DEFAULT_TENANT])?;
                let unfinished = match pg_ops(&rows) {
                    Ok(ops) => ops,
                    Err(e) => {
                        tx.rollback()?;
                        return Ok(Err(Refused::Io(e)));
                    }
                };
                match quota.check(operation, &unfinished) {
                    Ok(None) => {}
                    Ok(Some(refused)) => {
                        tx.rollback()?;
                        return Ok(Err(Refused::Admission(refused)));
                    }
                    Err(e) => {
                        tx.rollback()?;
                        return Ok(Err(Refused::Io(e)));
                    }
                }
            }
            for branch in &locks {
                tx.execute(
                    "DELETE FROM by_branch_locks WHERE repo = $1 AND branch = $2 \
                     AND expires_at IS NOT NULL AND expires_at <= clock_timestamp()",
                    &[&repo, branch],
                )?;
                let taken = tx.execute(
                    "INSERT INTO by_branch_locks (repo, branch, holder, token) \
                     VALUES ($1, $2, $3, $3) ON CONFLICT DO NOTHING",
                    &[&repo, branch, &id],
                )?;
                if taken == 0 {
                    tx.rollback()?;
                    return Ok(Err(Refused::Busy(branch.clone())));
                }
            }
            tx.execute(
                "INSERT INTO by_operation_queue \
                     (id, repo, work, requires, priority, tenant, enqueued_ms) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &id,
                    &repo,
                    &work,
                    &requires,
                    &priority,
                    &tenant,
                    &enqueued_ms,
                ],
            )?;
            tx.commit()?;
            Ok(Ok(()))
        })?;
        match admitted {
            Ok(()) => Ok(Admission::Admitted),
            Err(Refused::Admission(refused)) => Ok(refused),
            Err(Refused::Io(e)) => Err(e),
            Err(Refused::Replayed) => {
                let existing = match &idem {
                    Some(idem) => self.by_key(&idem.caller, &idem.key)?,
                    None => None,
                };
                existing
                    .map(|e| Admission::Replayed(Box::new(e)))
                    .ok_or_else(|| {
                        io::Error::other(format!("operation {} was already recorded", op.id))
                    })
            }
            Err(Refused::Busy(branch)) => {
                let (repo, name) = (op.repo.clone(), branch.clone());
                let rows = self.with(move |c| {
                    c.query(
                        "SELECT holder FROM by_branch_locks WHERE repo = $1 AND branch = $2",
                        &[&repo, &name],
                    )
                })?;
                let holder = rows
                    .first()
                    .map(|r| r.get::<_, String>(0))
                    .unwrap_or_else(|| "another operation".to_owned());
                Ok(Admission::Busy { branch, holder })
            }
        }
    }

    fn unfinished(&self, tenant: &str) -> io::Result<Vec<StoredOperation>> {
        let tenant = tenant.to_owned();
        let rows = self.with(move |c| c.query(PG_UNFINISHED, &[&tenant, &DEFAULT_TENANT]))?;
        pg_ops(&rows)
    }

    fn claim_next(
        &self,
        worker: &Worker,
        repos: &[String],
        labels: &[String],
        lease: Duration,
        scheduling: &Scheduling,
    ) -> io::Result<Option<Claim>> {
        let worker = worker.clone();
        let repos = repos.to_vec();
        let labels = labels.to_vec();
        let lease = pg_lease(lease);
        let now = scheduling.at_ms();
        let aging = scheduling.aging_ms();
        let window = ms(scheduling.window);
        let (tenants, weights) = scheduling.weight_arrays();
        // Claims whose process is gone from this host need not wait for
        // their lease. The PID is checked here, outside the connection's
        // closure, so a stored one that is no PID is an error naming it.
        let local: Vec<(String, i64, i64, String)> = self.with(|c| {
            Ok(c.query(
                "SELECT id, attempt, pid, start FROM by_operation_queue \
                 WHERE host = $1 AND worker IS NOT NULL AND worker <> $2 \
                   AND lease_until > clock_timestamp()",
                &[&worker.host, &worker.id],
            )?
            .iter()
            .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
            .collect())
        })?;
        let mut gone = Vec::new();
        for (id, attempt, pid, start) in local {
            if branchyard::process_gone(&worker.host, from_db_u32("pid", pid)?, &start) {
                gone.push((id, attempt));
            }
        }
        let claimed = self.with(move |c| {
            for (id, attempt) in &gone {
                c.execute(
                    "UPDATE by_operation_queue SET worker = NULL, lease_until = NULL \
                     WHERE id = $1 AND attempt = $2",
                    &[id, attempt],
                )?;
            }
            let pid = i64::from(worker.pid);
            let rows = c.query(
                PG_CLAIM,
                &[
                    &repos,
                    &worker.id,
                    &worker.host,
                    &pid,
                    &worker.start,
                    &(lease as f64),
                    &labels,
                    &aging,
                    &now,
                    &window,
                    &tenants,
                    &weights,
                ],
            )?;
            Ok(rows.first().map(|row| {
                let (id, fence, work, body, took_over): (String, i64, String, String, bool) =
                    (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4));
                (id, fence, work, body, took_over)
            }))
        })?;
        let Some((id, fence, work, body, took_over)) = claimed else {
            return Ok(None);
        };
        Ok(Some(Claim {
            operation: parse_op(&id, &body, "")?,
            work: serde_json::from_str(&work)?,
            fence,
            took_over,
        }))
    }

    fn queue(&self) -> io::Result<Vec<Queued>> {
        let rows = self.with(|c| {
            c.query(
                "SELECT id, repo, tenant, priority, requires, enqueued_ms, \
                     (worker IS NOT NULL AND lease_until > clock_timestamp()) \
                 FROM by_operation_queue ORDER BY seq",
                &[],
            )
        })?;
        Ok(rows
            .iter()
            .map(|row| Queued {
                id: row.get(0),
                repo: row.get(1),
                tenant: row.get(2),
                priority: row.get(3),
                requires: row.get(4),
                enqueued_ms: row.get(5),
                claimed: row.get(6),
            })
            .collect())
    }

    fn branch_priority(&self, repo: &str, branch: &str) -> io::Result<Option<i32>> {
        let (repo, branch) = (repo.to_owned(), branch.to_owned());
        let rows = self.with(move |c| {
            c.query(
                "SELECT COALESCE(((body::jsonb) #>> '{operation,priority}')::int, 0) \
                 FROM by_operations \
                 WHERE (body::jsonb) #>> '{operation,repo}' = $1 \
                   AND ((body::jsonb) #> '{operation,branches}') ? $2 \
                 ORDER BY seq DESC LIMIT 1",
                &[&repo, &branch],
            )
        })?;
        Ok(rows.first().map(|row| row.get(0)))
    }

    fn beat(
        &self,
        worker: &Worker,
        labels: &[String],
        repos: &[String],
        inventory: Option<&branchyard::inventory::Inventory>,
    ) -> io::Result<()> {
        let worker = worker.clone();
        let (labels, repos) = (labels.to_vec(), repos.to_vec());
        let inventory = inventory_text(inventory)?;
        self.with(move |c| {
            c.execute(
                "INSERT INTO by_workers (id, host, pid, labels, repos, seen, inventory) \
                 VALUES ($1, $2, $3, $4, $5, clock_timestamp(), $6) ON CONFLICT (id) DO UPDATE \
                 SET labels = EXCLUDED.labels, repos = EXCLUDED.repos, seen = EXCLUDED.seen, \
                 inventory = EXCLUDED.inventory",
                &[
                    &worker.id,
                    &worker.host,
                    &i64::from(worker.pid),
                    &labels,
                    &repos,
                    &inventory,
                ],
            )
        })?;
        Ok(())
    }

    fn workers(&self, within: Duration) -> io::Result<Vec<LiveWorker>> {
        let within = ms(within) as f64;
        let rows = self.with(move |c| {
            c.query(
                "SELECT id, host, labels, repos, \
                     (EXTRACT(EPOCH FROM clock_timestamp() - seen) * 1000)::float8, inventory \
                 FROM by_workers \
                 WHERE seen > clock_timestamp() - $1::float8 * interval '1 millisecond' \
                 ORDER BY id",
                &[&within],
            )
        })?;
        Ok(rows
            .iter()
            .map(|row| LiveWorker {
                id: row.get(0),
                host: row.get(1),
                labels: row.get(2),
                repos: row.get(3),
                seen_ms_ago: row.get::<_, f64>(4).max(0.0) as u64,
                inventory: inventory_of(row.get(5)),
            })
            .collect())
    }

    fn leave(&self, worker: &Worker) -> io::Result<()> {
        let id = worker.id.clone();
        self.with(move |c| c.execute("DELETE FROM by_workers WHERE id = $1", &[&id]))?;
        Ok(())
    }

    fn renew(&self, worker: &Worker, id: &str, fence: i64, lease: Duration) -> io::Result<bool> {
        let (id, worker) = (id.to_owned(), worker.id.clone());
        let lease = pg_lease(lease) as f64;
        let changed = self.with(move |c| {
            c.execute(
                "UPDATE by_operation_queue \
                 SET lease_until = clock_timestamp() + $4::float8 * interval '1 millisecond' \
                 WHERE id = $1 AND attempt = $2 AND worker = $3",
                &[&id, &fence, &worker, &lease],
            )
        })?;
        Ok(changed == 1)
    }

    fn start(
        &self,
        worker: &Worker,
        fence: i64,
        operation: &StoredOperation,
        lease: Duration,
    ) -> io::Result<bool> {
        let body = serde_json::to_string(operation)?;
        let (id, worker) = (operation.operation.id.clone(), worker.id.clone());
        let lease = pg_lease(lease) as f64;
        self.with(move |c| {
            let mut tx = c.transaction()?;
            let held = tx.execute(
                "UPDATE by_operation_queue \
                 SET lease_until = clock_timestamp() + $4::float8 * interval '1 millisecond' \
                 WHERE id = $1 AND attempt = $2 AND worker = $3",
                &[&id, &fence, &worker, &lease],
            )?;
            if held != 1 {
                tx.rollback()?;
                return Ok(false);
            }
            tx.execute(
                "UPDATE by_operations SET body = $2 WHERE id = $1",
                &[&id, &body],
            )?;
            tx.commit()?;
            Ok(true)
        })
    }

    fn finish(&self, worker: &Worker, fence: i64, operation: &StoredOperation) -> io::Result<bool> {
        let body = serde_json::to_string(operation)?;
        let (id, worker) = (operation.operation.id.clone(), worker.id.clone());
        self.with(move |c| {
            let mut tx = c.transaction()?;
            let held = tx.execute(
                "DELETE FROM by_operation_queue WHERE id = $1 AND attempt = $2 AND worker = $3",
                &[&id, &fence, &worker],
            )?;
            if held != 1 {
                tx.rollback()?;
                return Ok(false);
            }
            tx.execute(
                "UPDATE by_operations SET body = $2 WHERE id = $1",
                &[&id, &body],
            )?;
            tx.execute("DELETE FROM by_branch_locks WHERE token = $1", &[&id])?;
            tx.commit()?;
            Ok(true)
        })
    }

    fn release(&self, worker: &Worker, id: &str, fence: i64) -> io::Result<()> {
        let (id, worker) = (id.to_owned(), worker.id.clone());
        self.with(move |c| {
            c.execute(
                "UPDATE by_operation_queue SET worker = NULL, lease_until = NULL \
                 WHERE id = $1 AND attempt = $2 AND worker = $3",
                &[&id, &fence, &worker],
            )
        })?;
        Ok(())
    }

    fn pending(&self, repos: &[String]) -> io::Result<usize> {
        let repos = repos.to_vec();
        let count: i64 = self.with(move |c| {
            c.query_one(
                "SELECT COUNT(*) FROM by_operation_queue WHERE repo = ANY($1)",
                &[&repos],
            )
            .map(|row| row.get(0))
        })?;
        Ok(from_db_usize("pending", count)?)
    }

    fn hold(
        &self,
        repo: &str,
        branch: &str,
        holder: &str,
        token: &str,
        ttl: Duration,
    ) -> io::Result<Option<String>> {
        let (repo, branch) = (repo.to_owned(), branch.to_owned());
        let (holder, token) = (holder.to_owned(), token.to_owned());
        let ttl = pg_lease(ttl) as f64;
        self.with(move |c| {
            let mut tx = c.transaction()?;
            tx.execute(
                "DELETE FROM by_branch_locks WHERE repo = $1 AND branch = $2 \
                 AND expires_at IS NOT NULL AND expires_at <= clock_timestamp()",
                &[&repo, &branch],
            )?;
            let taken = tx.execute(
                "INSERT INTO by_branch_locks (repo, branch, holder, token, expires_at) \
                 VALUES ($1, $2, $3, $4, clock_timestamp() + $5::float8 * interval '1 millisecond') \
                 ON CONFLICT DO NOTHING",
                &[&repo, &branch, &holder, &token, &ttl],
            )?;
            if taken == 1 {
                tx.commit()?;
                return Ok(None);
            }
            let rows = tx.query(
                "SELECT holder FROM by_branch_locks WHERE repo = $1 AND branch = $2",
                &[&repo, &branch],
            )?;
            tx.rollback()?;
            Ok(Some(
                rows.first()
                    .map(|r| r.get::<_, String>(0))
                    .unwrap_or_else(|| "another operation".to_owned()),
            ))
        })
    }

    fn unhold(&self, repo: &str, branch: &str, token: &str) -> io::Result<()> {
        let (repo, branch, token) = (repo.to_owned(), branch.to_owned(), token.to_owned());
        self.with(move |c| {
            c.execute(
                "DELETE FROM by_branch_locks WHERE repo = $1 AND branch = $2 AND token = $3",
                &[&repo, &branch, &token],
            )
        })?;
        Ok(())
    }

    fn reset(&self) -> io::Result<()> {
        Err(io::Error::other(
            "a PostgreSQL registry may be shared by several servers; its claims expire instead",
        ))
    }

    fn load_webhook_cursor(&self, id: &str) -> io::Result<Option<u64>> {
        let id = id.to_owned();
        let rows = self.with(move |c| {
            c.query(
                "SELECT cursor FROM by_webhook_cursors WHERE id = $1",
                &[&id],
            )
        })?;
        let cursor = rows.first().map(|row| row.get::<_, i64>(0));
        Ok(from_db_opt("cursor", cursor)?)
    }

    fn save_webhook_cursor(&self, id: &str, cursor: u64) -> io::Result<()> {
        let id = id.to_owned();
        let cursor = to_db("cursor", cursor)?;
        self.with(move |c| {
            c.execute(
                "INSERT INTO by_webhook_cursors (id, cursor) VALUES ($1, $2) \
                 ON CONFLICT (id) DO UPDATE SET cursor = EXCLUDED.cursor",
                &[&id, &cursor],
            )
        })?;
        Ok(())
    }

    fn claim_webhook_cursor(&self, id: &str, expected: Option<u64>, next: u64) -> io::Result<bool> {
        let (id, next) = (id.to_owned(), to_db("next", next)?);
        let expected = to_db_opt("expected", expected)?;
        let changed = self.with(move |c| match expected {
            None => c.execute(
                "INSERT INTO by_webhook_cursors (id, cursor) VALUES ($1, $2) \
                 ON CONFLICT (id) DO NOTHING",
                &[&id, &next],
            ),
            Some(from) => c.execute(
                "UPDATE by_webhook_cursors SET cursor = $3 WHERE id = $1 AND cursor = $2",
                &[&id, &from, &next],
            ),
        })?;
        Ok(changed == 1)
    }
}

/// The worker-label conformance every [`OperationStore`] passes: a worker
/// claims only operations whose required labels it all carries; one
/// requiring nothing is anyone's; two workers racing for labeled work
/// claim each operation once, and a worker without the label none of it;
/// beats are seen by [`OperationStore::workers`], and a leaving worker is
/// not. `repo` names a repository no other test of the store uses. Panics
/// on a violation. Run on SQLite here and on PostgreSQL by
/// `tests/postgres.rs`.
#[doc(hidden)]
pub fn check_labels(store: &dyn OperationStore, repo: &str) {
    use branchyard_client::api::{OperationKind, OperationState};
    const LEASE: Duration = Duration::from_secs(30);
    let stored = |id: &str, requires: &[&str]| StoredOperation {
        operation: Operation {
            id: id.into(),
            repo: repo.into(),
            kind: OperationKind::Task,
            state: OperationState::Queued,
            branches: Vec::new(),
            cursor: 0,
            end_cursor: None,
            created_at_ms: 1,
            finished_at_ms: None,
            result: None,
            error: None,
            requires: requires.iter().map(|l| l.to_string()).collect(),
            waiting: None,
            priority: 0,
        },
        idempotency: None,
        locks: Vec::new(),
        tenant: String::new(),
        principal: None,
        creates: Vec::new(),
        trace: None,
    };
    let admit = |id: &str, requires: &[&str]| {
        let admitted = store
            .admit(
                &stored(&format!("{repo}-{id}"), requires),
                &serde_json::json!({}),
                &AdmissionQuota::default(),
            )
            .unwrap();
        assert_eq!(admitted, Admission::Admitted);
    };
    let worker = |id: &str| Worker {
        id: format!("{repo}-{id}"),
        ..Worker::current()
    };
    let labels = |list: &[&str]| list.iter().map(|l| l.to_string()).collect::<Vec<_>>();
    let repos = vec![repo.to_owned()];
    let claimed = |claim: Option<Claim>| claim.map(|c| c.operation.operation.id);
    admit("gpu", &["gpu"]);
    admit("any", &[]);
    admit("both", &["gpu", "linux"]);
    // Oldest first among what each may claim.
    let plain = worker("plain");
    assert_eq!(
        claimed(store.claim(&plain, &repos, &[], LEASE).unwrap()),
        Some(format!("{repo}-any"))
    );
    assert_eq!(
        claimed(
            store
                .claim(&plain, &repos, &labels(&["linux"]), LEASE)
                .unwrap()
        ),
        None
    );
    let gpu = worker("gpu");
    assert_eq!(
        claimed(store.claim(&gpu, &repos, &labels(&["gpu"]), LEASE).unwrap()),
        Some(format!("{repo}-gpu"))
    );
    assert_eq!(
        claimed(store.claim(&gpu, &repos, &labels(&["gpu"]), LEASE).unwrap()),
        None
    );
    let both = worker("both");
    assert_eq!(
        claimed(
            store
                .claim(&both, &repos, &labels(&["linux", "x", "gpu"]), LEASE)
                .unwrap()
        ),
        Some(format!("{repo}-both"))
    );

    // Two labeled workers race for eight labeled operations while an
    // unlabeled one tries too.
    for n in 0..8 {
        admit(&format!("race-{n}"), &["gpu"]);
    }
    let racers = [worker("racer-a"), worker("racer-b")];
    let outsider = worker("outsider");
    let (mut won, outside) = std::thread::scope(|scope| {
        let handles: Vec<_> = racers
            .iter()
            .map(|w| {
                let repos = repos.clone();
                scope.spawn(move || {
                    let mut got = Vec::new();
                    while let Some(claim) =
                        store.claim(w, &repos, &labels(&["gpu"]), LEASE).unwrap()
                    {
                        got.push(claim.operation.operation.id);
                    }
                    got
                })
            })
            .collect();
        let outside = scope.spawn(|| {
            let mut got = Vec::new();
            for _ in 0..20 {
                if let Some(claim) = store.claim(&outsider, &repos, &[], LEASE).unwrap() {
                    got.push(claim.operation.operation.id);
                }
            }
            got
        });
        let won: Vec<String> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        (won, outside.join().unwrap())
    });
    assert!(outside.is_empty(), "{outside:?}");
    won.sort();
    let mut expected: Vec<String> = (0..8).map(|n| format!("{repo}-race-{n}")).collect();
    expected.sort();
    assert_eq!(won, expected, "each operation claimed exactly once");

    // Beats.
    store.beat(&gpu, &labels(&["gpu"]), &repos, None).unwrap();
    store.beat(&plain, &[], &repos, None).unwrap();
    let live = store.workers(Duration::from_secs(60)).unwrap();
    let mine: Vec<&LiveWorker> = live.iter().filter(|w| w.id.starts_with(repo)).collect();
    assert_eq!(mine.len(), 2, "{live:?}");
    let seen = mine.iter().find(|w| w.id == gpu.id).unwrap();
    assert_eq!(seen.labels, ["gpu"]);
    assert_eq!(seen.repos, repos);
    assert!(seen.seen_ms_ago < 60_000);
    let reason = unclaimable(&labels(&["gpu", "linux"]), repo, &live).unwrap();
    assert!(
        reason.contains("gpu, linux") && reason.contains(&gpu.id),
        "{reason}"
    );
    assert_eq!(unclaimable(&labels(&["gpu"]), repo, &live), None);
    store.leave(&gpu).unwrap();
    let live = store.workers(Duration::from_secs(60)).unwrap();
    assert!(!live.iter().any(|w| w.id == gpu.id));
    assert!(unclaimable(&labels(&["gpu"]), repo, &live).is_some());

    // A beat carries the worker's harness inventory, and the next beat
    // replaces it (docs/harness-lifecycle.md).
    let mut inventory = test_inventory("codex");
    let derived = inventory.labels();
    assert_eq!(derived, ["harness:codex"]);
    store
        .beat(&plain, &derived, &repos, Some(&inventory))
        .unwrap();
    let live = store.workers(Duration::from_secs(60)).unwrap();
    let seen = live.iter().find(|w| w.id == plain.id).unwrap();
    assert_eq!(seen.inventory.as_ref(), Some(&inventory));
    assert_eq!(seen.labels, ["harness:codex"]);
    assert_eq!(
        harness_requirement(&labels(&["codex"]), repo, &live),
        ["harness:codex"]
    );
    // Steered only toward a worker that can run every harness named; never
    // held back when none advertises them.
    assert!(harness_requirement(&labels(&["codex", "goose"]), repo, &live).is_empty());
    assert!(harness_requirement(&labels(&["codex"]), "elsewhere", &live).is_empty());
    let gate = WorkersGate::new(repo, live.clone());
    use branchyard::inventory::HarnessGate;
    assert_eq!(gate.check("codex"), Ok(()));
    let why = gate.check("goose").unwrap_err();
    assert!(
        why.contains("not installed") && why.contains(&plain.id),
        "{why}"
    );
    assert_eq!(WorkersGate::new("elsewhere", live).check("goose"), Ok(()));
    inventory.harnesses[0].login.state = branchyard::inventory::LoginState::LoggedOut;
    store
        .beat(&plain, &inventory.labels(), &repos, Some(&inventory))
        .unwrap();
    let live = store.workers(Duration::from_secs(60)).unwrap();
    let seen = live.iter().find(|w| w.id == plain.id).unwrap();
    assert!(seen.labels.is_empty());
    assert!(harness_requirement(&labels(&["codex"]), repo, &live).is_empty());
    store.beat(&plain, &[], &repos, None).unwrap();
    let live = store.workers(Duration::from_secs(60)).unwrap();
    assert_eq!(
        live.iter().find(|w| w.id == plain.id).unwrap().inventory,
        None
    );
    store.leave(&plain).unwrap();
}

/// An inventory in which `id` is installed, on PATH and logged in.
pub fn test_inventory(id: &str) -> branchyard::inventory::Inventory {
    use branchyard::inventory::{Evidence, HarnessState, Inventory, Login, LoginState};
    Inventory {
        host: "box".into(),
        os: "Linux".into(),
        detected_at_ms: 1,
        checked: vec![id.to_owned(), "goose".into()],
        harnesses: vec![HarnessState {
            id: id.to_owned(),
            path: format!("/usr/bin/{id}"),
            on_path: true,
            version: Some("1.2.3".into()),
            version_note: None,
            login: Login {
                state: LoginState::LoggedIn,
                evidence: Evidence::Verified,
                detail: "logged in".into(),
            },
            quota: None,
        }],
        tools: Default::default(),
    }
}

/// Draws for the scheduling property check, on the one seedable generator
/// (`branchyard_support::rng`), so a failure names a seed that reproduces it.
trait Draw {
    /// An index into a collection of `len` items.
    fn index(&mut self, len: usize) -> usize;
    /// A scheduling priority from -10 to 10.
    fn priority(&mut self) -> i32;
}

impl Draw for branchyard_support::rng::SplitMix64 {
    fn index(&mut self, len: usize) -> usize {
        let len = u64::try_from(len).expect("a length fits u64");
        usize::try_from(self.below(len)).expect("below a usize fits it")
    }

    fn priority(&mut self) -> i32 {
        i32::try_from(self.below(21)).expect("below 21 fits i32") - 10
    }
}

/// What the scheduling conformance admits: an operation of `repo` for
/// `tenant`, at `priority`, admitted `age_ms` before the fixed clock.
fn scheduled(
    repo: &str,
    id: &str,
    tenant: &str,
    priority: i32,
    created_at_ms: u64,
    requires: &[&str],
) -> StoredOperation {
    use branchyard_client::api::{OperationKind, OperationState};
    StoredOperation {
        operation: Operation {
            id: id.into(),
            repo: repo.into(),
            kind: OperationKind::Task,
            state: OperationState::Queued,
            branches: Vec::new(),
            cursor: 0,
            end_cursor: None,
            created_at_ms,
            finished_at_ms: None,
            result: None,
            error: None,
            requires: requires.iter().map(|l| l.to_string()).collect(),
            waiting: None,
            priority,
        },
        idempotency: None,
        locks: Vec::new(),
        tenant: tenant.into(),
        principal: None,
        creates: Vec::new(),
        trace: None,
    }
}

/// The scheduling model the stores implement, in plain Rust: which of
/// `queued` (admission order) a worker carrying `gpu` claims next, given
/// each tenant's recorded claims (`used`, at a fixed clock, so undecayed)
/// and what runs. The conformance checks the stores against it claim by
/// claim.
struct Model {
    /// (id, tenant, priority, created_at_ms, requires gpu, state:
    /// 0 queued, 1 running, 2 finished)
    ops: Vec<(String, String, i32, u64, bool, u8)>,
    used: BTreeMap<String, f64>,
}

impl Model {
    fn pick(&self, scheduling: &Scheduling, gpu: bool) -> Option<usize> {
        let now = scheduling.at_ms();
        let share = |tenant: &str| {
            let running = self
                .ops
                .iter()
                .filter(|o| o.1 == tenant && o.5 == 1)
                .count() as f64;
            (self.used.get(tenant).copied().unwrap_or(0.0) + running) / scheduling.weight(tenant)
        };
        let mut best: Option<(usize, i64, f64)> = None;
        for (i, op) in self.ops.iter().enumerate() {
            if op.5 != 0 || (op.4 && !gpu) {
                continue;
            }
            let effective = scheduling.effective(op.2, op.3 as i64, now);
            let share = share(&op.1);
            let better = match best {
                None => true,
                Some((_, e, s)) => effective > e || (effective == e && share < s),
            };
            if better {
                best = Some((i, effective, share));
            }
        }
        best.map(|b| b.0)
    }
}

/// The scheduling conformance every [`OperationStore`] passes (see
/// [`Scheduling`]): priority order; weighted fair share between two tenants;
/// aging that lifts long-waiting low-priority work over fresh high-priority
/// work, and none without it; two workers racing never claiming one
/// operation twice; and random submissions, finishes and labeled and
/// unlabeled claims matching [`Model`] claim by claim. Measured against a
/// fixed clock, never the time it takes. `repo` names a repository (and
/// prefixes tenants) no other test of the store uses. Panics on a
/// violation. Run on SQLite here and on PostgreSQL by `tests/postgres.rs`.
#[doc(hidden)]
pub fn check_scheduling(store: &dyn OperationStore, repo: &str) {
    use branchyard_client::api::OperationState;
    const NOW: u64 = 2_000_000_000_000;
    const LEASE: Duration = Duration::from_secs(300);
    let repos = vec![repo.to_owned()];
    let worker = |id: &str| Worker {
        id: format!("{repo}-{id}"),
        ..Worker::current()
    };
    let admit = |op: StoredOperation| {
        let admitted = store
            .admit(&op, &serde_json::json!({}), &AdmissionQuota::default())
            .unwrap();
        assert_eq!(admitted, Admission::Admitted);
    };
    let fixed = |weights: &[(&str, f64)], aging: Option<Duration>| Scheduling {
        weights: weights
            .iter()
            .map(|(t, w)| (format!("{repo}-{t}"), *w))
            .collect(),
        aging,
        window: Duration::from_secs(3600),
        clock_ms: Some(NOW),
    };
    let claim = |w: &Worker, labels: &[String], scheduling: &Scheduling| {
        store
            .claim_next(w, &repos, labels, LEASE, scheduling)
            .unwrap()
    };
    let finish = |w: &Worker, claim: &Claim| {
        let mut done = claim.operation.clone();
        done.operation.state = OperationState::Succeeded;
        assert!(store.finish(w, claim.fence, &done).unwrap());
    };
    let short = |id: &str| id.trim_start_matches(&format!("{repo}-")).to_owned();

    // Priority order, highest first, then the oldest; aging off.
    let tenant = format!("{repo}-solo");
    for (n, priority) in [0, 5, -3, 5, 10, -10].into_iter().enumerate() {
        admit(scheduled(
            repo,
            &format!("{repo}-p{n}"),
            &tenant,
            priority,
            NOW,
            &[],
        ));
    }
    let one = worker("one");
    let plain = fixed(&[], None);
    let mut order = Vec::new();
    while let Some(c) = claim(&one, &[], &plain) {
        assert_eq!(c.operation.operation.priority, {
            let n: usize = short(&c.operation.operation.id)[1..].parse().unwrap();
            [0, 5, -3, 5, 10, -10][n]
        });
        order.push(short(&c.operation.operation.id));
        finish(&one, &c);
    }
    assert_eq!(order, ["p4", "p1", "p3", "p0", "p2", "p5"]);

    // Aging: a -10 that has waited 30 steps outranks a fresh 10; without
    // aging it does not.
    for aged in [true, false] {
        let tag = if aged { "aged" } else { "flat" };
        admit(scheduled(
            repo,
            &format!("{repo}-{tag}-low"),
            &tenant,
            -10,
            NOW - 30_000,
            &[],
        ));
        admit(scheduled(
            repo,
            &format!("{repo}-{tag}-high"),
            &tenant,
            10,
            NOW,
            &[],
        ));
        let scheduling = fixed(&[], aged.then_some(Duration::from_secs(1)));
        let first = claim(&one, &[], &scheduling).unwrap();
        let expected = if aged { "low" } else { "high" };
        assert_eq!(
            short(&first.operation.operation.id),
            format!("{tag}-{expected}")
        );
        finish(&one, &first);
        let second = claim(&one, &[], &scheduling).unwrap();
        finish(&one, &second);
    }

    // Weighted fair share: tenant a (weight 2) and b (weight 1), thirty
    // operations each, admitted alternately, claimed and finished one at a
    // time: a gets two claims for each of b's.
    let (a, b) = (format!("{repo}-a"), format!("{repo}-b"));
    for n in 0..30 {
        admit(scheduled(repo, &format!("{repo}-a{n}"), &a, 0, NOW, &[]));
        admit(scheduled(repo, &format!("{repo}-b{n}"), &b, 0, NOW, &[]));
    }
    let weighted = fixed(&[("a", 2.0), ("b", 1.0)], None);
    let mut first_thirty = (0, 0);
    let mut claimed = Vec::new();
    while let Some(c) = claim(&one, &[], &weighted) {
        let id = short(&c.operation.operation.id);
        if claimed.len() < 30 {
            match id.starts_with('a') {
                true => first_thirty.0 += 1,
                false => first_thirty.1 += 1,
            }
        }
        claimed.push(id);
        finish(&one, &c);
    }
    assert_eq!(claimed.len(), 60);
    assert!(
        (19..=21).contains(&first_thirty.0),
        "tenant a should get about two thirds of the first thirty claims: {first_thirty:?} \
         in {claimed:?}"
    );
    // Within a tenant, oldest first.
    let a_order: Vec<&String> = claimed.iter().filter(|i| i.starts_with('a')).collect();
    let mut sorted = a_order.clone();
    sorted.sort_by_key(|i| i[1..].parse::<u32>().unwrap());
    assert_eq!(a_order, sorted);

    // Two workers racing for mixed priorities and tenants claim each
    // operation exactly once.
    let mut rng = branchyard_support::rng::SplitMix64::new(0x5eed_0001);
    let mut expected: Vec<String> = Vec::new();
    for n in 0..24 {
        let tenant = format!("{repo}-race{}", rng.below(3));
        let id = format!("{repo}-race-{n}");
        admit(scheduled(
            repo,
            &id,
            &tenant,
            rng.priority(),
            NOW - rng.below(10_000),
            &[],
        ));
        expected.push(id);
    }
    let racers = [worker("racer-a"), worker("racer-b")];
    let racing = fixed(&[("race0", 3.0)], Some(Duration::from_secs(1)));
    let mut won: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = racers
            .iter()
            .map(|w| {
                let racing = racing.clone();
                scope.spawn(move || {
                    let mut got = Vec::new();
                    while let Some(c) = claim(w, &[], &racing) {
                        got.push(c.operation.operation.id.clone());
                        finish(w, &c);
                    }
                    got
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    won.sort();
    expected.sort();
    assert_eq!(won, expected, "each operation claimed exactly once");

    // Random submissions, finishes and claims by a worker with and one
    // without the `gpu` label, checked against the model claim by claim.
    for seed in 1..=4u64 {
        let mut rng = branchyard_support::rng::SplitMix64::new(seed);
        let tag = format!("prop{seed}");
        let tenants: Vec<String> = (0..3).map(|t| format!("{repo}-{tag}-t{t}")).collect();
        let weights = [("t0", 1.0), ("t1", 2.0), ("t2", 0.5)];
        let scheduling = Scheduling {
            weights: weights
                .iter()
                .map(|(t, w)| (format!("{repo}-{tag}-{t}"), *w))
                .collect(),
            aging: Some(Duration::from_secs(5)),
            window: Duration::from_secs(3600),
            clock_ms: Some(NOW),
        };
        let mut model = Model {
            ops: Vec::new(),
            used: BTreeMap::new(),
        };
        let gpu_worker = worker(&format!("{tag}-gpu"));
        let plain_worker = worker(&format!("{tag}-plain"));
        let gpu_labels = vec!["gpu".to_owned()];
        let mut claims: Vec<(usize, Claim, bool)> = Vec::new();
        for step in 0..120 {
            match rng.below(4) {
                // Submit.
                0 | 1 if model.ops.len() < 40 => {
                    let n = model.ops.len();
                    let id = format!("{repo}-{tag}-{n}");
                    let tenant = tenants[rng.index(tenants.len())].clone();
                    let priority = rng.priority();
                    let created = NOW - rng.below(60_000);
                    let gpu = rng.below(4) == 0;
                    admit(scheduled(
                        repo,
                        &id,
                        &tenant,
                        priority,
                        created,
                        if gpu { &["gpu"] } else { &[] },
                    ));
                    model.ops.push((id, tenant, priority, created, gpu, 0));
                }
                // Finish something running.
                2 if !claims.is_empty() => {
                    let k = rng.index(claims.len());
                    let (i, c, gpu) = claims.remove(k);
                    let w = if gpu { &gpu_worker } else { &plain_worker };
                    finish(w, &c);
                    model.ops[i].5 = 2;
                }
                // Claim, with or without the label.
                _ => {
                    let gpu = rng.below(2) == 0;
                    let (w, labels) = match gpu {
                        true => (&gpu_worker, gpu_labels.as_slice()),
                        false => (&plain_worker, &[][..]),
                    };
                    let want = model.pick(&scheduling, gpu);
                    let got = claim(w, labels, &scheduling);
                    let got_id = got.as_ref().map(|c| c.operation.operation.id.clone());
                    assert_eq!(
                        got_id,
                        want.map(|i| model.ops[i].0.clone()),
                        "seed {seed}, step {step}, gpu {gpu}: the store and the model differ"
                    );
                    if let (Some(i), Some(c)) = (want, got) {
                        model.ops[i].5 = 1;
                        *model.used.entry(model.ops[i].1.clone()).or_default() += 1.0;
                        claims.push((i, c, gpu));
                    }
                }
            }
        }
        // Drain: everything left is claimed exactly once, in model order.
        for (i, c, gpu) in claims.drain(..) {
            finish(if gpu { &gpu_worker } else { &plain_worker }, &c);
            model.ops[i].5 = 2;
        }
        while let Some(i) = model.pick(&scheduling, true) {
            let c = claim(&gpu_worker, &gpu_labels, &scheduling).unwrap();
            assert_eq!(
                c.operation.operation.id, model.ops[i].0,
                "seed {seed}, drain"
            );
            *model.used.entry(model.ops[i].1.clone()).or_default() += 1.0;
            model.ops[i].5 = 2;
            finish(&gpu_worker, &c);
        }
        assert!(claim(&gpu_worker, &gpu_labels, &scheduling).is_none());
    }

    // The queue as metrics read it: nothing of this repository is left.
    assert!(!store.queue().unwrap().iter().any(|q| q.repo == repo));
}

/// Operations in memory only, for tests and embedding: a [`SqliteStore`]
/// on an in-memory database.
pub struct MemoryStore(SqliteStore);

impl Default for MemoryStore {
    fn default() -> MemoryStore {
        MemoryStore(SqliteStore::memory())
    }
}

impl std::ops::Deref for MemoryStore {
    type Target = SqliteStore;
    fn deref(&self) -> &SqliteStore {
        &self.0
    }
}

macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        $(fn $name(&self, $($arg: $ty),*) -> $ret { self.0.$name($($arg),*) })*
    };
}

impl OperationStore for MemoryStore {
    fn services(&self) -> &dyn branchyard::services::ServiceStore {
        &self.0
    }
    forward! {
        load() -> io::Result<Vec<StoredOperation>>;
        get(id: &str) -> io::Result<Option<StoredOperation>>;
        by_key(caller: &str, key: &str) -> io::Result<Option<StoredOperation>>;
        save(operation: &StoredOperation) -> io::Result<()>;
        orphans() -> io::Result<Vec<StoredOperation>>;
        admit(operation: &StoredOperation, work: &Value, quota: &AdmissionQuota)
            -> io::Result<Admission>;
        unfinished(tenant: &str) -> io::Result<Vec<StoredOperation>>;
        claim_next(worker: &Worker, repos: &[String], labels: &[String], lease: Duration,
            scheduling: &Scheduling) -> io::Result<Option<Claim>>;
        queue() -> io::Result<Vec<Queued>>;
        branch_priority(repo: &str, branch: &str) -> io::Result<Option<i32>>;
        beat(worker: &Worker, labels: &[String], repos: &[String],
            inventory: Option<&branchyard::inventory::Inventory>) -> io::Result<()>;
        workers(within: Duration) -> io::Result<Vec<LiveWorker>>;
        leave(worker: &Worker) -> io::Result<()>;
        renew(worker: &Worker, id: &str, fence: i64, lease: Duration) -> io::Result<bool>;
        start(worker: &Worker, fence: i64, operation: &StoredOperation, lease: Duration)
            -> io::Result<bool>;
        finish(worker: &Worker, fence: i64, operation: &StoredOperation) -> io::Result<bool>;
        release(worker: &Worker, id: &str, fence: i64) -> io::Result<()>;
        pending(repos: &[String]) -> io::Result<usize>;
        hold(repo: &str, branch: &str, holder: &str, token: &str, ttl: Duration)
            -> io::Result<Option<String>>;
        unhold(repo: &str, branch: &str, token: &str) -> io::Result<()>;
        reset() -> io::Result<()>;
        load_webhook_cursor(id: &str) -> io::Result<Option<u64>>;
        save_webhook_cursor(id: &str, cursor: u64) -> io::Result<()>;
        claim_webhook_cursor(id: &str, expected: Option<u64>, next: u64) -> io::Result<bool>;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_client::api::{OperationKind, OperationState};

    fn op(id: &str, state: OperationState) -> StoredOperation {
        StoredOperation {
            operation: Operation {
                id: id.into(),
                repo: "r".into(),
                kind: OperationKind::Task,
                state,
                branches: vec!["b".into()],
                cursor: 0,
                end_cursor: None,
                created_at_ms: 1,
                finished_at_ms: None,
                result: None,
                error: None,
                requires: Vec::new(),
                waiting: None,
                priority: 0,
            },
            idempotency: None,
            locks: Vec::new(),
            tenant: String::new(),
            principal: None,
            creates: Vec::new(),
            trace: None,
        }
    }

    /// A fresh directory, removed when the returned guard is dropped.
    fn temp(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("branchyard-{name}-"))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn the_latest_save_wins_and_survives_reopening() {
        let temp = temp("store");
        let dir = temp.path();
        let path = dir.join("operations.jsonl");
        let store = FileStore::open(&path).unwrap();
        store.save(&op("a", OperationState::Queued)).unwrap();
        store.save(&op("b", OperationState::Queued)).unwrap();
        store.save(&op("a", OperationState::Succeeded)).unwrap();
        drop(store);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{{\"operation\":").unwrap();
        drop(file);
        let store = FileStore::open(&path).unwrap();
        let ops = store.load().unwrap();
        let states: Vec<_> = ops
            .iter()
            .map(|o| (o.operation.id.as_str(), o.operation.state))
            .collect();
        assert_eq!(
            states,
            [
                ("a", OperationState::Succeeded),
                ("b", OperationState::Queued)
            ]
        );
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
    }

    #[test]
    fn webhook_cursors_round_trip_and_only_ever_move_as_told() {
        let temp = temp("webhook-cursor");
        let dir = temp.path();
        let store = SqliteStore::open(dir.join("state.db"), None).unwrap();
        assert_eq!(store.load_webhook_cursor("repo:hook").unwrap(), None);
        store.save_webhook_cursor("repo:hook", 5).unwrap();
        assert_eq!(store.load_webhook_cursor("repo:hook").unwrap(), Some(5));
        store.save_webhook_cursor("repo:hook", 12).unwrap();
        assert_eq!(store.load_webhook_cursor("repo:hook").unwrap(), Some(12));
        // A distinct id has its own cursor.
        assert_eq!(store.load_webhook_cursor("repo:other").unwrap(), None);
        drop(store);
        // Durable: reopening finds it again.
        let store = SqliteStore::open(dir.join("state.db"), None).unwrap();
        assert_eq!(store.load_webhook_cursor("repo:hook").unwrap(), Some(12));

        let memory = MemoryStore::default();
        assert_eq!(memory.load_webhook_cursor("h").unwrap(), None);
        memory.save_webhook_cursor("h", 3).unwrap();
        assert_eq!(memory.load_webhook_cursor("h").unwrap(), Some(3));

        // A claim moves it only from where the claimer saw it.
        assert!(store.claim_webhook_cursor("repo:new", None, 4).unwrap());
        assert!(!store.claim_webhook_cursor("repo:new", None, 9).unwrap());
        assert!(!store.claim_webhook_cursor("repo:new", Some(3), 9).unwrap());
        assert!(store.claim_webhook_cursor("repo:new", Some(4), 9).unwrap());
        assert_eq!(store.load_webhook_cursor("repo:new").unwrap(), Some(9));
    }

    #[test]
    fn sqlite_imports_the_file_once_and_keeps_order_and_the_latest_save() {
        let temp = temp("sqlite-store");
        let dir = temp.path();
        let legacy = dir.join("operations.jsonl");
        let file = FileStore::open(&legacy).unwrap();
        file.save(&op("a", OperationState::Queued)).unwrap();
        file.save(&op("b", OperationState::Succeeded)).unwrap();
        drop(file);
        let db = dir.join("state.db");
        let store = SqliteStore::open(&db, Some(&legacy)).unwrap();
        assert!(!legacy.exists());
        assert!(dir.join("operations.jsonl.imported").is_file());
        // The imported queued operation has no queue row.
        let orphans = store.orphans().unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].operation.id, "a");
        store.save(&op("a", OperationState::Interrupted)).unwrap();
        store.save(&op("c", OperationState::Succeeded)).unwrap();
        drop(store);
        let store = SqliteStore::open(&db, Some(&legacy)).unwrap();
        let states: Vec<_> = store
            .load()
            .unwrap()
            .iter()
            .map(|o| (o.operation.id.clone(), o.operation.state))
            .collect();
        assert_eq!(
            states,
            [
                ("a".to_owned(), OperationState::Interrupted),
                ("b".to_owned(), OperationState::Succeeded),
                ("c".to_owned(), OperationState::Succeeded),
            ]
        );
        assert!(store.orphans().unwrap().is_empty());
    }

    fn keyed(id: &str, key: &str, locks: &[&str]) -> StoredOperation {
        let mut stored = op(id, OperationState::Queued);
        stored.idempotency = Some(Idempotency {
            caller: "c".into(),
            key: key.into(),
            fingerprint: "f".into(),
        });
        stored.locks = locks.iter().map(|s| s.to_string()).collect();
        stored
    }

    fn worker(id: &str) -> Worker {
        Worker {
            id: id.into(),
            ..Worker::current()
        }
    }

    const LEASE: Duration = Duration::from_secs(30);

    #[test]
    fn admission_binds_the_key_takes_locks_and_enqueues_together() {
        let store = MemoryStore::default();
        let work = serde_json::json!({ "kind": "test" });
        assert_eq!(
            store
                .admit(
                    &keyed("a", "k", &["x", "y"]),
                    &work,
                    &AdmissionQuota::default()
                )
                .unwrap(),
            Admission::Admitted
        );
        // The same key replays, whatever else differs.
        match store
            .admit(&keyed("b", "k", &[]), &work, &AdmissionQuota::default())
            .unwrap()
        {
            Admission::Replayed(existing) => assert_eq!(existing.operation.id, "a"),
            other => panic!("{other:?}"),
        }
        // A held branch refuses the whole admission.
        assert_eq!(
            store
                .admit(
                    &keyed("c", "k2", &["z", "y"]),
                    &work,
                    &AdmissionQuota::default()
                )
                .unwrap(),
            Admission::Busy {
                branch: "y".into(),
                holder: "a".into()
            }
        );
        assert!(store.get("c").unwrap().is_none());
        assert_eq!(store.hold("r", "z", "a removal", "t", LEASE).unwrap(), None);
        store.unhold("r", "z", "t").unwrap();
        assert_eq!(store.pending(&["r".into()]).unwrap(), 1);
        assert_eq!(store.pending(&["other".into()]).unwrap(), 0);

        // Claimed once; a second worker finds nothing claimable.
        let (one, two) = (worker("one"), worker("two"));
        let claim = store
            .claim(&one, &["r".into()], &[], LEASE)
            .unwrap()
            .unwrap();
        assert_eq!(
            (claim.operation.operation.id.as_str(), claim.fence),
            ("a", 1)
        );
        assert_eq!(claim.work, work);
        assert!(store
            .claim(&two, &["r".into()], &[], LEASE)
            .unwrap()
            .is_none());
        assert!(store.renew(&one, "a", 1, LEASE).unwrap());
        assert!(!store.renew(&two, "a", 1, LEASE).unwrap());
        let mut done = claim.operation.clone();
        done.operation.state = OperationState::Succeeded;
        assert!(!store.finish(&two, 1, &done).unwrap());
        assert!(store.finish(&one, 1, &done).unwrap());
        assert_eq!(store.pending(&["r".into()]).unwrap(), 0);
        assert_eq!(
            store.get("a").unwrap().unwrap().operation.state,
            OperationState::Succeeded
        );
        // Its locks went with it.
        assert_eq!(
            store
                .admit(
                    &keyed("c", "k2", &["x", "y"]),
                    &work,
                    &AdmissionQuota::default()
                )
                .unwrap(),
            Admission::Admitted
        );
    }

    #[test]
    fn claims_follow_priority_fair_share_and_aging_on_sqlite() {
        check_scheduling(&MemoryStore::default(), "mem");
        let temp = temp("scheduling");
        check_scheduling(
            &SqliteStore::open(temp.path().join("state.db"), None).unwrap(),
            "file",
        );
    }

    #[test]
    fn share_ranks_put_equal_shares_together_and_no_usage_lowest() {
        let ranks = share_ranks(&[
            ("a".into(), 1.0),
            ("b".into(), 0.5),
            ("c".into(), 1.0),
            ("d".into(), 0.0),
        ]);
        assert_eq!(ranks["d"], ranks[""]);
        assert!(ranks[""] < ranks["b"] && ranks["b"] < ranks["a"]);
        assert_eq!(ranks["a"], ranks["c"]);
    }

    #[test]
    fn queue_and_branch_priority_read_what_admission_wrote() {
        let store = MemoryStore::default();
        let mut op = scheduled("r", "x", "acme", 7, 5, &["gpu"]);
        op.operation.branches = vec!["feature".into()];
        store
            .admit(&op, &serde_json::json!({}), &AdmissionQuota::default())
            .unwrap();
        let queue = store.queue().unwrap();
        assert_eq!(
            queue,
            [Queued {
                id: "x".into(),
                repo: "r".into(),
                tenant: "acme".into(),
                priority: 7,
                requires: vec!["gpu".into()],
                enqueued_ms: 5,
                claimed: false,
            }]
        );
        assert_eq!(store.branch_priority("r", "feature").unwrap(), Some(7));
        assert_eq!(store.branch_priority("r", "other").unwrap(), None);
        assert_eq!(store.branch_priority("q", "feature").unwrap(), None);
        let claim = store
            .claim(&worker("w"), &["r".into()], &["gpu".into()], LEASE)
            .unwrap()
            .unwrap();
        assert!(!claim.took_over);
        assert!(store.queue().unwrap()[0].claimed);
    }

    #[test]
    fn workers_claim_only_what_their_labels_allow_on_sqlite() {
        check_labels(&MemoryStore::default(), "mem");
        let temp = temp("labels");
        let path = temp.path().join("state.db");
        check_labels(&SqliteStore::open(&path, None).unwrap(), "file");
        // An older queue without the column gains it, its rows requiring
        // nothing.
        let old = temp.path().join("old.db");
        let conn = rusqlite::Connection::open(&old).unwrap();
        conn.execute_batch(
            "CREATE TABLE operation_queue (id TEXT PRIMARY KEY, seq INTEGER NOT NULL, \
             repo TEXT NOT NULL, work TEXT NOT NULL, attempt INTEGER NOT NULL DEFAULT 0, \
             worker TEXT, host TEXT, pid INTEGER, start TEXT, lease_until INTEGER); \
             INSERT INTO operation_queue (id, seq, repo, work) VALUES ('x', 1, 'r', '{}');",
        )
        .unwrap();
        drop(conn);
        let store = SqliteStore::open(&old, None).unwrap();
        let requires: String = store
            .conn()
            .query_row("SELECT requires FROM operation_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(requires, "[]");
        // An older workers table gains the inventory column; its rows
        // advertise none.
        let workers = temp.path().join("old-workers.db");
        let conn = rusqlite::Connection::open(&workers).unwrap();
        conn.execute_batch(
            "CREATE TABLE workers (id TEXT PRIMARY KEY, host TEXT NOT NULL, \
             pid INTEGER NOT NULL, labels TEXT NOT NULL, repos TEXT NOT NULL, \
             seen INTEGER NOT NULL); \
             INSERT INTO workers VALUES ('w_old', 'h', 1, '[]', '[\"r\"]', 9999999999999);",
        )
        .unwrap();
        drop(conn);
        let store = SqliteStore::open(&workers, None).unwrap();
        let live = store.workers(Duration::from_secs(60)).unwrap();
        assert_eq!(live[0].id, "w_old");
        assert_eq!(live[0].inventory, None);
        let worker = Worker {
            id: "w_new".into(),
            host: "h".into(),
            pid: 2,
            start: "s".into(),
        };
        let inventory = test_inventory("codex");
        store
            .beat(&worker, &[], &["r".into()], Some(&inventory))
            .unwrap();
        let live = store.workers(Duration::from_secs(60)).unwrap();
        let new = live.iter().find(|w| w.id == "w_new").unwrap();
        assert_eq!(new.inventory.as_ref(), Some(&inventory));
    }

    #[test]
    fn stores_opened_at_once_on_an_older_file_migrate_it_once() {
        let temp = temp("open-together");
        for (n, old) in [false, true].into_iter().enumerate() {
            let path = temp.path().join(format!("state-{n}.db"));
            if old {
                rusqlite::Connection::open(&path)
                    .unwrap()
                    .execute_batch(
                        "CREATE TABLE operation_queue (id TEXT PRIMARY KEY, \
                         seq INTEGER NOT NULL, repo TEXT NOT NULL, work TEXT NOT NULL, \
                         attempt INTEGER NOT NULL DEFAULT 0, worker TEXT, host TEXT, \
                         pid INTEGER, start TEXT, lease_until INTEGER); \
                         INSERT INTO operation_queue (id, seq, repo, work) \
                         VALUES ('x', 1, 'r', '{}');",
                    )
                    .unwrap();
            }
            let barrier = std::sync::Barrier::new(8);
            std::thread::scope(|scope| {
                let opens: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            SqliteStore::open(&path, None).map(drop)
                        })
                    })
                    .collect();
                for open in opens {
                    open.join()
                        .unwrap()
                        .map_err(|e| format!("old={old}: {e}"))
                        .unwrap();
                }
            });
            let store = SqliteStore::open(&path, None).unwrap();
            if old {
                let migrated: (String, i32, String, i64) = store
                    .conn()
                    .query_row(
                        "SELECT requires, priority, tenant, enqueued_ms FROM operation_queue",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .unwrap();
                assert_eq!(migrated, ("[]".into(), 0, "default".into(), 0));
            }
            let usage: i64 = store
                .conn()
                .query_row("SELECT COUNT(*) FROM tenant_usage", [], |r| r.get(0))
                .unwrap();
            assert_eq!(usage, 0);
        }
    }

    #[test]
    fn labels_are_checked_and_unclaimable_work_says_why() {
        for good in [
            "gpu",
            "linux",
            "x86_64",
            "a.b-c",
            "9",
            "harness:claude-code",
        ] {
            assert!(valid_label(good), "{good}");
        }
        for bad in ["", "GPU", "-x", ":x", "a b", "a,b", &"x".repeat(64)] {
            assert!(!valid_label(bad), "{bad}");
        }
        assert_eq!(unclaimable(&[], "r", &[]), None);
        let reason = unclaimable(&["gpu".into()], "r", &[]).unwrap();
        assert!(reason.contains("no live worker serves r"), "{reason}");
    }

    #[test]
    fn an_expired_claim_is_claimed_again_under_a_new_fence() {
        let store = MemoryStore::default();
        store
            .admit(
                &keyed("a", "k", &["x"]),
                &serde_json::json!({}),
                &AdmissionQuota::default(),
            )
            .unwrap();
        let (one, two) = (worker("one"), worker("two"));
        let repos = ["r".to_owned()];
        let first = store
            .claim(&one, &repos, &[], Duration::from_millis(1))
            .unwrap()
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let second = store.claim(&two, &repos, &[], LEASE).unwrap().unwrap();
        assert_eq!(second.fence, first.fence + 1);
        assert!(!first.took_over);
        assert!(second.took_over, "the first claim's lease had lapsed");
        // The first worker is fenced out of every write.
        assert!(!store.renew(&one, "a", first.fence, LEASE).unwrap());
        assert!(!store
            .start(&one, first.fence, &first.operation, LEASE)
            .unwrap());
        assert!(!store.finish(&one, first.fence, &first.operation).unwrap());
        // Given back unstarted, another claim follows at once.
        store.release(&two, "a", second.fence).unwrap();
        let third = store.claim(&one, &repos, &[], LEASE).unwrap().unwrap();
        assert_eq!(third.fence, second.fence + 1);
        assert!(!third.took_over, "a released claim is not taken over");
        // A reset store (its only process restarted) frees every claim.
        store.reset().unwrap();
        assert!(store.claim(&two, &repos, &[], LEASE).unwrap().is_some());
    }

    #[test]
    fn a_failed_queue_write_rolls_the_admission_back() {
        let store = MemoryStore::default();
        store
            .conn()
            .execute_batch(
                "CREATE TRIGGER fail_enqueue BEFORE INSERT ON operation_queue \
                 BEGIN SELECT RAISE(ABORT, 'injected queue failure'); END;",
            )
            .unwrap();
        let error = store
            .admit(
                &keyed("a", "k", &["x"]),
                &serde_json::json!({}),
                &AdmissionQuota::default(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("injected"), "{error}");
        assert!(store.get("a").unwrap().is_none());
        assert!(store.by_key("c", "k").unwrap().is_none());
        assert_eq!(store.hold("r", "x", "a removal", "t", LEASE).unwrap(), None);
        assert_eq!(store.pending(&["r".into()]).unwrap(), 0);
    }

    fn in_tenant(id: &str, tenant: &str, locks: &[&str], creates: &[&str]) -> StoredOperation {
        let mut stored = keyed(id, &format!("key-{id}"), locks);
        stored.tenant = tenant.into();
        stored.creates = creates.iter().map(|s| s.to_string()).collect();
        stored
    }

    #[test]
    fn a_quota_refusal_writes_nothing_and_terminal_operations_stop_counting() {
        let store = MemoryStore::default();
        let work = serde_json::json!({});
        let quota = AdmissionQuota {
            max_running: Some(1),
            ..AdmissionQuota::default()
        };
        assert_eq!(
            store
                .admit(&in_tenant("a", "acme", &["x"], &[]), &work, &quota)
                .unwrap(),
            Admission::Admitted
        );
        assert_eq!(
            store
                .admit(&in_tenant("b", "acme", &["y"], &[]), &work, &quota)
                .unwrap(),
            Admission::Quota {
                limit: "max_running",
                max: 1,
                reserved: 1
            }
        );
        // Nothing of the refused admission was written: no record, no key
        // binding, no queue row, no lock.
        assert!(store.get("b").unwrap().is_none());
        assert!(store.by_key("c", "key-b").unwrap().is_none());
        assert_eq!(store.pending(&["r".into()]).unwrap(), 1);
        assert_eq!(store.hold("r", "y", "a removal", "t", LEASE).unwrap(), None);
        store.unhold("r", "y", "t").unwrap();
        // Another tenant, and a record from before tenants (the default
        // tenant's), count on their own.
        assert_eq!(
            store
                .admit(&in_tenant("c", "other", &[], &[]), &work, &quota)
                .unwrap(),
            Admission::Admitted
        );
        assert_eq!(store.unfinished("acme").unwrap().len(), 1);
        assert_eq!(store.unfinished("other").unwrap().len(), 1);
        assert!(store.unfinished(DEFAULT_TENANT).unwrap().is_empty());
        // Finished: it no longer counts.
        let one = worker("one");
        let claim = store
            .claim(&one, &["r".into()], &[], LEASE)
            .unwrap()
            .unwrap();
        assert_eq!(claim.operation.operation.id, "a");
        let mut done = claim.operation.clone();
        done.operation.state = OperationState::Succeeded;
        assert!(store.finish(&one, claim.fence, &done).unwrap());
        assert_eq!(
            store
                .admit(&in_tenant("b", "acme", &["y"], &[]), &work, &quota)
                .unwrap(),
            Admission::Admitted
        );
    }

    #[test]
    fn max_branches_counts_existing_and_planned_branches_once() {
        let store = MemoryStore::default();
        let work = serde_json::json!({});
        let quota = || AdmissionQuota {
            max_branches: Some(3),
            existing_branches: Some(Box::new(|| {
                Ok([("r".to_owned(), "old".to_owned())].into_iter().collect())
            })),
            ..AdmissionQuota::default()
        };
        assert_eq!(
            store
                .admit(&in_tenant("a", "acme", &[], &["one"]), &work, &quota())
                .unwrap(),
            Admission::Admitted
        );
        // Two more would make four: refused, with the three reserved.
        assert_eq!(
            store
                .admit(
                    &in_tenant("b", "acme", &[], &["two", "three"]),
                    &work,
                    &quota()
                )
                .unwrap(),
            Admission::Quota {
                limit: "max_branches",
                max: 3,
                reserved: 2
            }
        );
        assert!(store.get("b").unwrap().is_none());
        // A branch that already exists is not counted twice.
        assert_eq!(
            store
                .admit(
                    &in_tenant("c", "acme", &[], &["old", "two"]),
                    &work,
                    &quota()
                )
                .unwrap(),
            Admission::Admitted
        );
        // An operation that creates nothing is not refused.
        assert_eq!(
            store
                .admit(&in_tenant("d", "acme", &[], &[]), &work, &quota())
                .unwrap(),
            Admission::Admitted
        );
    }
}
