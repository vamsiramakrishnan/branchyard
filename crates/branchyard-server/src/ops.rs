//! The operation registry: long operations accepted durably, run in the
//! background on bounded worker threads, and looked up by ID.
//!
//! An operation is saved before it is acknowledged and before it starts.
//! Queued operations start in the order they were accepted.
//! Its idempotency key, scoped to the caller, maps every retry to the same
//! operation, and a retry never starts a second run. Branch locks keep two
//! operations from changing one branch at once, which the SDK does not
//! coordinate on its own.
//!
//! Operations run on their own threads, not on a request, so a client that
//! disconnects changes nothing (invariant 1). An operation still queued or
//! running when the server stops is recorded as `interrupted`, at shutdown
//! or at the next start; the engine recovers its branches when the server
//! next opens the repository.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use branchyard_client::api::{
    ErrorBody, Operation, OperationKind, OperationResult, OperationState,
};

use crate::error::ApiError;
use crate::store::{Idempotency, OperationStore, StoredOperation};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What a finished job reports.
pub struct Finished {
    pub result: Result<OperationResult, ErrorBody>,
    /// The feed head once the job's activity was ingested.
    pub end_cursor: Option<u64>,
}

pub type Job = Box<dyn FnOnce() -> Finished + Send + 'static>;

pub struct NewOperation {
    pub repo: String,
    pub kind: OperationKind,
    pub branches: Vec<String>,
    pub cursor: u64,
    /// Branches of `repo` to lock until the operation finishes.
    pub locks: Vec<String>,
    pub idempotency: Option<Idempotency>,
}

pub struct Registry {
    store: Box<dyn OperationStore>,
    state: Mutex<State>,
    changed: Condvar,
    max_running: usize,
}

struct State {
    ops: HashMap<String, StoredOperation>,
    /// `(caller, key)` to operation ID.
    keys: HashMap<(String, String), String>,
    /// `(repo, branch)` to the operation or request holding it.
    busy: HashMap<(String, String), String>,
    /// Queued operations, oldest first; they start in this order.
    queue: VecDeque<String>,
    running: usize,
    accepting: bool,
    /// Shut down: nothing more is saved.
    closed: bool,
}

fn interrupted(message: &str) -> ErrorBody {
    ErrorBody {
        code: "interrupted".into(),
        message: message.into(),
        detail: None,
    }
}

const STOPPED: &str = "the server stopped before this operation finished; \
     a turn it left running is recovered as interrupted when its repository is next opened";
const NOT_STARTED: &str = "the server shut down before this operation started";

