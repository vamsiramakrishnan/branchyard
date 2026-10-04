//! A server with `sync` (docs/sync.md): each served repository's branches
//! replicated to the configured remote.
//!
//! - **Before an operation** on existing branches, the server pulls each
//!   branch's task (creating or fast-forwarding its refs; a clean
//!   worktree follows) and takes the task's lease, renewed while the
//!   operation runs and registered in the repository's service registry.
//!   A lease another runner holds, or one the remote cannot grant, fails
//!   the operation with `409 sync_lease_held` or `503 sync_unavailable`:
//!   two machines never run one task at once.
//! - **While it runs**, a replicator per repository looks for changed
//!   refs every `interval` and pushes them (checkpoints land as turns
//!   end), so a phone or another machine sees the work as it goes.
//! - **If a lease is lost** while it runs (a renewal finds another
//!   runner took it over), the run is stopped: its branches' turns are
//!   cancelled, naming the loss, the task is fenced so the replicator no
//!   longer pushes it, and the operation fails with `409
//!   sync_lease_lost` instead of returning its result. The fence lifts
//!   when this server takes the task's lease again.
//! - **After it**, the operation's branches are queued and the replicator
//!   woken, then the leases are released.
//!
//! Each repository has its own outbox, `<data_dir>/sync/<repo>.db`.

use branchyard_support::best_effort;
use branchyard_support::LockExt as _;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use branchyard::services::{Clock, ServiceStore};
use branchyard_sync::lease::LeaseKeeper;
use branchyard_sync::outbox::Outbox;
use branchyard_sync::replicator::{Replicator, ReplicatorHandle};
use branchyard_sync::yard::YardTasks;
use branchyard_sync::{Kind, Remote};

use crate::api::RepoState;
use crate::config::Config;

/// The lease's attempt name for a branch: the branch is the attempt.
pub const ATTEMPT: &str = "run";

struct RepoSync {
    remote: Arc<Remote>,
    replicator: Arc<Replicator>,
    tasks: YardTasks,
    registry: Option<Arc<dyn ServiceStore>>,
}

pub struct ServerSync {
    repos: BTreeMap<String, RepoSync>,
    lease: Duration,
    interval: Duration,
    holder: String,
    handles: Mutex<Vec<ReplicatorHandle>>,
}

impl std::fmt::Debug for ServerSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ServerSync({:?})", self.repos.keys().collect::<Vec<_>>())
    }
}

/// A task's lease, held for an operation on one of its branches.
pub struct HeldLease {
    pub branch: String,
    pub task: String,
    pub keeper: LeaseKeeper,
}

/// Why an operation could not start.
pub enum Refusal {
    Held(String),
    Unavailable(String),
}

impl ServerSync {
    /// Open the remote for every served repository, or `None` without
    /// `sync` in the configuration.
    pub fn open(
        config: &Config,
        repos: &BTreeMap<String, RepoState>,
    ) -> Result<Option<Arc<ServerSync>>, String> {
        let Some(settings) = &config.sync else {
            return Ok(None);
        };
        let dir = config.data_dir.join("sync");
        std::fs::create_dir_all(&dir).map_err(|e| format!("sync: {}: {e}", dir.display()))?;
        let mut out = BTreeMap::new();
        let mut device = String::new();
        for (name, repo) in repos {
            let outbox = Arc::new(
                Outbox::open(&dir.join(format!("{name}.db")))
                    .map_err(|e| format!("sync: {name}: {e}"))?,
            );
            let remote = Arc::new(
                settings
                    .config
                    .open(&outbox, Clock::system())
                    .map_err(|e| format!("sync: {name}: {e}"))?,
            );
            device = remote.settings().device.clone();
            let tasks =
                YardTasks::new(repo.yard.clone()).map_err(|e| format!("sync: {name}: {e}"))?;
            let replicator = Arc::new(Replicator::new(
                remote.clone(),
                outbox,
                Arc::new(tasks.clone()),
            ));
            let registry = repo
                .yard
                .services()
                .ok()
                .map(|r| r as Arc<dyn ServiceStore>);
            out.insert(
                name.clone(),
                RepoSync {
                    remote,
                    replicator,
                    tasks,
                    registry,
                },
            );
        }
        Ok(Some(Arc::new(ServerSync {
            repos: out,
            lease: settings.lease,
            interval: settings.config.interval(),
            holder: branchyard_sync::lease::holder_for_this_process(&device),
            handles: Mutex::new(Vec::new()),
        })))
    }

