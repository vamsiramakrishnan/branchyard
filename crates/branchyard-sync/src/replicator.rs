//! The replicator: local first, with a durable outbox.
//!
//! - [`Replicator::scan`] queues every local task whose refs differ from
//!   what was last agreed with the remote (cheap: a `for-each-ref`), and
//!   every few rounds queues every task, to bring remote changes in.
//! - [`Replicator::drain`] syncs the due tasks one at a time (objects
//!   within a task go up in parallel, bounded); a failure is kept in the
//!   outbox with its error and tried again later with exponential backoff
//!   and full jitter. Everything a sync does is idempotent, so a retry
//!   after a crash at any point is safe.
//! - [`Replicator::spawn`] runs both every `interval` on a thread of its
//!   own (`by serve`, `by worker`); `by sync` runs them once.
//!
//! Counters are added to the outbox's totals after each drain, so `by
//! sync status` shows what every process did.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use branchyard::services::Clock;
use serde::{Deserialize, Serialize};

use crate::engine::{Remote, SyncReport};
use crate::error::{Error, Result};
use crate::outbox::{LogEntry, Outbox, Pending};
use crate::source::SyncSource;
use crate::stats::Snapshot;
use crate::util::SplitMix;

/// The local tasks a replicator can sync.
pub trait SourceProvider: Send + Sync {
    /// Every task that exists here.
    fn sources(&self) -> Result<Vec<Box<dyn SyncSource>>>;
    /// One task by ID: one that exists here, or a new one to pull into.
    fn source(&self, task: &str) -> Result<Option<Box<dyn SyncSource>>>;
}

/// What a drain did.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DrainReport {
    pub synced: Vec<SyncReport>,
    /// `(task, error)`.
    pub failed: Vec<(String, String)>,
}

/// One task's sync state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStatus {
    pub task: String,
    /// `synced`, `pending` (queued), `failing` (queued after an error) or
    /// `local` (never synced).
    pub state: String,
    pub refs: usize,
    pub seq: u64,
    pub synced_ms: Option<u64>,
    /// How long its oldest unpushed change has waited.
    pub lag_ms: u64,
    pub last_error: Option<String>,
}

/// `by sync status`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub remote: String,
    pub device: String,
    pub encrypted: bool,
    pub tasks: Vec<TaskStatus>,
    pub pending: Vec<Pending>,
    pub counters: Snapshot,
    pub recent: Vec<LogEntry>,
}

pub struct Replicator {
    remote: Arc<Remote>,
    outbox: Arc<Outbox>,
    provider: Arc<dyn SourceProvider>,
    clock: Clock,
    flushed: Mutex<Snapshot>,
    rng: Mutex<SplitMix>,
    rounds: Mutex<u64>,
}

/// The longest a failing task waits between tries.
const MAX_BACKOFF_MS: u64 = 15 * 60 * 1000;
/// Every this many scans, every task is queued, to pull remote changes.
const REFRESH_EVERY: u64 = 10;

impl Replicator {
    pub fn new(
        remote: Arc<Remote>,
        outbox: Arc<Outbox>,
        provider: Arc<dyn SourceProvider>,
    ) -> Replicator {
        let clock = remote.clock().clone();
        Replicator {
            remote,
            outbox,
            provider,
            clock,
            flushed: Mutex::new(Snapshot::default()),
            rng: Mutex::new(SplitMix::new(crate::util::random_seed())),
            rounds: Mutex::new(0),
        }
    }

    pub fn remote(&self) -> &Arc<Remote> {
        &self.remote
    }

    pub fn outbox(&self) -> &Arc<Outbox> {
        &self.outbox
    }

    fn key(&self) -> String {
        self.remote.url()
    }

    /// Queue `task` for a push now.
    pub fn enqueue(&self, task: &str) -> Result<()> {
        self.outbox.enqueue(&self.key(), task, self.clock.now())
    }

    /// Queue every local task whose refs moved since its last sync, and
    /// every task each tenth scan. Returns the tasks queued.
    pub fn scan(&self) -> Result<Vec<String>> {
        let refresh = {
            let mut rounds = self.rounds.lock().unwrap_or_else(|e| e.into_inner());
            *rounds += 1;
            (*rounds).is_multiple_of(REFRESH_EVERY)
        };
        let key = self.key();
        let mut queued = Vec::new();
        for source in self.provider.sources()? {
            let task = source.task_id().to_owned();
            let state = self.outbox.state(&key, &task)?;
            let changed = source.refs()? != state.base
                || !source.segments()?.is_empty() && state.synced_ms.is_none();
            if changed || refresh {
                self.outbox.enqueue(&key, &task, self.clock.now())?;
                queued.push(task);
            }
        }
        Ok(queued)
    }

