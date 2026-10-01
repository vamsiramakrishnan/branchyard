//! Warm pools: worktrees made ready before a branch asks for one, so a
//! new branch starts without waiting for its worktree, or for its prepared
//! environment to be restored. See `docs/pools.md`.
//!
//! - **Slots.** A pool (`[workspace.pool]`, [`PoolSpec`]) keeps `size`
//!   ready **slots**: detached worktrees under `.branchyard/pool/<id>`, at
//!   the pool's base commit, with the current prepared environment
//!   restored into them as a branch's setup would have restored it
//!   (cloned or copied, `share` directories linked). A pool is identified
//!   by its [`recipe`]: what setup builds and how it is placed. Changing
//!   setup makes a new pool; the old one's slots are discarded.
//! - **Records.** Each slot is a row in the store ([`PoolBackend`]): the
//!   repository's SQLite file, or the server's PostgreSQL database. A row
//!   is written (`filling`) before its worktree exists, made `ready` once
//!   it is, and changed only by compare-and-set, so a slot is claimed once:
//!   two branches asking at once never get the same one, in one process
//!   or several. A claimed slot is never ready again.
//! - **Claims.** A new top-level branch on this host whose workspace has a
//!   pool takes the oldest ready slot at its base, or behind it by at most
//!   `max_behind` commits that change no environment input, checks that
//!   the slot's worktree is clean and where its row says, then moves the
//!   worktree to the branch's path (`git worktree move`, a rename) and
//!   creates the branch there (`git switch -c`), which brings a slot that
//!   is behind forward. Its setup then finds the environment already in
//!   place and restores nothing. A slot that is stale (too old, its
//!   environment gone, its base moved beyond policy) is never claimed: it
//!   is marked for removal and the next is tried; with none, the branch is
//!   created as before. Claims never fill: refilling is the keeper's.
//! - **Filling.** [`fill`] (`by env pool fill`, or a [`PoolKeeper`] in `by
//!   serve` and `by worker`) discards stale slots and makes new ones until
//!   `size` are ready, one process at a time (an advisory lock). A slot's
//!   environment comes from the key's prepared environment, or is built in
//!   the slot when the key has none (setup runs there, as `by env rebuild`
//!   would). A key whose build failed is not retried by a pool.
//! - **Crash safety.** Rows name the process filling or claiming them. A
//!   row whose process is gone from this host (or that was abandoned) is
//!   reclaimed: its worktree removed if it is still in the pool, its row
//!   deleted. A directory in `.branchyard/pool/` with no row is an orphan
//!   and is removed. Recovery ([`crate::Yard::recover`]) does this, as
//!   does every fill and drain.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use branchyard_workspace::Git;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::environments as envs;
use crate::state::{now_ms, Record, SlotRow, SlotState};
use crate::workspace::WorkspaceSpec;
use crate::{proc, Error, Yard};

/// How long a ready slot is kept unless the pool says otherwise: a day.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// How many commits a slot may be behind its pool's base and still be
/// claimed, unless the pool says otherwise.
pub const DEFAULT_MAX_BEHIND: u32 = 20;
/// The largest pool.
pub const MAX_SIZE: u32 = 32;
/// How often a keeper looks again when no claim wakes it.
pub const KEEP_EVERY: Duration = Duration::from_secs(30);

/// A warm pool: `[workspace.pool]`, stored with a branch's
/// [`WorkspaceSpec`]. See `docs/pools.md`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSpec {
    /// Ready worktrees kept, at most [`MAX_SIZE`]. Zero keeps none (slots
    /// already made are still claimed).
    pub size: u32,
    /// Labels a server's or worker's process must all carry to keep this
    /// pool filled. A local `by env pool fill` ignores them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// How long a ready slot is kept, in seconds; default a day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_secs: Option<u64>,
    /// How many commits a slot may be behind the base a branch asks for
    /// and still be claimed (and brought forward); default 20. A commit
    /// that changes an environment input makes the slot stale whatever
    /// this says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_behind: Option<u32>,
    /// The revision slots are made at; default `HEAD` of the checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

impl PoolSpec {
    pub fn max_age(&self) -> Duration {
        self.max_age_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_MAX_AGE)
    }

    pub fn max_behind(&self) -> u32 {
        self.max_behind.unwrap_or(DEFAULT_MAX_BEHIND)
    }

    /// Whether a process carrying `labels` keeps this pool filled.
    pub fn kept_by(&self, labels: &[String]) -> bool {
        self.labels.iter().all(|l| labels.contains(l))
    }
}

