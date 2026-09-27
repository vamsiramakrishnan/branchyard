//! Durable state: `.branchyard/state.db`, and the directories the engine
//! owns (worktrees, private homes, delegation tokens).
//!
//! [`Store`] is what the engine uses. It forwards to a [`Backend`], the one
//! abstraction for durable state: branch records, the event log, journaled
//! steps, leases, harness process identities and cancel signals. Local mode
//! uses [`crate::sqlite::Sqlite`]; `docs/durability.md` maps the same
//! operations onto PostgreSQL for the server.
//!
//! Every write for a running turn carries a [`Fence`]: the turn's lease and
//! the generation it was granted. A backend refuses a fenced write once the
//! lease has moved on, in the same transaction as the write, so an engine
//! that lost its lease cannot change the branch.
//!
//! A branch name is reserved by creating its row without a record, with
//! the reserving engine named like a lease's owner; the record is written
//! when the branch is created. A reservation whose engine is gone from this
//! host, or that is older than [`RESERVATION_TTL`], is reclaimed by
//! recovery. A record's `children`
//! belong to [`Store::add_child`]: every other write keeps the list in the
//! store, so a turn that ends after it spawned children cannot drop them.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::delegation::Grant;
use crate::{proc, BranchInfo, Error, Provider, RecordedEvent};

pub(crate) const DIR: &str = ".branchyard";

/// How long a lease lasts without renewal.
pub(crate) const LEASE_TTL: Duration = Duration::from_secs(30);
/// How often a running turn renews its lease.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(5);
/// How long a reservation may stay without its branch being created
/// before any engine may reclaim it. Creating the branch follows reserving
/// its name within the same call, so this is far longer than it takes.
pub(crate) const RESERVATION_TTL: Duration = Duration::from_secs(600);
/// How often a waiting reader looks for another process's writes.
const POLL: Duration = Duration::from_millis(100);

/// The `.branchyard` directory of the repository at `root`.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

/// A branch's record: its public info and what the engine needs to continue
/// it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Record {
    pub info: BranchInfo,
    /// Milliseconds since the Unix epoch, for ordering.
    pub created_ms: u64,
    pub check: Option<Vec<String>>,
    /// Command override, reused by sends and forks.
    pub command: Option<Vec<String>>,
    /// Private `HOME` when the branch runs isolated.
    pub home: Option<PathBuf>,
    /// For a forked session, the parent's cumulative cost at the fork,
    /// which the harness keeps reporting as part of the fork's.
    pub cost_baseline: Option<f64>,
    /// Where the harness runs; `None` is local.
    #[serde(default)]
    pub provider: Option<Provider>,
    /// What the branch may delegate, and for a delegated child the limits
    /// and denials its parent imposed. `None`: no delegation.
    #[serde(default)]
    pub grant: Option<Grant>,
}

/// The right to write a branch's state for one turn: the branch's current
/// incarnation (a removed and recreated branch is a new one), the lease
/// generation, and the turn the lease was granted for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fence {
    pub branch: String,
    pub incarnation: i64,
    pub generation: u64,
    /// The engine call the lease was granted for; the key of its journaled
    /// steps. Equal to the generation it was granted with; a recovery that
    /// takes the lease over keeps the turn and bumps the generation.
    pub turn: u64,
}

/// An engine instance that can hold leases: one per opened [`Store`].
#[derive(Clone, Debug)]
pub(crate) struct Owner {
    pub id: String,
    /// Host and boot, from [`proc::host`].
    pub host: String,
    pub pid: u32,
    /// The process's start time, from [`proc::start_time`].
    pub start: String,
}

impl Owner {
    fn new() -> Owner {
        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        Owner {
            id: format!(
                "{pid}-{:x}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
                N.fetch_add(1, Ordering::Relaxed)
            ),
            host: proc::host().to_owned(),
            pid,
            start: proc::own_start().to_owned(),
        }
    }
}

/// A branch's lease as stored.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LeaseRow {
    pub branch: String,
    pub incarnation: i64,
    pub generation: u64,
    pub turn: u64,
    /// `None` once released.
    pub owner: Option<String>,
    pub host: String,
    pub pid: u32,
    pub start: String,
    pub expires_ms: u64,
    /// The turn's `max_duration` deadline, when it has one.
    pub deadline_ms: Option<u64>,
}