impl Registry {
    /// Load every saved operation. Any left queued or running by a server
    /// that stopped is recorded as interrupted.
    pub fn open(store: Box<dyn OperationStore>, max_running: usize) -> io::Result<Arc<Registry>> {
        let mut ops = HashMap::new();
        let mut keys = HashMap::new();
        for mut stored in store.load()? {
            if !stored.operation.state.is_terminal() {
                stored.operation.state = OperationState::Interrupted;
                stored.operation.error = Some(interrupted(STOPPED));
                stored.operation.finished_at_ms = Some(now_ms());
                store.save(&stored)?;
            }
            if let Some(idem) = &stored.idempotency {
                keys.insert(
                    (idem.caller.clone(), idem.key.clone()),
                    stored.operation.id.clone(),
                );
            }
            ops.insert(stored.operation.id.clone(), stored);
        }
        Ok(Arc::new(Registry {
            store,
            state: Mutex::new(State {
                ops,
                keys,
                busy: HashMap::new(),
                queue: VecDeque::new(),
                running: 0,
                accepting: true,
                closed: false,
            }),
            changed: Condvar::new(),
            max_running: max_running.max(1),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn get(&self, id: &str) -> Option<Operation> {
        self.lock().ops.get(id).map(|s| s.operation.clone())
    }

    /// The operation an earlier request with this key created, if any.
    pub fn replay(&self, idem: &Idempotency) -> Result<Option<Operation>, ApiError> {
        replay(&self.lock(), idem)
    }

    /// Record a new operation durably and start it, or return the one an
    /// earlier request with the same key created (`true`).
    pub fn submit(
        self: &Arc<Self>,
        new: NewOperation,
        job: Job,
    ) -> Result<(Operation, bool), ApiError> {
        let mut state = self.lock();
        if let Some(idem) = &new.idempotency {
            if let Some(existing) = replay(&state, idem)? {
                return Ok((existing, true));
            }
        }
        if !state.accepting {
            return Err(ApiError::shutting_down());
        }
        for branch in &new.locks {
            if let Some(holder) = state.busy.get(&(new.repo.clone(), branch.clone())) {
                return Err(busy(branch, holder));
            }
        }
        let id = format!("op_{}", &branchyard_client::new_key()[..24]);
        let stored = StoredOperation {
            operation: Operation {
                id: id.clone(),
                repo: new.repo.clone(),
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
        };
        self.store
            .save(&stored)
            .map_err(|e| ApiError::internal(format!("could not record the operation: {e}")))?;
        if let Some(idem) = &stored.idempotency {
            state
                .keys
                .insert((idem.caller.clone(), idem.key.clone()), id.clone());
        }
        for branch in &stored.locks {
            state
                .busy
                .insert((new.repo.clone(), branch.clone()), id.clone());
        }
        let operation = stored.operation.clone();
        state.ops.insert(id.clone(), stored);
        state.queue.push_back(id.clone());
        drop(state);
        let registry = self.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("branchyard-{id}"))
            .spawn(move || registry.work(&id, job));
        if let Err(error) = spawned {
            // Recorded but not started: say so rather than leave it queued.
            let mut state = self.lock();
            let message = format!("could not start a worker thread: {error}");
            self.finish(
                &mut state,
                &operation.id,
                OperationState::Failed,
                None,
                Some(*ApiError::internal(message).body),
                None,
            );
            return Ok((state.ops[&operation.id].operation.clone(), false));
        }
        Ok((operation, false))
    }

    fn work(&self, id: &str, job: Job) {
        {
            let mut state = self.lock();
            loop {
                if state.closed || !state.accepting {
                    self.finish(
                        &mut state,
                        id,
                        OperationState::Interrupted,
                        None,
                        Some(interrupted(NOT_STARTED)),
                        None,
                    );
                    return;
                }
                let next = state.queue.front().is_some_and(|first| first == id);
                if next && state.running < self.max_running {
                    state.queue.pop_front();
                    state.running += 1;
                    // The next in line may have a slot too.
                    self.changed.notify_all();
                    break;
                }
                state = self.changed.wait(state).unwrap_or_else(|p| p.into_inner());
            }
            if let Some(stored) = state.ops.get_mut(id) {
                stored.operation.state = OperationState::Running;
                let stored = stored.clone();
                if let Err(error) = self.store.save(&stored) {
                    eprintln!("branchyard-server: could not record {id} as running: {error}");
                }
            }
        }
        let finished = catch_unwind(AssertUnwindSafe(job)).unwrap_or_else(|_| Finished {
            result: Err(*ApiError::internal("the operation panicked; see the server log").body),
            end_cursor: None,
        });
        let mut state = self.lock();
        state.running -= 1;
        let (outcome, result, error) = match finished.result {
            Ok(result) => (OperationState::Succeeded, Some(result), None),
            Err(error) => (OperationState::Failed, None, Some(error)),
        };
        self.finish(&mut state, id, outcome, result, error, finished.end_cursor);
    }

    /// Record an outcome, release the operation's locks and wake waiters.
    /// After shutdown the record is left as shutdown wrote it.
    fn finish(
        &self,
        state: &mut State,
        id: &str,
        outcome: OperationState,
        result: Option<OperationResult>,
        error: Option<ErrorBody>,
        end_cursor: Option<u64>,
    ) {
        state.queue.retain(|queued| queued != id);
        if state.closed {
            self.changed.notify_all();
            return;
        }
        if let Some(stored) = state.ops.get_mut(id) {
            stored.operation.state = outcome;
            stored.operation.result = result;
            stored.operation.error = error;
            stored.operation.end_cursor = end_cursor;
            stored.operation.finished_at_ms = Some(now_ms());
            let stored = stored.clone();
            if let Err(e) = self.store.save(&stored) {
                eprintln!("branchyard-server: could not record the outcome of {id}: {e}");
            }
            for branch in &stored.locks {
                state
                    .busy
                    .remove(&(stored.operation.repo.clone(), branch.clone()));
            }
        }
        self.changed.notify_all();
    }

    /// Hold `branch` for a short synchronous change, such as a removal.
    pub fn hold(
        self: &Arc<Self>,
        repo: &str,
        branch: &str,
        holder: &str,
    ) -> Result<Hold, ApiError> {
        let mut state = self.lock();
        if !state.accepting {
            return Err(ApiError::shutting_down());
        }
        let key = (repo.to_owned(), branch.to_owned());
        if let Some(existing) = state.busy.get(&key) {
            return Err(busy(branch, existing));
        }
        state.busy.insert(key.clone(), holder.to_owned());
        Ok(Hold {
            registry: self.clone(),
            key,
        })
    }

    /// Refuse new operations; queued ones will not start.
    pub fn stop_accepting(&self) {
        self.lock().accepting = false;
        self.changed.notify_all();
    }

    /// Wait until nothing is queued or running, for at most `timeout`.
    /// True when idle.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            let pending = state.ops.values().any(|s| !s.operation.state.is_terminal());
            if !pending {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// Record every unfinished operation as interrupted and save nothing
    /// more. Returns how many were interrupted.
    pub fn close(&self) -> usize {
        let mut state = self.lock();
        state.accepting = false;
        let mut count = 0;
        let ids: Vec<String> = state
            .ops
            .values()
            .filter(|s| !s.operation.state.is_terminal())
            .map(|s| s.operation.id.clone())
            .collect();
        for id in ids {
            let message = match state.ops[&id].operation.state {
                OperationState::Queued => NOT_STARTED,
                _ => STOPPED,
            };
            self.finish(
                &mut state,
                &id,
                OperationState::Interrupted,
                None,
                Some(interrupted(message)),
                None,
            );
            count += 1;
        }
        state.closed = true;
        self.changed.notify_all();
        count
    }
}

fn replay(state: &State, idem: &Idempotency) -> Result<Option<Operation>, ApiError> {
    let Some(id) = state.keys.get(&(idem.caller.clone(), idem.key.clone())) else {
        return Ok(None);
    };
    let stored = &state.ops[id];
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
        .detail(serde_json::json!({ "operation": id })));
    }
    Ok(Some(stored.operation.clone()))
}

fn busy(branch: &str, holder: &str) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "branch_busy",
        format!("branch {branch} is busy with {holder}"),
    )
    .detail(serde_json::json!({ "branch": branch, "holder": holder }))
}

/// A branch held by [`Registry::hold`], released on drop.
pub struct Hold {
    registry: Arc<Registry>,
    key: (String, String),
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.registry.lock().busy.remove(&self.key);
        self.registry.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use std::sync::mpsc;

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
        }
    }

    fn done() -> Finished {
        Finished {
            result: Ok(OperationResult::default()),
            end_cursor: Some(0),
        }
    }

    #[test]
    fn a_repeated_key_returns_the_first_operation_and_runs_once() {
        let registry = Registry::open(Box::new(MemoryStore::default()), 2).unwrap();
        let (tx, rx) = mpsc::channel();
        let job = move |tx: mpsc::Sender<()>| -> Job {
            Box::new(move || {
                tx.send(()).unwrap();
                done()
            })
        };
        let (first, replayed) = registry
            .submit(new(Some("k"), &[]), job(tx.clone()))
            .unwrap();
        assert!(!replayed);
        let (again, replayed) = registry.submit(new(Some("k"), &[]), job(tx)).unwrap();
        assert!(replayed);
        assert_eq!(first.id, again.id);
        assert!(registry.wait_idle(Duration::from_secs(5)));
        assert_eq!(rx.try_iter().count(), 1);
        assert_eq!(
            registry.get(&first.id).unwrap().state,
            OperationState::Succeeded
        );

        let mut other = new(Some("k"), &[]);
        other.idempotency.as_mut().unwrap().fingerprint = "g".into();
        let error = registry.submit(other, Box::new(done)).unwrap_err();
        assert_eq!(error.body.code, "idempotency_key_reused");
    }

    #[test]
    fn locks_refuse_a_second_change_until_the_first_finishes() {
        let registry = Registry::open(Box::new(MemoryStore::default()), 2).unwrap();
        let (release, wait) = mpsc::channel::<()>();
        let (op, _) = registry
            .submit(
                new(None, &["b"]),
                Box::new(move || {
                    wait.recv().unwrap();
                    done()
                }),
            )
            .unwrap();
        let error = registry
            .submit(new(None, &["b"]), Box::new(done))
            .unwrap_err();
        assert_eq!(error.body.code, "branch_busy");
        assert!(registry.hold("r", "b", "delete").is_err());
        release.send(()).unwrap();
        assert!(registry.wait_idle(Duration::from_secs(5)));
        assert_eq!(
            registry.get(&op.id).unwrap().state,
            OperationState::Succeeded
        );
        let hold = registry.hold("r", "b", "delete").unwrap();
        assert!(registry.submit(new(None, &["b"]), Box::new(done)).is_err());
        drop(hold);
        assert!(registry.submit(new(None, &["b"]), Box::new(done)).is_ok());
    }

    #[test]
    fn queued_operations_start_in_order() {
        let registry = Registry::open(Box::new(MemoryStore::default()), 1).unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (release, wait) = mpsc::channel::<()>();
        registry
            .submit(
                new(None, &[]),
                Box::new(move || {
                    wait.recv().unwrap();
                    done()
                }),
            )
            .unwrap();
        for n in 0..5 {
            let order = order.clone();
            registry
                .submit(
                    new(None, &[]),
                    Box::new(move || {
                        order.lock().unwrap().push(n);
                        done()
                    }),
                )
                .unwrap();
        }
        release.send(()).unwrap();
        assert!(registry.wait_idle(Duration::from_secs(5)));
        assert_eq!(*order.lock().unwrap(), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn close_interrupts_and_reopening_keeps_the_record() {
        let store = Arc::new(MemoryStore::default());
        struct Shared(Arc<MemoryStore>);
        impl OperationStore for Shared {
            fn load(&self) -> io::Result<Vec<StoredOperation>> {
                self.0.load()
            }
            fn save(&self, op: &StoredOperation) -> io::Result<()> {
                self.0.save(op)
            }
        }
        let registry = Registry::open(Box::new(Shared(store.clone())), 1).unwrap();
        let (release, wait) = mpsc::channel::<()>();
        let (running, _) = registry
            .submit(
                new(None, &[]),
                Box::new(move || {
                    let _ = wait.recv();
                    done()
                }),
            )
            .unwrap();
        let (queued, _) = registry.submit(new(None, &[]), Box::new(done)).unwrap();
        while registry.get(&running.id).unwrap().state != OperationState::Running {
            std::thread::sleep(Duration::from_millis(5));
        }
        registry.stop_accepting();
        assert!(!registry.wait_idle(Duration::from_millis(50)));
        assert_eq!(registry.close(), 1, "the queued one gave up on its own");
        release.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let reopened = Registry::open(Box::new(Shared(store)), 1).unwrap();
        for (id, code) in [(&running.id, STOPPED), (&queued.id, NOT_STARTED)] {
            let op = reopened.get(id).unwrap();
            assert_eq!(op.state, OperationState::Interrupted);
            assert_eq!(op.error.unwrap().message, code);
        }
    }
}