    /// Queue every local task.
    pub fn enqueue_all(&self) -> Result<usize> {
        let sources = self.provider.sources()?;
        for source in &sources {
            self.enqueue(source.task_id())?;
        }
        Ok(sources.len())
    }

    /// Sync one task now, recording the outcome.
    pub fn sync_task(&self, task: &str) -> Result<SyncReport> {
        let key = self.key();
        let started = self.clock.now();
        let source = self
            .provider
            .source(task)?
            .ok_or_else(|| Error::not_found(format!("there is no task {task} here")))?;
        let mut state = self.outbox.state(&key, task)?;
        let result = self.remote.sync(source.as_ref(), &mut state);
        // What was learned is kept even when the sync failed part way.
        self.outbox.save_state(&key, task, &state)?;
        match &result {
            Ok(report) => {
                self.outbox.done(&key, task, started)?;
                self.outbox
                    .record(&key, task, "synced", &describe(report), self.clock.now())?;
            }
            Err(e) => {
                self.outbox.enqueue(&key, task, started)?;
                let attempts = self
                    .outbox
                    .pending(&key)?
                    .into_iter()
                    .find(|p| p.task == task)
                    .map(|p| p.attempts)
                    .unwrap_or(0);
                let next = self.clock.now() + self.backoff(attempts + 1);
                self.outbox.failed(&key, task, &e.to_string(), next)?;
                self.outbox
                    .record(&key, task, "failed", &e.to_string(), self.clock.now())?;
            }
        }
        self.flush_counters()?;
        result
    }

    /// Pull one task now: import it and create or fast-forward its refs.
    pub fn pull(&self, task: &str) -> Result<SyncReport> {
        let key = self.key();
        let source = self
            .provider
            .source(task)?
            .ok_or_else(|| Error::not_found(format!("cannot make a place for {task} here")))?;
        let mut state = self.outbox.state(&key, task)?;
        let result = self.remote.pull(source.as_ref(), &mut state);
        self.outbox.save_state(&key, task, &state)?;
        if let Ok(report) = &result {
            self.outbox
                .record(&key, task, "pulled", &describe(report), self.clock.now())?;
        }
        self.flush_counters()?;
        result
    }

    fn backoff(&self, attempts: u32) -> u64 {
        let ceiling = (5_000u64 << attempts.min(20)).min(MAX_BACKOFF_MS);
        let mut rng = self.rng.lock().unwrap_or_else(|e| e.into_inner());
        1_000 + rng.below_or_at(ceiling)
    }

    /// Sync every due task.
    pub fn drain(&self) -> Result<DrainReport> {
        let mut report = DrainReport::default();
        for pending in self.outbox.due(&self.key(), self.clock.now())? {
            match self.sync_task(&pending.task) {
                Ok(r) => report.synced.push(r),
                Err(e) => report.failed.push((pending.task.clone(), e.to_string())),
            }
        }
        Ok(report)
    }

    /// Add what this process counted since the last flush to the totals.
    pub fn flush_counters(&self) -> Result<()> {
        let now = self.remote.stats().snapshot();
        let mut flushed = self.flushed.lock().unwrap_or_else(|e| e.into_inner());
        let delta = now.minus(&flushed);
        self.outbox.add_counters(&self.key(), &delta)?;
        *flushed = now;
        Ok(())
    }