/// Whether the process `pid` that started at `start` on `host` is known
/// to be gone: it ran on this host and boot, and is not running now.
pub(crate) fn gone(host: &str, pid: u32, start: &str) -> bool {
    host == proc::host() && !proc::alive(pid, start)
}

impl LeaseRow {
    /// Why this held lease no longer protects a live engine, if it does
    /// not: its owner is gone from this host, or it expired.
    pub fn stale(&self, now_ms: u64) -> Option<String> {
        self.owner.as_ref()?;
        if gone(&self.host, self.pid, &self.start) {
            return Some(format!(
                "its engine (pid {}) is no longer running",
                self.pid
            ));
        }
        if self.expires_ms <= now_ms {
            return Some(format!(
                "its engine's lease expired {}s ago",
                (now_ms - self.expires_ms) / 1000
            ));
        }
        None
    }
}

/// A name reserved for a branch not yet created, and the engine that
/// reserved it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReservationRow {
    pub name: String,
    /// The reserving [`Owner`]'s ID.
    pub owner: String,
    pub host: String,
    pub pid: u32,
    pub start: String,
    pub reserved_ms: u64,
}

impl ReservationRow {
    /// Why this reservation can be reclaimed, if it can: the engine that
    /// made it is gone from this host, or it is older than
    /// [`RESERVATION_TTL`].
    pub fn stale(&self, now_ms: u64) -> Option<String> {
        if gone(&self.host, self.pid, &self.start) {
            return Some(format!(
                "the engine that reserved it (pid {}) is no longer running",
                self.pid
            ));
        }
        let age = now_ms.saturating_sub(self.reserved_ms);
        (age >= RESERVATION_TTL.as_millis() as u64)
            .then(|| format!("it was reserved {}s ago and never created", age / 1000))
    }
}

/// What [`Backend::acquire`] found.
#[derive(Debug)]
pub(crate) enum Acquired {
    Granted(Fence),
    /// Another owner holds the lease.
    Held(LeaseRow),
}

/// A journaled step as [`Backend::begin_step`] found it.
#[derive(Debug, PartialEq)]
pub(crate) enum Begun {
    /// The intent is now recorded; carry out the effect.
    Fresh,
    /// An earlier attempt recorded this intent and no outcome: the effect
    /// may or may not have happened.
    Pending(Value),
    /// The step already completed with this outcome.
    Done(Value),
}

/// One journaled step.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StepRow {
    pub step: String,
    pub intent: Value,
    pub outcome: Option<Value>,
}

/// A harness process started for a turn.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProcessRow {
    pub pid: u32,
    pub pgid: u32,
    pub start: String,
    pub host: String,
}

/// One event in the repository-wide feed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FeedRow {
    pub id: u64,
    pub branch: String,
    pub event: RecordedEvent,
}

/// Durable state for one repository. Every method is atomic. Methods that
/// take a [`Fence`] fail with [`Error::Fenced`] unless the branch's lease
/// still has the fence's incarnation and generation, checked in the same
/// transaction as the write.
pub(crate) trait Backend: Send + Sync + fmt::Debug {
    /// Reserve `name` for `owner`; false if it is already taken.
    fn reserve(&self, name: &str, owner: &Owner) -> Result<bool, Error>;
    /// Give up a reservation that has no record yet.
    fn release(&self, name: &str) -> Result<(), Error>;
    /// Every reservation whose branch has not been created, with the engine
    /// that made it. A reservation made by an earlier version names no
    /// engine and is not listed.
    fn reservations(&self) -> Result<Vec<ReservationRow>, Error>;
    /// Free the name `row` reserved, unless it changed since it was read:
    /// created, released, or reserved again. True if it was freed.
    fn reclaim(&self, row: &ReservationRow) -> Result<bool, Error>;
    fn taken(&self, name: &str) -> Result<bool, Error>;
    /// The created branch's record; `None` for a reservation or no branch.
    fn read(&self, name: &str) -> Result<Option<Record>, Error>;
    /// Every created branch, oldest first.
    fn list(&self) -> Result<Vec<Record>, Error>;
    /// Replace the record, keeping the stored `children`.
    fn write(&self, record: &Record, fence: Option<&Fence>) -> Result<(), Error>;
    fn add_child(&self, parent: &str, child: &str) -> Result<(), Error>;
    /// Delete the branch's record, lease, steps, processes and cancels.
    /// Its events stay in the feed.
    fn delete(&self, name: &str) -> Result<(), Error>;