/// Where a slot is in its life.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolSlotState {
    /// Being made.
    Filling,
    /// Ready and unclaimed.
    Ready,
    /// Taken for a branch, or for removal.
    Claimed,
}

/// One slot, as [`crate::Yard::pool_status`] reports it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSlot {
    pub id: String,
    pub state: PoolSlotState,
    /// The commit its worktree is at.
    pub base: String,
    /// The prepared environment restored into it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// How it was restored: `clone`, `copy` or `link`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    pub path: PathBuf,
    /// The branch it was claimed for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The process filling or claiming it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Milliseconds since the Unix epoch.
    pub created_ms: u64,
    /// When it last changed state (became ready, was claimed).
    pub changed_ms: u64,
    /// How long making it took, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_ms: Option<u64>,
}

/// How a new branch's worktree related to its pool: on its setup's
/// `workspace` event ([`crate::WorkspaceReport::pool`]).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolUse {
    /// The slot whose worktree the branch took (a hit); `None` when it
    /// took none (a miss).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    /// Why no slot was taken, or how the one taken was brought forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When the branch was asked for (its record written), in
    /// milliseconds since the Unix epoch: its start latency runs from here
    /// to its first prompt.
    pub requested_ms: u64,
    /// How long taking the slot, or creating the worktree without one,
    /// took, in milliseconds.
    pub worktree_ms: u64,
}

/// What a branch took from its pool, stored with its workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PoolClaim {
    #[serde(rename = "use")]
    pub used: PoolUse,
    /// The environment the slot holds, as the slot was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub produced: Vec<String>,
}

/// What a slot row's `detail` holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Detail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub produced: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_ms: Option<u64>,
    /// For a claim: the branch's worktree it is being moved to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// For a slot taken to be removed: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Detail {
    fn of(row: &SlotRow) -> Detail {
        serde_json::from_str(&row.detail).unwrap_or_default()
    }

    fn text(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

/// A pool as [`crate::Yard::pool_status`] reports it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolStatus {
    /// What identifies the pool: [`recipe`].
    pub recipe: String,
    pub size: u32,
    /// The commit slots are made at now, when the base resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// This pool's slots on this host, oldest first.
    pub slots: Vec<PoolSlot>,
    /// Slots of other pools (an earlier setup) still here, to be
    /// discarded by the next fill.
    pub other: usize,
}

impl PoolStatus {
    pub fn ready(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.state == PoolSlotState::Ready)
            .count()
    }
}

/// What [`crate::Yard::fill_pool`] did.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolFill {
    /// Slots made, each with how long it took.
    pub made: Vec<PoolSlot>,
    /// Slots discarded as stale, each with why.
    pub discarded: Vec<(String, String)>,
    /// Slots and directories a stopped process left, removed, each with
    /// why.
    pub reclaimed: Vec<(String, String)>,
    /// Ready slots of the pool after the fill.
    pub ready: usize,
    pub size: u32,
    /// Why the fill stopped short, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Why nothing was done: another process is filling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// What [`crate::Yard::drain_pool`] did.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolDrained {
    /// Slots removed, each with why.
    pub removed: Vec<(String, String)>,
    /// Slots left (another process is making or claiming them), each with
    /// why.
    pub kept: Vec<(String, String)>,
}

/// `.branchyard/pool` of the repository at `root`.
pub(crate) fn dir(root: &Path) -> PathBuf {
    crate::state::dir(root).join("pool")
}

/// What identifies `spec`'s pool: a hash of what its setup builds and how
/// a restore places it. Slots of one recipe stand in for each other.
pub fn recipe(spec: &WorkspaceSpec) -> String {
    let inputs: Vec<String> = match spec.inputs.is_empty() {
        true => envs::DEFAULT_INPUTS.iter().map(|p| p.to_string()).collect(),
        false => spec.inputs.clone(),
    };
    let text = json!({
        "version": 1,
        "setup": spec.setup,
        "copy": spec.copy,
        "prepare": spec.prepare,
        "inputs": inputs,
        "share": spec.share,
    })
    .to_string();
    blake3::hash(text.as_bytes()).to_hex()[..24].to_owned()
}

/// This checkout on this host, as slot rows name it.
fn place(root: &Path) -> String {
    let host = proc::host();
    let name = host.split('/').next().unwrap_or(host);
    format!("{name}:{}", root.display())
}

