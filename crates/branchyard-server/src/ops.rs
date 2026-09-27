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
//! A dispatcher thread claims queued operations oldest first, up to
//! `max_running` at once, each under a lease it renews and a fence that
//! every later write names. A worker records an operation `running` before
//! it runs it, and its outcome after, releasing the branch locks in the
//! same transaction. A claim whose lease expired, or whose process is gone
//! from this host, is claimed again by any worker: an operation still
//! `queued` then runs, and one already `running` is recorded as
//! `interrupted`, never run again, since its turn may have started; the
//! engine recovers that turn's branch as it does any whose engine died.
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

use std::collections::HashMap;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use branchyard_client::api::{
    ErrorBody, Operation, OperationKind, OperationResult, OperationState,
};
use serde_json::Value;

use crate::config::Principal;
use crate::error::ApiError;
use crate::store::{
    Admission, AdmissionQuota, Claim, Idempotency, OperationStore, StoredOperation, Worker,
};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What a finished operation reports.
pub struct Finished {
    pub result: Result<OperationResult, ErrorBody>,
    /// The feed head once the operation's activity was ingested.
    pub end_cursor: Option<u64>,
}

/// Runs a claimed operation's work description, as the principal its
/// record names (`StoredOperation::principal`), never as the worker.
pub trait Executor: Send + Sync {
    fn execute(&self, operation: &StoredOperation, work: &Value) -> Finished;
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
}

/// How a registry dispatches.
#[derive(Clone, Debug)]
pub struct Options {
    /// Operations this process runs at once; more wait queued.
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
}

impl Options {
    pub fn new(repos: Vec<String>) -> Options {
        Options {
            max_running: 8,
            lease: DEFAULT_LEASE,
            poll: Duration::from_millis(250),
            repos,
            exclusive: true,
        }
    }
}

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
}