    /// Write `record` and take the branch's lease for a new turn, unless a
    /// lease is held. Creating a reserved branch requires that `owner` made
    /// the reservation; one that was reclaimed and reserved again by
    /// another engine is refused with [`Error::BranchExists`].
    fn acquire(&self, record: &Record, owner: &Owner, ttl: Duration) -> Result<Acquired, Error>;
    fn renew(&self, fence: &Fence, ttl: Duration) -> Result<(), Error>;
    /// Write `record` and append `event`, if given, then release the lease:
    /// atomically.
    fn finish(
        &self,
        fence: &Fence,
        record: Option<&Record>,
        event: Option<&RecordedEvent>,
    ) -> Result<(), Error>;
    /// Every held lease.
    fn leases(&self) -> Result<Vec<LeaseRow>, Error>;
    /// Take over `lease`, as observed, for recovery: a new generation for
    /// the same turn. `None` if it changed meanwhile.
    fn take_over(
        &self,
        lease: &LeaseRow,
        owner: &Owner,
        ttl: Duration,
    ) -> Result<Option<Fence>, Error>;
    fn set_deadline(&self, fence: &Fence, deadline_ms: Option<u64>) -> Result<(), Error>;

    /// Record the intent of step `(branch, turn, step)` unless it exists.
    fn begin_step(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        intent: &Value,
    ) -> Result<Begun, Error>;
    fn finish_step(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        outcome: &Value,
    ) -> Result<(), Error>;
    /// Forget a step whose effect failed without effect, so it can run
    /// again.
    fn abandon_step(&self, fence: &Fence, turn: u64, step: &str) -> Result<(), Error>;
    /// The steps of one turn of the branch's current incarnation.
    fn steps(&self, name: &str, turn: u64) -> Result<Vec<StepRow>, Error>;

    fn record_process(&self, fence: &Fence, process: &ProcessRow) -> Result<(), Error>;
    /// The processes recorded for one turn.
    fn processes(&self, name: &str, turn: u64) -> Result<Vec<ProcessRow>, Error>;

    /// Ask the branch's running turn to stop; false when no turn holds its
    /// lease. The first request for a turn is kept.
    fn request_cancel(&self, name: &str, by: &str, subtree: bool) -> Result<bool, Error>;
    /// Who asked to cancel the fenced turn, if anyone did.
    fn cancel_requested(&self, fence: &Fence) -> Result<Option<String>, Error>;

    /// Append an event to the branch's log; returns its sequence number in
    /// the branch, counting from 1.
    fn append(
        &self,
        name: &str,
        event: &RecordedEvent,
        fence: Option<&Fence>,
    ) -> Result<u64, Error>;
    /// Up to `limit` events of the branch with sequence numbers after
    /// `after`.
    fn events_since(
        &self,
        name: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<(u64, RecordedEvent)>, Error>;
    /// How many events the branch has.
    fn event_count(&self, name: &str) -> Result<u64, Error>;
    /// Up to `limit` events of every branch with feed positions after
    /// `after`. Positions only grow, in commit order.
    fn feed_since(&self, after: u64, limit: usize) -> Result<Vec<FeedRow>, Error>;
    /// The last feed position; 0 when empty.
    fn head(&self) -> Result<u64, Error>;
}

/// Wakes readers in this process when events are appended to a store.
#[derive(Debug, Default)]
struct Signal {
    appended: Mutex<u64>,
    changed: Condvar,
}

fn signal_for(path: &Path) -> Arc<Signal> {
    static SIGNALS: OnceLock<Mutex<HashMap<PathBuf, Arc<Signal>>>> = OnceLock::new();
    let mut signals = SIGNALS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    signals.entry(path.to_path_buf()).or_default().clone()
}

/// The engine's handle on durable state. Cheap to clone; clones share the
/// backend and the owner identity.
#[derive(Clone, Debug)]
pub(crate) struct Store {
    dir: PathBuf,
    backend: Arc<dyn Backend>,
    owner: Arc<Owner>,
    signal: Arc<Signal>,
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Store {
    /// Open the repository's store, creating `.branchyard/` and importing
    /// branch records and event logs left by earlier versions.
    pub fn open(root: &Path) -> Result<Store, Error> {
        let dir = dir(root);
        let worktrees = dir.join("worktrees");
        std::fs::create_dir_all(&worktrees)
            .map_err(|e| Error::State(format!("create {}: {e}", worktrees.display())))?;
        let backend = crate::sqlite::Sqlite::open(&dir)?;
        let signal = signal_for(backend.path());
        Ok(Store {
            dir,
            backend: Arc::new(backend),
            owner: Arc::new(Owner::new()),
            signal,
        })
    }

    /// The `.branchyard` directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn owner(&self) -> &Owner {
        &self.owner
    }