fn nonce() -> String {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{:x}{:x}{}",
        now_ms(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Whether the process a row names is gone: it was abandoned (no host),
/// ran in an earlier boot of this host (rows are per host), or is not
/// running now.
fn gone(row: &SlotRow) -> bool {
    row.host.is_empty() || row.host != proc::host() || !proc::alive(row.pid, &row.start)
}

fn info(row: &SlotRow) -> PoolSlot {
    let detail = Detail::of(row);
    PoolSlot {
        id: row.id.clone(),
        state: match row.state {
            SlotState::Filling => PoolSlotState::Filling,
            SlotState::Ready => PoolSlotState::Ready,
            SlotState::Claimed => PoolSlotState::Claimed,
        },
        base: row.base.clone(),
        environment: detail.key,
        method: detail.method,
        path: PathBuf::from(&row.path),
        branch: row.branch.clone(),
        pid: (row.state != SlotState::Ready && !row.host.is_empty()).then_some(row.pid),
        created_ms: row.created_ms,
        changed_ms: row.changed_ms,
        fill_ms: detail.fill_ms,
    }
}

/// Wakes a keeper in this process when a slot is claimed (or a claim
/// found none), so it refills at once.
#[derive(Default)]
struct Signal {
    claims: Mutex<u64>,
    changed: Condvar,
}

fn signal(root: &Path) -> Arc<Signal> {
    static SIGNALS: OnceLock<Mutex<HashMap<PathBuf, Arc<Signal>>>> = OnceLock::new();
    SIGNALS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(root.to_path_buf())
        .or_default()
        .clone()
}

fn wake(root: &Path) {
    let signal = signal(root);
    *signal.claims.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    signal.changed.notify_all();
}

/// An advisory lock on filling the pools of a checkout: one filler at a
/// time. Released when dropped, or when its process dies.
struct FillLock {
    _file: fs::File,
}

impl FillLock {
    fn try_take(root: &Path) -> Result<Option<FillLock>, String> {
        use rustix::fs::{flock, FlockOperation};
        fs::create_dir_all(dir(root)).map_err(|e| e.to_string())?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir(root).join(".fill.lock"))
            .map_err(|e| format!("could not open the pool's lock: {e}"))?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(FillLock { _file: file })),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(e) => Err(format!("could not lock the pool: {e}")),
        }
    }

    /// Wait for the lock, at most as long as setup may run.
    fn take(root: &Path) -> Result<FillLock, String> {
        let deadline = Instant::now() + crate::workspace::SETUP_TIMEOUT;
        loop {
            if let Some(lock) = Self::try_take(root)? {
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                return Err("another process was still filling the pool".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Remove a slot's worktree: its links first (never what they point at),
/// then the worktree as git knows it, then whatever is left.
fn remove_files(root: &Path, path: &Path, shared: &[String]) {
    if !path.starts_with(dir(root)) {
        return;
    }
    branchyard_workspace::materialize::remove_links(path, shared);
    if path.exists() {
        let _lock = crate::git::lock();
        let _ = Git::new(root)
            .args(["worktree", "remove", "--force"])
            .arg(path)
            .run();
    }
    let _ = fs::remove_dir_all(path);
}

/// Forget worktrees git still lists whose directories are gone (a slot
/// removed without git).
fn prune_worktrees(root: &Path) {
    let _lock = crate::git::lock();
    let _ = Git::new(root).args(["worktree", "prune"]).run();
}

/// Take a ready slot to remove it: compare-and-set to `claimed` by nobody,
/// so whoever reclaims next removes it if this process does not.
fn take_for_removal(yard: &Yard, row: &SlotRow, why: &str) -> bool {
    let mut detail = Detail::of(row);
    detail.reason = Some(why.to_owned());
    let taken = SlotRow {
        state: SlotState::Claimed,
        detail: detail.text(),
        host: String::new(),
        pid: 0,
        start: String::new(),
        branch: None,
        changed_ms: now_ms(),
        ..row.clone()
    };
    yard.store()
        .pool()
        .update_slot(&taken, SlotState::Ready)
        .unwrap_or(false)
}

/// Remove a slot this process holds (filling, or taken for removal): its
/// files, then its row.
fn discard(yard: &Yard, row: &SlotRow) {
    remove_files(&yard.root, Path::new(&row.path), &Detail::of(row).shared);
    let _ = yard.store().pool().delete_slot(&row.id);
}

/// Remove what stopped processes left: rows filling or claiming whose
/// process is gone, ready rows whose worktree is gone, and directories in
/// `.branchyard/pool/` with no row. Each with why.
pub(crate) fn reclaim(yard: &Yard) -> Vec<(String, String)> {
    let root = &yard.root;
    let mut done = Vec::new();
    // Directories first, rows second: a directory a filler makes after
    // this listing is not in it, and its row was written before it.
    let listed: Vec<String> = fs::read_dir(dir(root))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    let store = yard.store();
    let Ok(rows) = store.pool().slots(Some(&place(root))) else {
        return done;
    };
    let mut pruned = false;
    for row in &rows {
        let path = Path::new(&row.path);
        let detail = Detail::of(row);
        match row.state {
            SlotState::Ready => {
                if !path.is_dir() && take_for_removal(yard, row, "its worktree is gone") {
                    let _ = store.pool().delete_slot(&row.id);
                    pruned = true;
                    done.push((row.id.clone(), "its worktree is gone".into()));
                }
            }
            SlotState::Filling | SlotState::Claimed if gone(row) => {
                let why = match (row.state, &row.branch, &detail.reason) {
                    (SlotState::Filling, _, _) => "the process making it stopped".to_owned(),
                    (_, Some(branch), _) => {
                        format!("the process claiming it for {branch} stopped")
                    }
                    (_, None, Some(reason)) => reason.clone(),
                    (_, None, None) => "taken to be removed".to_owned(),
                };
                if path.exists() {
                    remove_files(root, path, &detail.shared);
                } else if let (Some(target), Some(branch)) = (&detail.target, &row.branch) {
                    // Moved, and stopped before the branch was made there:
                    // the half-made worktree is no branch's.
                    let target = Path::new(target);
                    let on = crate::git::current_branch(target).ok().flatten();
                    if target.is_dir() && on.as_deref() != Some(&format!("by/{branch}")) {
                        branchyard_workspace::materialize::remove_links(target, &detail.shared);
                        let _lock = crate::git::lock();
                        let _ = Git::new(root)
                            .args(["worktree", "remove", "--force"])
                            .arg(target)
                            .run();
                    }
                }
                if store.pool().delete_slot(&row.id).unwrap_or(false) {
                    pruned = true;
                    done.push((row.id.clone(), why));
                }
            }
            _ => {}
        }
    }
    for name in listed {
        if !rows.iter().any(|r| r.id == name) {
            remove_files(root, &dir(root).join(&name), &[]);
            pruned = true;
            done.push((name, "a directory with no record".into()));
        }
    }
    if pruned {
        prune_worktrees(root);
    }
    done
}

/// Environments slots link into, by key: never pruned while they do.
pub(crate) fn linking(yard: &Yard) -> BTreeMap<String, Vec<String>> {
    let mut using: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in yard
        .store()
        .pool()
        .slots(Some(&place(&yard.root)))
        .unwrap_or_default()
    {
        let detail = Detail::of(&row);
        if let (Some(key), false) = (detail.key, detail.shared.is_empty()) {
            using
                .entry(key)
                .or_default()
                .push(format!("pool slot {}", row.id));
        }
    }
    using
}

/// The commit `rev` names, in the checkout at `root`.
fn resolve(root: &Path, rev: &str) -> Option<String> {
    crate::git::commit(root, rev).ok().flatten()
}

/// Why a slot at `slot` cannot stand in for a branch at `base`, if it
/// cannot: `base` is not ahead of it, is too far ahead, or changes an
/// environment input on the way (a new key).
fn behind(
    root: &Path,
    spec: &WorkspaceSpec,
    pool: &PoolSpec,
    slot: &str,
    base: &str,
) -> Result<u32, String> {
    if slot == base {
        return Ok(0);
    }
    let ahead = crate::git::test(root, &["merge-base", "--is-ancestor", slot, base]);
    if !ahead.unwrap_or(false) {
        return Err(format!(
            "its base {} is not behind {}",
            short(slot),
            short(base)
        ));
    }
    let count: u32 = crate::git::run(root, &["rev-list", "--count", &format!("{slot}..{base}")])
        .ok()
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(u32::MAX);
    if count > pool.max_behind() {
        return Err(format!(
            "the base moved {count} commits past it (the pool allows {})",
            pool.max_behind()
        ));
    }
    if spec.prepare {
        let inputs: Vec<String> = match spec.inputs.is_empty() {
            true => envs::DEFAULT_INPUTS.iter().map(|p| p.to_string()).collect(),
            false => spec.inputs.clone(),
        };
        let mut args: Vec<String> = ["diff", "--quiet", "--no-ext-diff", slot, base, "--"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        args.extend(inputs.iter().map(|p| format!(":(glob){p}")));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        if !crate::git::test(root, &args).unwrap_or(false) {
            return Err("the commits since change its environment's inputs (a new key)".into());
        }
    }
    Ok(count)
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// Why a ready slot is stale for `spec`'s pool at `base` (the pool's base
/// now, or the base a branch asks for), if it is.
fn stale(
    yard: &Yard,
    spec: &WorkspaceSpec,
    pool: &PoolSpec,
    row: &SlotRow,
    base: Option<&str>,
    now: u64,
) -> Option<String> {
    if row.recipe != recipe(spec) {
        return Some("the workspace setup changed (a new pool)".into());
    }
    let age = Duration::from_millis(now.saturating_sub(row.changed_ms));
    if age > pool.max_age() {
        return Some(format!(
            "ready for longer than {}s (max_age)",
            pool.max_age().as_secs()
        ));
    }
    if let Some(key) = Detail::of(row).key {
        if envs::good(&yard.root, &key).is_none() {
            return Some(format!(
                "its environment {} is no longer there",
                short(&key)
            ));
        }
    }
    let base = base?;
    behind(&yard.root, spec, pool, &row.base, base).err()
}

/// What a claim took.
pub(crate) struct Taken {
    pub row: SlotRow,
    pub detail: Detail,
    /// Commits the slot was behind the branch's base.
    pub behind: u32,
}

/// Whether a branch about to be created takes from its pool: a new
/// top-level branch on this host whose workspace has a pool and has not
/// been set up.
pub(crate) fn eligible(record: &Record) -> bool {
    let pooled = record
        .workspace
        .as_ref()
        .is_some_and(|w| w.spec.pool.is_some() && !w.ready);
    pooled
        && record.provider.is_none()
        && record.info.parent.is_none()
        && record.sandbox_seed.is_none()
}

/// Take a ready slot of `record`'s pool for it at `base`: the oldest at
/// `base`, else the oldest behind it within the pool's policy. Stale slots
/// met on the way are taken for removal, never claimed. `Err` says why no
/// slot was taken.
pub(crate) fn take(yard: &Yard, record: &Record, base: &str) -> Result<Taken, String> {
    let Some(spec) = record.workspace.as_ref().map(|w| &w.spec) else {
        return Err("the branch has no workspace".into());
    };
    let Some(pool) = &spec.pool else {
        return Err("the workspace has no pool".into());
    };
    let root = &yard.root;
    let store = yard.store();
    let wanted = recipe(spec);
    let rows = store
        .pool()
        .slots(Some(&place(root)))
        .map_err(|e| format!("could not read the pool: {e}"))?;
    let mut ready: Vec<SlotRow> = rows
        .into_iter()
        .filter(|r| r.state == SlotState::Ready && r.recipe == wanted)
        .collect();
    if ready.is_empty() {
        return Err("no ready slot".into());
    }
    // At the base first; then the rest, oldest first.
    ready.sort_by_key(|r| (r.base != base, r.created_ms));
    let now = now_ms();
    let mut passed = Vec::new();
    for row in ready {
        if let Some(why) = stale(yard, spec, pool, &row, Some(base), now) {
            if take_for_removal(yard, &row, &why) {
                passed.push(why);
            }
            continue;
        }
        let behind = behind(root, spec, pool, &row.base, base).unwrap_or(0);
        let mut detail = Detail::of(&row);
        detail.target = Some(record.info.worktree.display().to_string());
        let (host, pid, start) = crate::process_identity();
        let claimed = SlotRow {
            state: SlotState::Claimed,
            branch: Some(record.info.name.clone()),
            detail: detail.text(),
            host,
            pid,
            start,
            changed_ms: now_ms(),
            ..row.clone()
        };
        match store.pool().update_slot(&claimed, SlotState::Ready) {
            Ok(true) => {}
            // Another branch took it first.
            Ok(false) => continue,
            Err(e) => return Err(format!("could not claim a slot: {e}")),
        }
        // Clean, and where its row says.
        let path = Path::new(&row.path);
        let head = resolve(path, "HEAD");
        let changed = crate::git::run(path, &["status", "--porcelain", "--untracked-files=no"])
            .map(|out| !out.trim().is_empty())
            .unwrap_or(true);
        if !path.is_dir() || head.as_deref() != Some(row.base.as_str()) || changed {
            let why = "its worktree was not clean at its base".to_owned();
            discard(yard, &claimed);
            passed.push(why);
            continue;
        }
        return Ok(Taken {
            row: claimed,
            detail,
            behind,
        });
    }
    wake(root);
    Err(match passed.first() {
        Some(why) => format!("no ready slot fits ({} passed over: {why})", passed.len()),
        None => "no ready slot".into(),
    })
}

/// The claim is done: the slot is the branch's worktree now.
pub(crate) fn settle(yard: &Yard, taken: &Taken) {
    let _ = yard.store().pool().delete_slot(&taken.row.id);
    wake(&yard.root);
}

/// The claim failed after the slot was taken: leave it to be removed.
pub(crate) fn abandon(yard: &Yard, taken: &Taken) {
    let left = SlotRow {
        host: String::new(),
        pid: 0,
        start: String::new(),
        ..taken.row.clone()
    };
    if Path::new(&taken.row.path).exists() {
        discard(yard, &left);
    } else {
        let _ = yard.store().pool().update_slot(&left, SlotState::Claimed);
        let _ = yard.store().pool().delete_slot(&taken.row.id);
    }
    wake(&yard.root);
}

/// The claim a branch's workspace records.
pub(crate) fn claim_of(taken: &Taken, used: PoolUse) -> PoolClaim {
    PoolClaim {
        used,
        key: taken.detail.key.clone(),
        method: taken.detail.method.clone(),
        shared: taken.detail.shared.clone(),
        produced: taken.detail.produced.clone(),
    }
}

/// `spec`'s pool on this host now.
pub(crate) fn status(yard: &Yard, spec: &WorkspaceSpec) -> Result<PoolStatus, Error> {
    let pool = spec.pool.clone().unwrap_or_default();
    let wanted = recipe(spec);
    let rows = yard.store().pool().slots(Some(&place(&yard.root)))?;
    let (mine, other): (Vec<SlotRow>, Vec<SlotRow>) =
        rows.into_iter().partition(|r| r.recipe == wanted);
    Ok(PoolStatus {
        recipe: wanted,
        size: pool.size,
        base: resolve(&yard.root, pool.base.as_deref().unwrap_or("HEAD")),
        slots: mine.iter().map(info).collect(),
        other: other
            .iter()
            .filter(|r| r.state != SlotState::Claimed)
            .count(),
    })
}

/// Every slot on this host, of any pool.
pub(crate) fn slots(yard: &Yard) -> Result<Vec<PoolSlot>, Error> {
    Ok(yard
        .store()
        .pool()
        .slots(Some(&place(&yard.root)))?
        .iter()
        .map(info)
        .collect())
}

/// Discard stale slots and make new ones until `spec`'s pool has `size`
/// ready, one filler per checkout at a time. Runs setup in a slot when its
/// environment key has none built (the caller decides whether `spec`'s
/// scripts may run).
pub(crate) fn fill(yard: &Yard, spec: &WorkspaceSpec) -> Result<PoolFill, Error> {
    let Some(pool) = spec.pool.clone() else {
        return Err(Error::State("[workspace] has no pool".into()));
    };
    let root = &yard.root;
    let mut report = PoolFill {
        size: pool.size.min(MAX_SIZE),
        ..PoolFill::default()
    };
    let Some(_lock) = FillLock::try_take(root).map_err(Error::State)? else {
        report.skipped = Some("another process is filling the pool".into());
        report.ready = status(yard, spec)?.ready();
        return Ok(report);
    };
    report.reclaimed = reclaim(yard);
    let base_rev = pool.base.as_deref().unwrap_or("HEAD");
    let Some(base) = resolve(root, base_rev) else {
        report.error = Some(format!("the pool's base {base_rev:?} names no commit"));
        return Ok(report);
    };
    let store = yard.store();
    let now = now_ms();
    let wanted = recipe(spec);
    let mut ready = 0;
    let mut filling = 0;
    for row in store.pool().slots(Some(&place(root)))? {
        match row.state {
            SlotState::Ready => match stale(yard, spec, &pool, &row, Some(&base), now) {
                Some(why) => {
                    if take_for_removal(yard, &row, &why) {
                        discard(yard, &row);
                        report.discarded.push((row.id.clone(), why));
                    }
                }
                None => ready += 1,
            },
            SlotState::Filling if row.recipe == wanted => filling += 1,
            _ => {}
        }
    }
    // Taken for removal by a claim: removed here, off the claim's path.
    report.reclaimed.extend(reclaim(yard));
    let mut have = ready + filling;
    while have < report.size as usize {
        let started = Instant::now();
        match make(yard, spec, &wanted, &base) {
            Ok(mut slot) => {
                slot.fill_ms = Some(started.elapsed().as_millis() as u64);
                report.made.push(slot);
                ready += 1;
                have += 1;
            }
            Err(why) => {
                report.error = Some(why);
                break;
            }
        }
    }
    report.ready = ready;
    Ok(report)
}

/// Make one slot of `recipe` at `base`.
fn make(yard: &Yard, spec: &WorkspaceSpec, recipe: &str, base: &str) -> Result<PoolSlot, String> {
    let root = &yard.root;
    let started = Instant::now();
    let id = format!("s{}", nonce());
    let path = dir(root).join(&id);
    let (host, pid, start) = crate::process_identity();
    let now = now_ms();
    let mut row = SlotRow {
        id: id.clone(),
        place: place(root),
        recipe: recipe.to_owned(),
        state: SlotState::Filling,
        base: base.to_owned(),
        path: path.display().to_string(),
        detail: "{}".into(),
        host,
        pid,
        start,
        branch: None,
        created_ms: now,
        changed_ms: now,
    };
    let store = yard.store();
    store
        .pool()
        .insert_slot(&row)
        .map_err(|e| format!("could not record a slot: {e}"))?;
    let made = (|| {
        fs::create_dir_all(dir(root)).map_err(|e| e.to_string())?;
        {
            let _lock = crate::git::lock();
            Git::new(root)
                .no_hooks()
                .args(["worktree", "add", "--quiet", "--detach"])
                .arg(&path)
                .arg(base)
                .run()
                .map_err(|e| format!("could not make a worktree: {e}"))?;
        }
        environment(yard, spec, &path)
    })();
    let mut detail = match made {
        Ok(detail) => detail,
        Err(why) => {
            discard(yard, &row);
            return Err(why);
        }
    };
    detail.fill_ms = Some(started.elapsed().as_millis() as u64);
    let filling = row.clone();
    row.state = SlotState::Ready;
    row.detail = detail.text();
    row.changed_ms = now_ms();
    match store.pool().update_slot(&row, SlotState::Filling) {
        Ok(true) => Ok(info(&row)),
        Ok(false) => {
            discard(yard, &filling);
            Err("the slot was taken away while it was made".into())
        }
        Err(e) => {
            discard(yard, &filling);
            Err(format!("could not record the slot ready: {e}"))
        }
    }
}

/// Put `spec`'s prepared environment into the slot at `path`, as a
/// branch's setup would have: restored from the key's environment, or the
/// last good one of its recipe, or built here when there is neither.
fn environment(yard: &Yard, spec: &WorkspaceSpec, path: &Path) -> Result<Detail, String> {
    let root = &yard.root;
    if !spec.prepare || spec.setup.is_empty() {
        return Ok(Detail::default());
    }
    let restore = |info: &envs::EnvironmentInfo| -> Result<Detail, String> {
        let (method, shared) = envs::restore(root, info, path, &spec.share)?;
        Ok(Detail {
            key: Some(info.key.clone()),
            method: method.map(|m| m.as_str().to_owned()),
            shared,
            produced: info.produced.clone(),
            ..Detail::default()
        })
    };
    match envs::plan(root, spec, path, &|| None)? {
        envs::Plan::Restore(info) => restore(&info),
        envs::Plan::LastGood { good, .. } => restore(&good),
        envs::Plan::Build {
            key,
            recipe,
            inputs,
            lock,
        } => {
            if let Some(failure) = envs::failed(root, &key) {
                return Err(envs::failure_reason(&key, &failure));
            }
            if yard.hub.scripts_denied() {
                return Err(crate::workspace::DENIED.into());
            }
            let built = envs::build_in(yard, spec, path, "pool", "pool", &key, &recipe, inputs)
                .map_err(|e| e.to_string())?;
            drop(lock);
            // Copied files are the branch's to copy, fresh, when it starts.
            for rel in &built.copied {
                let target = path.join(rel);
                let _ = match fs::symlink_metadata(&target).map(|m| m.is_dir()) {
                    Ok(true) => fs::remove_dir_all(&target),
                    Ok(false) => fs::remove_file(&target),
                    Err(_) => Ok(()),
                };
            }
            match built.environment {
                Some(info) => {
                    envs::prune_after_build(yard);
                    restore(&info)
                }
                None => Err(format!(
                    "{}; output:\n{}",
                    built.report.failure(),
                    built.report.output
                )),
            }
        }
    }
}

/// Remove every ready slot on this host, of any pool, and what stopped
/// processes left; slots being made or claimed by a live process stay.
pub(crate) fn drain(yard: &Yard) -> Result<PoolDrained, Error> {
    let root = &yard.root;
    let _lock = FillLock::take(root).map_err(Error::State)?;
    let mut drained = PoolDrained {
        removed: reclaim(yard),
        ..PoolDrained::default()
    };
    for row in yard.store().pool().slots(Some(&place(root)))? {
        match row.state {
            SlotState::Ready => {
                if take_for_removal(yard, &row, "drained") {
                    discard(yard, &row);
                    drained.removed.push((row.id.clone(), "drained".into()));
                }
            }
            SlotState::Filling => drained
                .kept
                .push((row.id.clone(), format!("being made by process {}", row.pid))),
            SlotState::Claimed => drained.kept.push((
                row.id.clone(),
                format!(
                    "being claimed for {} by process {}",
                    row.branch.as_deref().unwrap_or("removal"),
                    row.pid
                ),
            )),
        }
    }
    Ok(drained)
}

/// Keeps a repository's pool filled from a thread of its own: fills at
/// once, again whenever a branch in this process claims a slot (or finds
/// none), and otherwise every `every`. Stops when dropped.
pub struct PoolKeeper {
    stop: Arc<AtomicBool>,
    root: PathBuf,
    thread: Option<JoinHandle<()>>,
}

impl PoolKeeper {
    pub(crate) fn start(
        yard: Yard,
        spec: Box<dyn Fn() -> Option<WorkspaceSpec> + Send>,
        every: Duration,
        on_fill: Box<dyn Fn(&PoolFill) + Send>,
    ) -> PoolKeeper {
        let stop = Arc::new(AtomicBool::new(false));
        let root = yard.root.clone();
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("by-pool".into())
                .spawn(move || {
                    let signal = signal(&yard.root);
                    let mut seen = *signal.claims.lock().unwrap_or_else(|e| e.into_inner());
                    while !stop.load(Ordering::SeqCst) {
                        if let Some(spec) = spec().filter(|s| s.pool.is_some()) {
                            match fill(&yard, &spec) {
                                Ok(filled) => on_fill(&filled),
                                Err(e) => on_fill(&PoolFill {
                                    error: Some(e.to_string()),
                                    ..PoolFill::default()
                                }),
                            }
                        }
                        let deadline = Instant::now() + every;
                        let mut claims = signal.claims.lock().unwrap_or_else(|e| e.into_inner());
                        while *claims == seen && !stop.load(Ordering::SeqCst) {
                            let left = deadline.saturating_duration_since(Instant::now());
                            if left.is_zero() {
                                break;
                            }
                            claims = signal
                                .changed
                                .wait_timeout(claims, left)
                                .unwrap_or_else(|e| e.into_inner())
                                .0;
                        }
                        seen = *claims;
                    }
                })
                .ok()
        };
        PoolKeeper { stop, root, thread }
    }
}

impl Drop for PoolKeeper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let signal = signal(&self.root);
        // Under the lock, so the keeper is either waiting (and woken) or
        // will see the flag before it waits.
        drop(signal.claims.lock().unwrap_or_else(|e| e.into_inner()));
        signal.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pool_is_its_setup_and_how_it_is_placed() {
        let spec = WorkspaceSpec {
            setup: vec!["pnpm install".into()],
            prepare: true,
            ..WorkspaceSpec::default()
        };
        let a = recipe(&spec);
        assert_eq!(a, recipe(&spec));
        let mut sized = spec.clone();
        sized.pool = Some(PoolSpec {
            size: 4,
            ..PoolSpec::default()
        });
        assert_eq!(
            recipe(&sized),
            a,
            "the pool's own settings are not its identity"
        );
        let mut other = spec.clone();
        other.setup.push("pnpm build".into());
        assert_ne!(recipe(&other), a);
        let mut shared = spec.clone();
        shared.share = vec!["node_modules".into()];
        assert_ne!(recipe(&shared), a);
    }

    #[test]
    fn labels_choose_who_keeps_a_pool() {
        let pool = PoolSpec {
            size: 1,
            labels: vec!["linux".into()],
            ..PoolSpec::default()
        };
        assert!(pool.kept_by(&["linux".into(), "gpu".into()]));
        assert!(!pool.kept_by(&["gpu".into()]));
        assert!(PoolSpec::default().kept_by(&[]));
        assert_eq!(pool.max_age(), DEFAULT_MAX_AGE);
        assert_eq!(pool.max_behind(), DEFAULT_MAX_BEHIND);
    }
}