    /// Start each repository's replicator.
    pub fn start(&self) -> Result<(), String> {
        let mut handles = self.handles.lock_recovering("handles");
        for (name, repo) in &self.repos {
            handles.push(
                repo.replicator
                    .clone()
                    .spawn(self.interval)
                    .map_err(|e| format!("sync: {name}: {e}"))?,
            );
        }
        Ok(())
    }

    /// Stop the replicators (each finishes the round it is in).
    pub fn stop(&self) {
        self.handles.lock_recovering("handles").clear();
    }

    /// Stop the replicators, then push what is queued once more.
    pub fn finish(&self) {
        self.stop();
        for (name, r) in &self.repos {
            if let Err(e) = r.replicator.drain() {
                eprintln!("branchyard-server: sync: {name}: {e}");
            }
        }
    }

    /// Pull each branch's task and take its lease, for an operation on
    /// `branches` of `repo`. The leases are held until the returned
    /// keepers are dropped. A task fenced after an earlier loss is pushed
    /// again once its lease is taken.
    pub fn begin(&self, repo: &str, branches: &[String]) -> Result<Vec<HeldLease>, Refusal> {
        let Some(r) = self.repos.get(repo) else {
            return Ok(Vec::new());
        };
        let mut keepers = Vec::new();
        for branch in branches {
            let task = r
                .tasks
                .task_id(branch)
                .map_err(|e| Refusal::Unavailable(e.to_string()))?;
            match r.replicator.pull(&task) {
                Ok(_) => {}
                // Not in the remote yet: nothing to pull.
                Err(e) if e.is(Kind::NotFound) => {}
                // Local first: an operation still runs on what is here.
                Err(e) => eprintln!("branchyard-server: sync: pulling {task}: {e}"),
            }
            let keeper = LeaseKeeper::start(
                r.remote.clone(),
                &task,
                ATTEMPT,
                &self.holder,
                self.lease,
                r.registry.clone(),
            )
            .map_err(|e| match e.kind {
                Kind::LeaseHeld => Refusal::Held(e.message),
                _ => Refusal::Unavailable(format!("the lease on {task} could not be taken: {e}")),
            })?;
            r.replicator.unfence(&task);
            keepers.push(HeldLease {
                branch: branch.clone(),
                task,
                keeper,
            });
        }
        Ok(keepers)
    }

    /// Stop pushing `task` of `repo`: this server lost its lease on it.
    pub fn fence(&self, repo: &str, task: &str, reason: &str) {
        if let Some(r) = self.repos.get(repo) {
            r.replicator.fence(task, reason);
        }
    }

    /// Queue an operation's branches and wake the replicator.
    pub fn end(&self, repo: &str, branches: &[String]) {
        let Some(r) = self.repos.get(repo) else {
            return;
        };
        for branch in branches {
            if let Ok(task) = r.tasks.task_id(branch) {
                best_effort("queue the task for sync", r.replicator.enqueue(&task));
            }
        }
        for handle in self.handles.lock_recovering("handles").iter() {
            handle.wake();
        }
    }

    /// Each repository's counters, queued tasks and lag, for `/metrics`.
    pub fn observe(&self) -> Vec<(String, branchyard_sync::stats::Snapshot, usize, f64)> {
        self.repos
            .iter()
            .map(|(name, r)| {
                let pending = r
                    .replicator
                    .outbox()
                    .pending(&r.remote.url())
                    .map(|p| p.len())
                    .unwrap_or(0);
                let lag = r.replicator.lag_ms().unwrap_or(0) as f64 / 1000.0;
                (name.clone(), r.remote.stats().snapshot(), pending, lag)
            })
            .collect()
    }
}