    pub fn backend(&self) -> &dyn Backend {
        self.backend.as_ref()
    }

    pub fn worktree(&self, name: &str) -> PathBuf {
        self.dir.join("worktrees").join(name)
    }

    pub fn home(&self, name: &str) -> PathBuf {
        self.dir.join("homes").join(name)
    }

    /// Where the engine running `name` writes its delegation token and the
    /// address of its broker, while a turn runs.
    pub fn token_path(&self, name: &str) -> PathBuf {
        self.dir.join("delegation").join(format!("{name}.json"))
    }

    /// Whether a record or reservation exists for `name`. An unreadable
    /// store counts as taken.
    pub fn taken(&self, name: &str) -> bool {
        self.backend.taken(name).unwrap_or(true)
    }

    /// Reserve `name`; false if it is already taken.
    pub fn reserve(&self, name: &str) -> Result<bool, Error> {
        self.backend.reserve(name, &self.owner)
    }

    /// Give up a reservation made by [`Store::reserve`].
    pub fn release(&self, name: &str) {
        let _ = self.backend.release(name);
    }

    /// Replace the record outside any turn. The `children` already stored
    /// are kept.
    pub fn write(&self, record: &Record) -> Result<(), Error> {
        self.backend.write(record, None)
    }

    /// Replace the record for the fenced turn.
    pub fn write_fenced(&self, record: &Record, fence: &Fence) -> Result<(), Error> {
        self.backend.write(record, Some(fence))
    }

    /// Append `child` to `parent`'s children.
    pub fn add_child(&self, parent: &str, child: &str) -> Result<(), Error> {
        self.backend.add_child(parent, child)
    }

    pub fn read(&self, name: &str) -> Result<Record, Error> {
        if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
            return Err(Error::UnknownBranch(name.to_owned()));
        }
        self.backend
            .read(name)?
            .ok_or_else(|| Error::UnknownBranch(name.to_owned()))
    }

    /// Every created branch, oldest first.
    pub fn list(&self) -> Result<Vec<Record>, Error> {
        self.backend.list()
    }

    /// Delete a branch's record and what the engine kept for its turns.
    pub fn delete(&self, name: &str) -> Result<(), Error> {
        self.backend.delete(name)
    }

    /// Write `record` and take its branch's lease for a new turn. Refused
    /// with [`Error::Running`] while a live engine holds it; a lease left by
    /// a stopped engine is reported as [`Taken::Stale`] for recovery first.
    pub fn acquire(&self, record: &Record) -> Result<Taken, Error> {
        match self.backend.acquire(record, &self.owner, LEASE_TTL)? {
            Acquired::Granted(fence) => Ok(Taken::Granted(Lease::new(self.clone(), fence))),
            Acquired::Held(row) => match row.stale(now_ms()) {
                Some(_) => Ok(Taken::Stale),
                None => Err(Error::Running(record.info.name.clone())),
            },
        }
    }

    /// Ask the branch's running turn to stop, on behalf of `by`. False when
    /// no turn is running.
    pub fn request_cancel(&self, name: &str, by: &str, subtree: bool) -> Result<bool, Error> {
        self.backend.request_cancel(name, by, subtree)
    }