    pub fn status(&self) -> Result<Status> {
        let key = self.key();
        let now = self.clock.now();
        let pending = self.outbox.pending(&key)?;
        let known = self.outbox.tasks(&key)?;
        let mut tasks = Vec::new();
        for source in self.provider.sources()? {
            let task = source.task_id().to_owned();
            let state = known
                .iter()
                .find(|(t, _)| *t == task)
                .map(|(_, s)| s.clone())
                .unwrap_or_default();
            let queued = pending.iter().find(|p| p.task == task);
            let refs = source.refs()?;
            let in_step = state.synced_ms.is_some() && refs == state.base;
            let (status, lag) = match (queued, in_step, state.synced_ms) {
                (Some(p), _, _) if p.last_error.is_some() => {
                    ("failing", now.saturating_sub(p.enqueued_ms))
                }
                (Some(p), _, _) => ("pending", now.saturating_sub(p.enqueued_ms)),
                (None, true, _) => ("synced", 0),
                (None, false, None) => ("local", 0),
                (None, false, Some(at)) => ("changed", now.saturating_sub(at)),
            };
            tasks.push(TaskStatus {
                task,
                state: status.into(),
                refs: refs.len(),
                seq: state.seq,
                synced_ms: state.synced_ms,
                lag_ms: lag,
                last_error: queued.and_then(|p| p.last_error.clone()),
            });
        }
        let persisted = self.outbox.counters(&key)?;
        let unflushed = {
            let flushed = self.flushed.lock().unwrap_or_else(|e| e.into_inner());
            self.remote.stats().snapshot().minus(&flushed)
        };
        Ok(Status {
            remote: key.clone(),
            device: self.remote.settings().device.clone(),
            encrypted: self.remote.sealer().sealed(),
            tasks,
            pending,
            counters: persisted.plus(&unflushed),
            recent: self.outbox.recent(&key, 20)?,
        })
    }

    /// The largest lag of any queued task, in milliseconds.
    pub fn lag_ms(&self) -> Result<u64> {
        let now = self.clock.now();
        Ok(self
            .outbox
            .pending(&self.key())?
            .iter()
            .map(|p| now.saturating_sub(p.enqueued_ms))
            .max()
            .unwrap_or(0))
    }

    /// Scan and drain every `interval` on a thread of its own, until the
    /// handle is dropped.
    pub fn spawn(self: Arc<Self>, interval: Duration) -> Result<ReplicatorHandle> {
        let signal = Arc::new((Mutex::new(Signal::default()), Condvar::new()));
        let thread = {
            let signal = signal.clone();
            std::thread::Builder::new()
                .name("by-sync".into())
                .spawn(move || loop {
                    if let Err(e) = self.scan().and_then(|_| self.drain()) {
                        eprintln!("branchyard sync: {e}");
                    }
                    let (lock, wake) = &*signal;
                    let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                    // A wake-up that came while the round ran is not lost.
                    let (mut guard, _) = wake
                        .wait_timeout_while(guard, interval, |s| !s.stop && !s.woken)
                        .unwrap_or_else(|e| e.into_inner());
                    if guard.stop {
                        break;
                    }
                    guard.woken = false;
                })
                .map_err(|e| Error::local(format!("sync thread: {e}")))?
        };
        Ok(ReplicatorHandle {
            signal,
            thread: Some(thread),
        })
    }
}

#[derive(Default)]
struct Signal {
    stop: bool,
    woken: bool,
}

/// Stops the replicator's thread when dropped.
pub struct ReplicatorHandle {
    signal: Arc<(Mutex<Signal>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl ReplicatorHandle {
    /// Run a round now instead of waiting for the interval.
    pub fn wake(&self) {
        self.signal
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .woken = true;
        self.signal.1.notify_all();
    }
}

impl Drop for ReplicatorHandle {
    fn drop(&mut self) {
        self.signal.0.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        self.signal.1.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One line for the log.
pub fn describe(report: &SyncReport) -> String {
    let mut parts = Vec::new();
    if report.swapped {
        parts.push(format!("seq {}", report.seq));
    }
    if !report.pushed.is_empty() {
        parts.push(format!("pushed {}", report.pushed.join(", ")));
    }
    if !report.pulled.is_empty() {
        parts.push(format!("pulled {}", report.pulled.join(", ")));
    }
    for (r, c) in &report.conflicts {
        parts.push(format!("{r} diverged: kept as {c}"));
    }
    if !report.behind.is_empty() {
        parts.push(format!("behind (checked out) {}", report.behind.join(", ")));
    }
    if let Some(p) = &report.pack {
        parts.push(format!("pack of {} objects, {} bytes", p.objects, p.bytes));
    }
    if report.chunks_up + report.chunks_down > 0 {
        parts.push(format!(
            "chunks {} up, {} down",
            report.chunks_up, report.chunks_down
        ));
    }
    match parts.is_empty() {
        true => "up to date".into(),
        false => parts.join("; "),
    }
}