struct State {
    /// Operations this process runs, by ID, with their claim's fence.
    running: HashMap<String, i64>,
    /// Set by [`Registry::start`]; cleared at close.
    executor: Option<Arc<dyn Executor>>,
    accepting: bool,
    /// Shut down: nothing more is recorded.
    closed: bool,
    /// Something was admitted here since the dispatcher last looked.
    admitted: bool,
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
            }),
            changed: Condvar::new(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// This registry's worker identity, as its claims record it.
    pub fn worker(&self) -> &Worker {
        &self.worker
    }

    /// Start dispatching: claim queued operations and run them with
    /// `executor`.
    pub fn start(self: &Arc<Self>, executor: Arc<dyn Executor>) -> io::Result<()> {
        self.lock().executor = Some(executor);
        let registry = self.clone();
        std::thread::Builder::new()
            .name("branchyard-dispatch".into())
            .spawn(move || registry.dispatch())?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<Operation>, ApiError> {
        self.store
            .get(id)
            .map(|found| found.map(|s| s.operation))
            .map_err(|e| ApiError::internal(format!("could not read the operation: {e}")))
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
            .map(|stored| stored.operation))
    }

    /// `tenant`'s queued and running operations, as the store holds them.
    pub fn unfinished(&self, tenant: &str) -> Result<Vec<StoredOperation>, ApiError> {
        self.store
            .unfinished(tenant)
            .map_err(|e| ApiError::internal(format!("could not read the operations: {e}")))
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
                    .map(|s| s.operation)
            })
            .map_err(|e| ApiError::internal(format!("could not read the operation: {e}")))
    }

    /// Admit a new operation durably, with `work` describing what to run,
    /// or return the one an earlier request with the same key created
    /// (`true`).
    pub fn submit(&self, new: NewOperation, work: Value) -> Result<(Operation, bool), ApiError> {
        if let Some(idem) = &new.idempotency {
            if let Some(existing) = self.replay(idem, &new.principal.tenant)? {
                return Ok((existing, true));
            }
        }
        if !self.lock().accepting {
            return Err(ApiError::shutting_down());
        }
        let id = format!("op_{}", &branchyard_client::new_key()[..24]);
        let stored = StoredOperation {
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
            },
            idempotency: new.idempotency,
            locks: new.locks,
            tenant: new.principal.tenant.clone(),
            principal: Some(new.principal),
            creates: new.creates,
        };
        let admitted = self
            .store
            .admit(&stored, &work, &new.quota)
            .map_err(|e| ApiError::internal(format!("could not record the operation: {e}")))?;
        match admitted {
            Admission::Admitted => {
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
        loop {
            let (free, executor) = {
                let mut state = self.lock();
                if state.closed || !state.accepting {
                    return;
                }
                state.admitted = false;
                (
                    state.running.len() < self.options.max_running,
                    state.executor.clone(),
                )
            };
            let Some(executor) = executor else { return };
            if renewed.elapsed() >= renew_every {
                self.renew();
                renewed = Instant::now();
            }
            if free {
                match self
                    .store
                    .claim(&self.worker, &self.options.repos, self.options.lease)
                {
                    Ok(Some(claim)) => {
                        self.run(claim, executor);
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => tracing::error!(error = %e, "could not claim an operation"),
                }
            }
            let wait = self.options.poll.min(renew_every);
            let state = self.lock();
            if !state.admitted || !free {
                let _ = self
                    .changed
                    .wait_timeout(state, wait)
                    .unwrap_or_else(|p| p.into_inner());
            }
        }
    }

    /// Extend the lease of every operation running here.
    fn renew(&self) {
        let running: Vec<(String, i64)> = self
            .lock()
            .running
            .iter()
            .map(|(id, fence)| (id.clone(), *fence))
            .collect();
        for (id, fence) in running {
            match self
                .store
                .renew(&self.worker, &id, fence, self.options.lease)
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(%id, "lost the claim on an operation; another worker took it over")
                }
                Err(e) => {
                    tracing::error!(%id, error = %e, "could not renew the claim on an operation")
                }
            }
        }
    }

    /// Run a claimed operation on a thread of its own.
    fn run(self: &Arc<Self>, claim: Claim, executor: Arc<dyn Executor>) {
        let id = claim.operation.operation.id.clone();
        {
            let mut state = self.lock();
            if state.closed || !state.accepting {
                drop(state);
                let _ = self.store.release(&self.worker, &id, claim.fence);
                return;
            }
            state.running.insert(id.clone(), claim.fence);
        }
        let registry = self.clone();
        let fence = claim.fence;
        let spawned = std::thread::Builder::new()
            .name(format!("branchyard-{id}"))
            .spawn(move || registry.work(claim, executor));
        if let Err(error) = spawned {
            tracing::error!(%id, %error, "could not start a worker thread for an operation");
            let _ = self.store.release(&self.worker, &id, fence);
            self.done(&id);
        }
    }

    fn work(&self, claim: Claim, executor: Arc<dyn Executor>) {
        let Claim {
            operation: mut stored,
            work,
            fence,
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
                let _ = self.store.release(&self.worker, &id, fence);
                self.done(&id);
                return;
            }
        }
        let admitted = stored.clone();
        let finished = catch_unwind(AssertUnwindSafe(|| executor.execute(&admitted, &work)))
            .unwrap_or_else(|_| Finished {
                result: Err(*ApiError::internal("the operation panicked; see the server log").body),
                end_cursor: None,
            });
        let (outcome, result, error) = match finished.result {
            Ok(result) => (OperationState::Succeeded, Some(result), None),
            Err(error) => (OperationState::Failed, None, Some(error)),
        };
        stored.operation.state = outcome;
        stored.operation.result = result;
        stored.operation.error = error;
        stored.operation.end_cursor = finished.end_cursor;
        stored.operation.finished_at_ms = Some(now_ms());
        self.record(&stored, fence);
        self.done(&id);
    }

    /// Record an outcome under the claim `fence`. After shutdown, or once
    /// another worker took the claim over, the record is left as they
    /// wrote it.
    fn record(&self, stored: &StoredOperation, fence: i64) -> bool {
        let id = &stored.operation.id;
        match self.store.finish(&self.worker, fence, stored) {
            Ok(true) => true,
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
        self.lock().running.remove(id);
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
            let _ = self
                .changed
                .wait_timeout(state, (deadline - now).min(Duration::from_millis(20)))
                .unwrap_or_else(|p| p.into_inner());
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
            state.running.drain().collect()
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
        self.lock().closed = true;
        self.changed.notify_all();
        count
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MemoryStore, SqliteStore};
    use std::sync::mpsc;

    /// Runs work `{"n": N}` by recording N; `{"wait": true}` blocks until
    /// released.
    #[derive(Default)]
    struct Recorder {
        ran: Mutex<Vec<u64>>,
        gate: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl Executor for Recorder {
        fn execute(&self, _: &StoredOperation, work: &Value) -> Finished {
            if work["wait"] == true {
                let gate = self.gate.lock().unwrap().take();
                if let Some(gate) = gate {
                    let _ = gate.recv();
                }
            }
            if let Some(n) = work["n"].as_u64() {
                self.ran.lock().unwrap().push(n);
            }
            Finished {
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

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("branchyard-ops-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("state.db")
    }

    #[test]
    fn close_interrupts_what_runs_and_a_restart_runs_what_was_queued() {
        let db = temp_db("close");
        let (registry, recorder) = started(Box::new(SqliteStore::open(&db, None).unwrap()), 1);
        let (release, gate) = mpsc::channel::<()>();
        *recorder.gate.lock().unwrap() = Some(gate);
        let (running, _) = registry
            .submit(new(None, &["a"]), serde_json::json!({ "wait": true }))
            .unwrap();
        let (queued, _) = registry
            .submit(new(None, &["b"]), serde_json::json!({ "n": 7 }))
            .unwrap();
        while state(&registry, &running.id) != OperationState::Running {
            std::thread::sleep(Duration::from_millis(5));
        }
        registry.stop_accepting();
        assert!(!registry.wait_running(Duration::from_millis(50)));
        assert_eq!(registry.close(), 1);
        release.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
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
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn a_crash_between_admission_and_execution_runs_it_once_elsewhere() {
        let db = temp_db("crash");
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
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    #[test]
    fn an_expired_claim_is_taken_over_and_a_started_one_is_never_rerun() {
        let db = temp_db("takeover");
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
        let lease = Duration::from_millis(100);
        let repos = ["r".to_owned()];
        let first = store.claim(&ghost, &repos, lease).unwrap().unwrap();
        let second = store.claim(&ghost, &repos, lease).unwrap().unwrap();
        assert_eq!(first.operation.operation.id, unstarted.id);
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
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }
}