    /// Append an event; wakes waiting readers.
    pub fn append(
        &self,
        name: &str,
        event: &RecordedEvent,
        fence: Option<&Fence>,
    ) -> Result<u64, Error> {
        let seq = self.backend.append(name, event, fence)?;
        self.notify();
        Ok(seq)
    }

    /// Wake readers waiting in this process.
    pub fn notify(&self) {
        let mut appended = self
            .signal
            .appended
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *appended += 1;
        self.signal.changed.notify_all();
    }

    /// Wait until `ready` returns something or `timeout` passes. Appends in
    /// this process wake the wait at once; another process's are seen
    /// within [`POLL`].
    pub fn wait<T>(
        &self,
        timeout: Duration,
        mut ready: impl FnMut() -> Result<Option<T>, Error>,
    ) -> Result<Option<T>, Error> {
        let deadline = Instant::now() + timeout;
        loop {
            let seen = *self
                .signal
                .appended
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(found) = ready()? {
                return Ok(Some(found));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let appended = self
                .signal
                .appended
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if *appended == seen {
                let _ = self
                    .signal
                    .changed
                    .wait_timeout(appended, POLL.min(deadline - now));
            }
        }
    }
}

/// What [`Store::acquire`] got.
pub(crate) enum Taken {
    Granted(Lease),
    /// Held by an engine that stopped; recover the branch, then retry.
    Stale,
}

/// A held lease, renewed by a [`Heartbeat`] while held. Released when
/// dropped unless finished first, so a turn that never ran does not keep
/// its branch.
pub(crate) struct Lease {
    store: Store,
    fence: Fence,
    heartbeat: Option<Heartbeat>,
    done: bool,
}

impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease").field("fence", &self.fence).finish()
    }
}

impl Lease {
    pub fn new(store: Store, fence: Fence) -> Lease {
        let heartbeat = Heartbeat::start(&store, &fence);
        Lease {
            store,
            fence,
            heartbeat: Some(heartbeat),
            done: false,
        }
    }

    pub fn fence(&self) -> &Fence {
        &self.fence
    }

    /// Whether a renewal was refused because another engine took the
    /// lease.
    pub fn lost(&self) -> bool {
        self.heartbeat.as_ref().is_some_and(Heartbeat::lost)
    }

    /// Write the final record and event and release the lease,
    /// atomically; wakes waiting readers.
    pub fn finish(
        mut self,
        record: Option<&Record>,
        event: Option<&RecordedEvent>,
    ) -> Result<(), Error> {
        self.done = true;
        self.heartbeat.take();
        self.store.backend.finish(&self.fence, record, event)?;
        if event.is_some() {
            self.store.notify();
        }
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.heartbeat.take();
        if !self.done {
            let _ = self.store.backend.finish(&self.fence, None, None);
        }
    }
}

/// Renews a lease every [`HEARTBEAT`] until dropped. `lost` turns true
/// once a renewal is refused because the lease moved on.
pub(crate) struct Heartbeat {
    stop: Arc<(Mutex<bool>, Condvar)>,
    lost: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Heartbeat {
    pub fn start(store: &Store, fence: &Fence) -> Heartbeat {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (store, fence) = (store.clone(), fence.clone());
        let (stopping, losing) = (stop.clone(), lost.clone());
        let thread = std::thread::Builder::new()
            .name(format!("by-lease-{}", fence.branch))
            .spawn(move || {
                let (flag, wake) = &*stopping;
                let mut stopped = flag.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    let deadline = Instant::now() + HEARTBEAT;
                    while !*stopped && Instant::now() < deadline {
                        let left = deadline.saturating_duration_since(Instant::now());
                        stopped = wake
                            .wait_timeout(stopped, left)
                            .unwrap_or_else(|e| e.into_inner())
                            .0;
                    }
                    if *stopped {
                        return;
                    }
                    match store.backend.renew(&fence, LEASE_TTL) {
                        Ok(()) => {}
                        Err(Error::Fenced(_)) => {
                            losing.store(true, Ordering::Release);
                            return;
                        }
                        // A busy or unreadable store: try again next beat,
                        // while the lease still has time.
                        Err(_) => {}
                    }
                }
            })
            .ok();
        Heartbeat { stop, lost, thread }
    }

    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        let (flag, wake) = &*self.stop;
        *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
