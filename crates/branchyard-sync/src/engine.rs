//! The commit protocol: upload what the remote lacks, then swap the task's
//! one manifest by compare-and-swap.
//!
//! A sync of one task, in rounds:
//!
//! 1. Read the manifest and its generation.
//! 2. Import what it references that this machine lacks: packs (only when
//!    a ref's commit is missing here), chunk indexes and chunks, and
//!    segments. Every object read is checked against its name.
//! 3. Merge, ref by ref ([`plan`]): equal stays; a fast-forward either way
//!    moves the behind side; a ref one side deleted and the other did not
//!    touch is deleted; two sides that moved apart keep the remote's value
//!    and record the local one as `refs/heads/conflict/<device>/<n>`.
//!    Never last-writer-wins.
//! 4. Apply the local side of the merge (a ref checked out in a worktree
//!    is left, and reported as behind).
//! 5. If the manifest would not change, stop. Otherwise register as an
//!    active writer (so the collector does not sweep under it), upload one
//!    pack of the objects the remote's refs do not reach (`git
//!    pack-objects --revs`, the remote tips excluded), the new chunks with
//!    an index listing them, and changed segments, under the bandwidth
//!    budget with bounded concurrency.
//! 6. Swap the manifest: `put_if_absent` for a new task, else
//!    `put_if_match` on the generation read. If another writer moved
//!    first, wait (exponential backoff, full jitter) and start again at 1.
//!    A swap whose response was lost is recognized by its random commit
//!    ID on the next read.
//!
//! Objects are written before the manifest that names them, so a reader
//! (or a crash between them) never sees a manifest naming a missing
//! object; objects written by a swap that lost are unreferenced and the
//! collector reclaims them.

use branchyard_support::LockExt as _;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use branchyard::services::Clock;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Kind, Result};
use crate::git::Git;
use crate::manifest::{
    check_task_id, chunk_key, hold_key, index_key, manifest_key, pack_key, segment_key, ChunkIndex,
    Hold, IndexEntry, Manifest, PackEntry, SegmentEntry, Writer, CONTENT_PREFIXES,
};
use crate::pace::{Budget, RealSleeper, Retrier, RetryPolicy, Sleeper};
use crate::seal::{Encryption, Sealer};
use crate::source::{read_chunk, write_chunk, ChunkId, SyncSource};
use crate::stats::Stats;
use crate::store::managed::Managed;
use crate::store::{Generation, NoJournal, ObjectStore, UploadJournal};

/// How a remote is used.
#[derive(Clone, Debug)]
pub struct Settings {
    /// This machine's name in conflict branches and leases.
    pub device: String,
    /// Objects uploaded or downloaded at once.
    pub concurrency: usize,
    /// Retries of each request.
    pub retry: RetryPolicy,
    /// Manifest swaps tried before giving up for now.
    pub max_rounds: u32,
    /// How long an active writer's mark lasts without renewal.
    pub writer_ttl: Duration,
    /// How long an unreferenced object waits before it is collected:
    /// longer than any upload takes.
    pub grace: Duration,
    /// The tenant's quota in bytes.
    pub quota_bytes: Option<u64>,
    /// Keep a task this long after its last change (legal holds win).
    pub retention: Option<Duration>,
    /// Clock skew allowed between machines when reading another's lease
    /// or mark as expired.
    pub skew: Duration,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            device: default_device(),
            concurrency: 8,
            retry: RetryPolicy::default(),
            max_rounds: 12,
            writer_ttl: Duration::from_secs(15 * 60),
            grace: Duration::from_secs(24 * 3600),
            quota_bytes: None,
            retention: None,
            skew: Duration::from_secs(30),
        }
    }
}

/// This host's name, made safe for ref names.
pub fn default_device() -> String {
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "device".into());
    sanitize_device(&host)
}

/// A device name: lower-case `a-z 0-9 - _`, 1 to 40 characters.
pub fn sanitize_device(text: &str) -> String {
    let out: String = text
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' | '_' => c,
            _ => '-',
        })
        .take(40)
        .collect();
    let out = out.trim_matches('-').to_owned();
    match out.is_empty() {
        true => "device".into(),
        false => out,
    }
}

/// What this machine knows about one task in one remote, kept in the
/// outbox between syncs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskState {
    /// The refs as last agreed with the remote.
    pub base: BTreeMap<String, String>,
    /// The remote manifest's `seq` last seen.
    pub seq: u64,
    pub imported_packs: BTreeSet<String>,
    pub imported_indexes: BTreeSet<String>,
    /// Chunk IDs (hex) the remote holds for this task.
    pub remote_chunks: BTreeSet<String>,
    pub synced_ms: Option<u64>,
}

/// What one sync of a task did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    pub task: String,
    /// The remote manifest's `seq` afterwards (0: the remote has none).
    pub seq: u64,
    /// Whether this sync swapped the manifest.
    pub swapped: bool,
    pub rounds: u32,
    pub pushed: Vec<String>,
    pub pulled: Vec<String>,
    pub deleted: Vec<String>,
    /// Refs the remote moved past a ref checked out here.
    pub behind: Vec<String>,
    /// `(ref, conflict ref)` for each divergence recorded.
    pub conflicts: Vec<(String, String)>,
    pub pack: Option<PackEntry>,
    pub packs_down: u64,
    pub chunks_up: u64,
    pub chunks_down: u64,
    pub segments_up: u64,
    pub segments_down: u64,
}

/// A change to a local ref that a merge decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalUpdate {
    pub task_ref: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// A merge's outcome.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub manifest: BTreeMap<String, String>,
    pub local: Vec<LocalUpdate>,
    pub pushed: Vec<String>,
    pub pulled: Vec<String>,
    pub deleted: Vec<String>,
    pub conflicts: Vec<(String, String)>,
}

pub const CONFLICT_PREFIX: &str = "refs/heads/conflict/";

/// How often a writer reads the keyring again, at least: well under the
/// shortest grace period (15 minutes), after which a superseded key
/// version can be retired.
pub const KEYS_CHECK_MS: u64 = 60_000;

/// How long the collector's count of the tenant's bytes is used.
const USAGE_MAX_AGE_MS: u64 = 3_600_000;

/// Merge the local refs with the remote's, ref by ref, from the last
/// agreed `base`. `is_ancestor(a, b)` says whether `a` is `b` or behind it.
pub fn plan(
    base: &BTreeMap<String, String>,
    local: &BTreeMap<String, String>,
    remote: &BTreeMap<String, String>,
    device: &str,
    mut is_ancestor: impl FnMut(&str, &str) -> Result<bool>,
) -> Result<Plan> {
    let mut out = Plan::default();
    let mut diverged: Vec<(String, String)> = Vec::new();
    let names: BTreeSet<&String> = local.keys().chain(remote.keys()).collect();
    for name in names {
        let (l, r, b) = (local.get(name), remote.get(name), base.get(name));
        match (l, r) {
            (Some(l), Some(r)) if l == r => {
                out.manifest.insert(name.clone(), r.clone());
            }
            (None, Some(r)) => {
                if b == Some(r) {
                    // Deleted here since the last sync; the remote did not
                    // move it.
                    out.deleted.push(name.clone());
                } else {
                    out.manifest.insert(name.clone(), r.clone());
                    out.local.push(LocalUpdate {
                        task_ref: name.clone(),
                        old: None,
                        new: Some(r.clone()),
                    });
                    out.pulled.push(name.clone());
                }
            }
            (Some(l), None) => {
                if b == Some(l) {
                    // Deleted in the remote; not moved here.
                    out.local.push(LocalUpdate {
                        task_ref: name.clone(),
                        old: Some(l.clone()),
                        new: None,
                    });
                    out.deleted.push(name.clone());
                } else {
                    out.manifest.insert(name.clone(), l.clone());
                    out.pushed.push(name.clone());
                }
            }
            (Some(l), Some(r)) => {
                if is_ancestor(r, l)? {
                    out.manifest.insert(name.clone(), l.clone());
                    out.pushed.push(name.clone());
                } else if is_ancestor(l, r)? {
                    out.manifest.insert(name.clone(), r.clone());
                    out.local.push(LocalUpdate {
                        task_ref: name.clone(),
                        old: Some(l.clone()),
                        new: Some(r.clone()),
                    });
                    out.pulled.push(name.clone());
                } else {
                    out.manifest.insert(name.clone(), r.clone());
                    if !name.starts_with(CONFLICT_PREFIX) {
                        diverged.push((name.clone(), l.clone()));
                    }
                }
            }
            (None, None) => {}
        }
    }
    // Record each divergence once: an existing conflict branch of this
    // device at the same commit is reused.
    let mine = format!("{CONFLICT_PREFIX}{device}/");
    let mut taken: BTreeMap<u64, String> = BTreeMap::new();
    for (name, oid) in local.iter().chain(remote.iter()) {
        if let Some(n) = name.strip_prefix(&mine).and_then(|n| n.parse::<u64>().ok()) {
            taken.insert(n, oid.clone());
        }
    }
    for (name, oid) in diverged {
        if taken.values().any(|v| *v == oid) {
            continue;
        }
        let n = taken.keys().next_back().copied().unwrap_or(0) + 1;
        taken.insert(n, oid.clone());
        let conflict = format!("{mine}{n}");
        out.manifest.insert(conflict.clone(), oid.clone());
        out.local.push(LocalUpdate {
            task_ref: conflict.clone(),
            old: None,
            new: Some(oid),
        });
        out.conflicts.push((name, conflict));
    }
    Ok(out)
}

/// What a remote is opened with.
pub struct Options {
    pub encryption: Encryption,
    pub settings: Settings,
    pub clock: Clock,
    pub sleeper: Arc<dyn Sleeper>,
    /// Bytes a second, up and down together.
    pub bandwidth: Option<u64>,
    pub journal: Arc<dyn UploadJournal>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            encryption: Encryption::None,
            settings: Settings::default(),
            clock: Clock::system(),
            sleeper: Arc::new(RealSleeper),
            bandwidth: None,
            journal: Arc::new(NoJournal),
        }
    }
}

/// A sync remote: a backend, its keys, and how it is used.
pub struct Remote {
    pub(crate) store: Arc<Managed>,
    pub(crate) sealer: Mutex<Sealer>,
    /// What opens the keyring, to read it again after a rotation.
    encryption: Encryption,
    /// When the keyring was last read.
    keys_checked_ms: std::sync::atomic::AtomicU64,
    pub(crate) settings: Settings,
    pub(crate) clock: Clock,
    pub(crate) retrier: Arc<Retrier>,
    pub(crate) stats: Arc<Stats>,
    pub(crate) journal: Arc<dyn UploadJournal>,
    /// `(when, bytes)` of each object this process stored, so a quota
    /// check counts what was written since the collector's last count.
    pub(crate) uploaded: Mutex<Vec<(u64, u64)>>,
}

/// A remote's task, as listed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task: String,
    pub seq: u64,
    pub refs: BTreeMap<String, String>,
    pub updated_ms: u64,
    pub device: String,
    pub bytes: u64,
    pub held: bool,
}

/// A writer's mark: the collector does not sweep while one is live.
pub struct Mark {
    key: String,
    generation: Generation,
    renewed_ms: u64,
    expires_ms: u64,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct LockRecord {
    pub holder: String,
    pub expires_ms: u64,
}

impl Remote {
    /// Open `store` as a remote: read (or make) its keyring.
    pub fn open(store: Arc<dyn ObjectStore>, options: Options) -> Result<Remote> {
        let stats = Arc::new(Stats::default());
        let retrier = Arc::new(Retrier::new(
            options.settings.retry.clone(),
            options.sleeper.clone(),
        ));
        let budget = options.bandwidth.map(|rate| {
            Arc::new(Budget::new(
                rate,
                options.clock.clone(),
                options.sleeper.clone(),
            ))
        });
        let managed = Arc::new(Managed::new(store, retrier.clone(), budget, stats.clone()));
        let sealer =
            crate::seal::open_keyring(managed.as_ref(), &options.encryption, options.clock.now())?;
        Ok(Remote {
            keys_checked_ms: std::sync::atomic::AtomicU64::new(options.clock.now()),
            encryption: options.encryption,
            store: managed,
            sealer: Mutex::new(sealer),
            settings: options.settings,
            clock: options.clock,
            retrier,
            stats,
            journal: options.journal,
            uploaded: Mutex::new(Vec::new()),
        })
    }

    pub fn url(&self) -> String {
        self.store.url()
    }

    pub fn store(&self) -> &Arc<Managed> {
        &self.store
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn sealer(&self) -> Sealer {
        self.sealer.lock_recovering("sealer").clone()
    }

    pub(crate) fn set_sealer(&self, sealer: Sealer) {
        *self.sealer.lock_recovering("sealer") = sealer;
    }

    /// Rotate the tenant key (see [`crate::seal::rotate`]): `wrapper`
    /// opens the keyring now; `to`, when given, wraps it from now on.
    pub fn rotate_key(
        &self,
        wrapper: &dyn crate::kms::Wrapper,
        to: Option<&dyn crate::kms::Wrapper>,
    ) -> Result<crate::seal::Rotation> {
        let (sealer, report) = crate::seal::rotate(
            self.store.as_ref(),
            wrapper,
            to,
            self.clock.now(),
            self.settings.grace.as_millis() as u64,
        )?;
        self.set_sealer(sealer);
        Ok(report)
    }

    /// Read the keyring again, and take its current key version when it
    /// moved (another process rotated the key). True when it moved.
    pub fn refresh_keys(&self) -> Result<bool> {
        let file = crate::seal::read_keyring(self.store.as_ref())?;
        self.keys_checked_ms
            .store(self.clock.now(), std::sync::atomic::Ordering::SeqCst);
        if self.sealer().matches(&file) {
            return Ok(false);
        }
        self.set_sealer(crate::seal::sealer_for(&file, &self.encryption)?);
        Ok(true)
    }

    /// The sealer to write with: the keyring is read again when it was
    /// last read longer ago than [`KEYS_CHECK_MS`], so a process sealing
    /// under a version a rotation superseded moves off it well within the
    /// grace period before that version can be retired.
    pub(crate) fn write_sealer(&self) -> Result<Sealer> {
        let checked = self
            .keys_checked_ms
            .load(std::sync::atomic::Ordering::SeqCst);
        if self.clock.now() >= checked + KEYS_CHECK_MS {
            self.refresh_keys()?;
        }
        Ok(self.sealer())
    }

    /// The most requests that were in flight at once.
    pub fn max_in_flight(&self) -> u64 {
        self.store
            .max_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Read a framed object and open it.
    pub(crate) fn read(&self, key: &str) -> Result<(Vec<u8>, Generation)> {
        let object = self.store.get(key)?;
        let mut opened = self.sealer().open(key, &object.data);
        if let Err(e) = &opened {
            // Sealed under a version this process has not loaded: a
            // rotation since the keyring was read. Read it again.
            if e.is(Kind::Refused) && self.refresh_keys()? {
                opened = self.sealer().open(key, &object.data);
            }
        }
        let plain = opened.inspect_err(|e| {
            if e.is(Kind::Corrupt) {
                Stats::add(&self.stats.corrupt, 1);
            }
        })?;
        Ok((plain, object.generation))
    }

    /// Read a content-addressed object and check it against its name.
    pub(crate) fn read_content(&self, kind: &str, key: &str, name: &str) -> Result<Vec<u8>> {
        let (plain, _) = self.read(key)?;
        if self.sealer().name(kind, &blake3::hash(&plain)) != name {
            Stats::add(&self.stats.corrupt, 1);
            return Err(Error::corrupt(format!(
                "{key} does not match its name; refused"
            )));
        }
        Ok(plain)
    }

    /// Upload a content-addressed object; `key_of` makes its key from its
    /// name. An object already there is left (its name says it is the
    /// same). Returns its name, key and stored size.
    pub(crate) fn write_content(
        &self,
        kind: &str,
        key_of: fn(&str) -> String,
        data: &[u8],
        check_first: bool,
    ) -> Result<(String, String, u64)> {
        let sealer = self.write_sealer()?;
        let name = sealer.name(kind, &blake3::hash(data));
        let key = key_of(&name);
        if check_first {
            if let Some(entry) = self.store.stat(&key)? {
                return Ok((name, key, entry.size));
            }
        }
        let framed = sealer.seal(&key, data)?;
        match self
            .store
            .resumable_put(&key, &framed, self.journal.as_ref())
        {
            Ok(_) => self.note_upload(framed.len() as u64),
            Err(e) if e.is(Kind::Precondition) => {}
            Err(e) => return Err(e),
        }
        Ok((name, key, framed.len() as u64))
    }

    /// The task's manifest and its generation, or `None`.
    pub fn manifest(&self, task: &str) -> Result<Option<(Manifest, Generation)>> {
        check_task_id(task)?;
        let key = manifest_key(&self.sealer().task_dir(task));
        match self.read(&key) {
            Ok((plain, generation)) => {
                let manifest = Manifest::decode(&plain)?;
                if manifest.task != task {
                    Stats::add(&self.stats.corrupt, 1);
                    return Err(Error::corrupt(format!(
                        "{key} holds task {:?}, not {task:?}",
                        manifest.task
                    )));
                }
                Ok(Some((manifest, generation)))
            }
            Err(e) if e.is(Kind::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Every task in the remote.
    pub fn tasks(&self) -> Result<Vec<TaskSummary>> {
        let mut out = Vec::new();
        let entries = self.store.list("tasks/")?;
        let held: BTreeSet<String> = entries
            .iter()
            .filter(|e| e.key.ends_with("/hold"))
            .map(|e| e.key.trim_end_matches("/hold").to_owned())
            .collect();
        for entry in &entries {
            if !entry.key.ends_with("/manifest") {
                continue;
            }
            let (plain, _) = match self.read(&entry.key) {
                Ok(r) => r,
                Err(e) if e.is(Kind::NotFound) => continue,
                Err(e) => return Err(e),
            };
            let m = Manifest::decode(&plain)?;
            out.push(TaskSummary {
                held: held.contains(entry.key.trim_end_matches("/manifest")),
                task: m.task.clone(),
                seq: m.seq,
                updated_ms: m.writer.at_ms,
                device: m.writer.device.clone(),
                bytes: m.bytes(),
                refs: m.refs,
            });
        }
        out.sort_by(|a, b| a.task.cmp(&b.task));
        Ok(out)
    }

    fn read_lock(&self, key: &str) -> Result<Option<(LockRecord, Generation)>> {
        match self.read(key) {
            Ok((plain, g)) => Ok(Some((serde_json::from_slice(&plain)?, g))),
            Err(e) if e.is(Kind::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn lock_live(&self, record: &LockRecord) -> bool {
        record.expires_ms + self.settings.skew.as_millis() as u64 > self.clock.now()
    }

    /// Mark this writer active, then make sure no sweep is under way
    /// (else unmark, wait and try again). Together with the collector's
    /// order (take the sweep lock, then look for writers), at most one of
    /// the two proceeds.
    pub fn begin_write(&self) -> Result<Mark> {
        let sealer = self.write_sealer()?;
        let nonce = hex::encode(&crate::util::random_bytes(8)?);
        let key = format!(
            "locks/writers/{}-{nonce}",
            sealer.keyed("device", &self.settings.device)
        );
        for attempt in 1..=self.settings.max_rounds {
            let now = self.clock.now();
            let expires = now + self.settings.writer_ttl.as_millis() as u64;
            let record = LockRecord {
                holder: self.settings.device.clone(),
                expires_ms: expires,
            };
            let generation = self
                .store
                .put_if_absent(&key, &sealer.seal(&key, &serde_json::to_vec(&record)?)?)?;
            match self.read_lock("locks/sweep")? {
                Some((sweep, _)) if self.lock_live(&sweep) => {
                    branchyard_support::best_effort(
                        "self.store.delete_if_match",
                        self.store.delete_if_match(&key, &generation),
                    );
                    self.retrier.sleeper.sleep(self.retrier.backoff(attempt));
                }
                _ => {
                    return Ok(Mark {
                        key,
                        generation,
                        renewed_ms: now,
                        expires_ms: expires,
                    })
                }
            }
        }
        Err(Error::transient(
            "the collector is sweeping this remote; try again later",
        ))
    }

    /// Renew the mark when half its time is gone; false when it already
    /// ran out (the round must start again).
    pub(crate) fn keep_mark(&self, mark: &mut Mark) -> Result<bool> {
        let now = self.clock.now();
        if now >= mark.expires_ms {
            return Ok(false);
        }
        let ttl = self.settings.writer_ttl.as_millis() as u64;
        if now < mark.renewed_ms + ttl / 2 {
            return Ok(true);
        }
        let record = LockRecord {
            holder: self.settings.device.clone(),
            expires_ms: now + ttl,
        };
        let sealer = self.write_sealer()?;
        match self.store.put_if_match(
            &mark.key,
            &sealer.seal(&mark.key, &serde_json::to_vec(&record)?)?,
            &mark.generation,
        ) {
            Ok(g) => {
                mark.generation = g;
                mark.renewed_ms = now;
                mark.expires_ms = record.expires_ms;
                Ok(true)
            }
            Err(e) if e.is(Kind::Precondition) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Remove this writer's mark.
    pub fn end_write(&self, mark: Mark) {
        branchyard_support::best_effort(
            "self.store.delete_if_match",
            self.store.delete_if_match(&mark.key, &mark.generation),
        );
    }

    /// Record an object this process stored, for [`Remote::usage`].
    fn note_upload(&self, bytes: u64) {
        let now = self.clock.now();
        let mut uploaded = self.uploaded.lock_recovering("uploaded");
        // Older than any count still used: no longer needed.
        uploaded
            .retain(|(at, _)| at + USAGE_MAX_AGE_MS + self.settings.skew.as_millis() as u64 > now);
        uploaded.push((now, bytes));
    }

    /// Bytes the tenant stores: the collector's last count when it is
    /// under an hour old, plus what this process stored since (allowing
    /// for clock skew); else counted now.
    pub fn usage(&self) -> Result<u64> {
        self.estimate_usage().map(|(bytes, _)| bytes)
    }

    /// [`Remote::usage`], and whether it was counted now.
    fn estimate_usage(&self) -> Result<(u64, bool)> {
        if let Ok((plain, _)) = self.read("gc/usage") {
            if let Ok(usage) = serde_json::from_slice::<crate::gc::Usage>(&plain) {
                if usage.at_ms + USAGE_MAX_AGE_MS > self.clock.now() {
                    let since = usage
                        .at_ms
                        .saturating_sub(self.settings.skew.as_millis() as u64);
                    let ours: u64 = self
                        .uploaded
                        .lock_recovering("uploaded")
                        .iter()
                        .filter(|(at, _)| *at >= since)
                        .map(|(_, bytes)| bytes)
                        .sum();
                    return Ok((usage.bytes + ours, false));
                }
            }
        }
        Ok((self.count_usage()?, true))
    }

    /// Bytes the tenant stores, counted now by listing every content
    /// object.
    pub fn count_usage(&self) -> Result<u64> {
        let mut bytes = 0;
        for prefix in CONTENT_PREFIXES {
            bytes += self.store.list(prefix)?.iter().map(|e| e.size).sum::<u64>();
        }
        Ok(bytes)
    }

    /// Refuse a write of `new_bytes` that would take the tenant over its
    /// quota. The estimate ([`Remote::usage`]) misses what other machines
    /// stored since the collector's count, so within a tenth of the quota
    /// the bytes are counted afresh before deciding.
    fn admit(&self, new_bytes: u64) -> Result<()> {
        let Some(quota) = self.settings.quota_bytes else {
            return Ok(());
        };
        let (mut used, counted) = self.estimate_usage()?;
        if !counted && used + new_bytes > quota - quota / 10 {
            used = self.count_usage()?;
        }
        if used + new_bytes > quota {
            return Err(Error::new(
                Kind::Quota,
                format!("this sync adds {new_bytes} bytes to {used}, over the quota of {quota}"),
            ));
        }
        Ok(())
    }

    /// Run `jobs` on up to `concurrency` threads; the first error wins.
    pub(crate) fn parallel<T: Send>(
        &self,
        jobs: Vec<T>,
        work: impl Fn(T) -> Result<()> + Sync,
    ) -> Result<()> {
        let queue = Mutex::new(jobs);
        let failed: Mutex<Option<Error>> = Mutex::new(None);
        let threads = self.settings.concurrency.max(1);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| loop {
                    if failed.lock_recovering("failed").is_some() {
                        return;
                    }
                    let job = queue.lock_recovering("queue").pop();
                    let Some(job) = job else { return };
                    if let Err(e) = work(job) {
                        failed.lock_recovering("failed").get_or_insert(e);
                        return;
                    }
                });
            }
        });
        match failed.into_inner_recovering("failed") {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Bring what `manifest` references and this machine lacks.
    fn import(
        &self,
        source: &dyn SyncSource,
        git: &Git,
        manifest: &Manifest,
        state: &mut TaskState,
        report: &mut SyncReport,
    ) -> Result<()> {
        let mut missing = false;
        for oid in manifest.refs.values() {
            if !git.has(oid)? {
                missing = true;
                break;
            }
        }
        if missing {
            for pack in &manifest.packs {
                if state.imported_packs.contains(&pack.name) {
                    continue;
                }
                let data = self.read_content("pack", &pack_key(&pack.name), &pack.name)?;
                git.import_pack(&data)?;
                state.imported_packs.insert(pack.name.clone());
                report.packs_down += 1;
            }
            for (name, oid) in &manifest.refs {
                if !git.has(oid)? {
                    return Err(Error::corrupt(format!(
                        "the remote's {name} names {oid}, which none of its packs holds"
                    )));
                }
            }
        } else {
            state
                .imported_packs
                .extend(manifest.packs.iter().map(|p| p.name.clone()));
        }
        for index in &manifest.chunk_indexes {
            if state.imported_indexes.contains(&index.name) {
                continue;
            }
            let data = self.read_content("index", &index_key(&index.name), &index.name)?;
            let list: ChunkIndex = serde_json::from_slice(&data)?;
            state.remote_chunks.extend(list.chunks.keys().cloned());
            state.imported_indexes.insert(index.name.clone());
        }
        if let Some(dir) = source.chunk_dir() {
            let sealer = self.sealer();
            let wanted: Vec<ChunkId> = state
                .remote_chunks
                .iter()
                .filter_map(|hex| ChunkId::parse(hex).ok())
                .filter(|id| !crate::source::chunk_path(dir, id).exists())
                .collect();
            let count = std::sync::atomic::AtomicU64::new(0);
            self.parallel(wanted, |id| {
                let name = sealer.name("chunk", &id.hash());
                let data = self.read_content("chunk", &chunk_key(&name), &name)?;
                write_chunk(dir, &id, &data)?;
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            })?;
            report.chunks_down += count.into_inner();
        }
        if let Some(dir) = source.segment_dir() {
            for (name, segment) in &manifest.segments {
                let path = dir.join(name);
                let current = std::fs::read(&path).ok();
                let same = current.as_ref().is_some_and(|data| {
                    self.sealer().name("segment", &blake3::hash(data)) == segment.object
                });
                if same {
                    continue;
                }
                let data =
                    self.read_content("segment", &segment_key(&segment.object), &segment.object)?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, data)?;
                report.segments_down += 1;
            }
        }
        Ok(())
    }

    /// Apply a plan's local side. A ref checked out in a worktree moves
    /// only by a fast-forward of a clean worktree.
    fn apply(
        &self,
        source: &dyn SyncSource,
        git: &Git,
        plan: &Plan,
        report: &mut SyncReport,
    ) -> Result<()> {
        let checked_out = git.checked_out()?;
        for update in &plan.local {
            let Some(local) = source.local_ref(&update.task_ref) else {
                continue;
            };
            if let Some(worktree) = checked_out.get(&local) {
                // A clean worktree follows a fast-forward; one with
                // changes is left, and reported behind.
                let moved = match (&update.old, &update.new) {
                    (Some(_), Some(new)) => git.fast_forward_worktree(worktree, new)?,
                    _ => false,
                };
                if !moved && !report.behind.contains(&update.task_ref) {
                    report.behind.push(update.task_ref.clone());
                }
                continue;
            }
            match &update.new {
                Some(new) => git.update_ref(&local, new, update.old.as_deref())?,
                None => {
                    if let Some(old) = &update.old {
                        git.delete_ref(&local, old)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Pull a task: import what the remote has and fast-forward or create
    /// the local refs; nothing is uploaded and the manifest is not
    /// changed. Divergences are left for the next `sync` to record.
    pub fn pull(&self, source: &dyn SyncSource, state: &mut TaskState) -> Result<SyncReport> {
        let task = source.task_id().to_owned();
        check_task_id(&task)?;
        let git = Git::new(source.git_dir());
        let mut report = SyncReport {
            task: task.clone(),
            ..SyncReport::default()
        };
        let Some((manifest, _)) = self.manifest(&task)? else {
            return Err(Error::not_found(format!("the remote has no task {task}")));
        };
        self.import(source, &git, &manifest, state, &mut report)?;
        let local = source.refs()?;
        let mut plan = plan(
            &state.base,
            &local,
            &manifest.refs,
            &self.settings.device,
            |a, b| git.is_ancestor(a, b),
        )?;
        // A pull only moves local refs forward or creates them; recording
        // a divergence is a sync's work.
        let recorded: Vec<String> = plan.conflicts.iter().map(|(_, c)| c.clone()).collect();
        plan.local
            .retain(|u| u.new.is_some() && !recorded.contains(&u.task_ref));
        self.apply(source, &git, &plan, &mut report)?;
        report.pulled = plan
            .pulled
            .iter()
            .filter(|r| !report.behind.contains(r))
            .cloned()
            .collect();
        report.seq = manifest.seq;
        // The refs both sides now agree on.
        for (name, oid) in &manifest.refs {
            if source.refs()?.get(name) == Some(oid) {
                state.base.insert(name.clone(), oid.clone());
            }
        }
        state.seq = manifest.seq;
        state.synced_ms = Some(self.clock.now());
        Ok(report)
    }

    /// Sync a task both ways, as the module says.
    pub fn sync(&self, source: &dyn SyncSource, state: &mut TaskState) -> Result<SyncReport> {
        let result = self.sync_inner(source, state);
        Stats::add(&self.stats.syncs, 1);
        if result.is_err() {
            Stats::add(&self.stats.errors, 1);
        }
        result
    }

    #[allow(clippy::map_unwrap_or)] // ratchet: branchyard-sync
    fn sync_inner(&self, source: &dyn SyncSource, state: &mut TaskState) -> Result<SyncReport> {
        let task = source.task_id().to_owned();
        check_task_id(&task)?;
        let git = Git::new(source.git_dir());
        let mut report = SyncReport {
            task: task.clone(),
            ..SyncReport::default()
        };
        let mut mark: Option<Mark> = None;
        let commit = hex::encode(&crate::util::random_bytes(12)?);
        let result = (|| -> Result<()> {
            for round in 1..=self.settings.max_rounds {
                report.rounds = round;
                let read = self.manifest(&task)?;
                if let Some((m, _)) = &read {
                    if m.writer.commit == commit {
                        // Our swap landed; its response was lost.
                        report.swapped = true;
                        self.settle(state, m, &report.behind);
                        report.seq = m.seq;
                        return Ok(());
                    }
                }
                let (remote, generation) = match read {
                    Some((m, g)) => (Some(m), Some(g)),
                    None => (None, None),
                };
                if let Some(m) = &remote {
                    self.import(source, &git, m, state, &mut report)?;
                }
                let remote_refs = remote.as_ref().map(|m| m.refs.clone()).unwrap_or_default();
                let local = source.refs()?;
                let plan = plan(
                    &state.base,
                    &local,
                    &remote_refs,
                    &self.settings.device,
                    |a, b| git.is_ancestor(a, b),
                )?;
                self.apply(source, &git, &plan, &mut report)?;
                merge_names(&mut report.pulled, &plan.pulled);
                merge_names(&mut report.deleted, &plan.deleted);
                for c in &plan.conflicts {
                    if !report.conflicts.contains(c) {
                        report.conflicts.push(c.clone());
                        Stats::add(&self.stats.divergences, 1);
                    }
                }
                let mut next = remote
                    .clone()
                    .unwrap_or_else(|| Manifest::empty(&task, &self.settings.device, 0));
                next.refs = plan.manifest.clone();
                let segments = source.segments()?;
                let watermark = source.ledger_watermark().or(next.ledger_watermark);
                let sealer = self.sealer();
                let segments_changed = segments.iter().any(|s| {
                    std::fs::read(&s.path).is_ok_and(|data| {
                        next.segments.get(&s.name).map(|e| e.object.as_str())
                            != Some(sealer.name("segment", &blake3::hash(&data)).as_str())
                    })
                });
                let chunk_dir = source.chunk_dir().map(|d| d.to_path_buf());
                let new_tips: Vec<String> = plan
                    .manifest
                    .values()
                    .filter(|oid| !remote_refs.values().any(|r| r == *oid))
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let mut new_chunks: BTreeSet<ChunkId> = BTreeSet::new();
                if chunk_dir.is_some() {
                    for tip in &new_tips {
                        for id in source.reachable_chunks(tip)? {
                            if !state.remote_chunks.contains(&id.hex()) {
                                new_chunks.insert(id);
                            }
                        }
                    }
                }
                let unchanged = remote.is_some()
                    && next.refs == remote_refs
                    && new_chunks.is_empty()
                    && !segments_changed
                    && watermark == next.ledger_watermark;
                if unchanged || (remote.is_none() && next.refs.is_empty() && segments.is_empty()) {
                    if let Some(m) = &remote {
                        self.settle(state, m, &report.behind);
                        report.seq = m.seq;
                    }
                    report.behind.sort();
                    return Ok(());
                }
                if mark.is_none() {
                    mark = Some(self.begin_write()?);
                }
                // The pack of what the remote's refs do not reach.
                let tips: Vec<String> = next
                    .refs
                    .values()
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let exclude: Vec<String> = remote_refs
                    .values()
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let (pack, objects) = git.pack(&tips, &exclude)?;
                let mut new_bytes = 0u64;
                let pack_upload = (objects > 0).then_some(pack);
                if let Some(pack) = &pack_upload {
                    new_bytes += pack.len() as u64;
                }
                let mut chunk_sizes: BTreeMap<String, u64> = BTreeMap::new();
                if let Some(dir) = &chunk_dir {
                    for id in &new_chunks {
                        let size = std::fs::metadata(crate::source::chunk_path(dir, id))
                            .map(|m| m.len())
                            .map_err(|e| Error::local(format!("chunk {}: {e}", id.hex())))?;
                        chunk_sizes.insert(id.hex(), size);
                        new_bytes += size;
                    }
                }
                self.admit(new_bytes)?;
                if let Some(pack) = pack_upload {
                    let (name, _, bytes) = self.write_content("pack", pack_key, &pack, false)?;
                    let entry = PackEntry {
                        name: name.clone(),
                        objects,
                        bytes,
                    };
                    if !next.packs.iter().any(|p| p.name == name) {
                        next.packs.push(entry.clone());
                    }
                    report.pack = Some(entry);
                    state.imported_packs.insert(name);
                }
                if let (Some(dir), false) = (&chunk_dir, new_chunks.is_empty()) {
                    let up = std::sync::atomic::AtomicU64::new(0);
                    self.parallel(new_chunks.iter().copied().collect(), |id| {
                        let data = read_chunk(dir, &id)?;
                        self.write_content("chunk", chunk_key, &data, true)?;
                        up.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        Ok(())
                    })?;
                    report.chunks_up = up.into_inner();
                    let index = ChunkIndex {
                        chunks: chunk_sizes,
                    };
                    let data = serde_json::to_vec(&index)?;
                    let (name, _, bytes) = self.write_content("index", index_key, &data, false)?;
                    if !next.chunk_indexes.iter().any(|i| i.name == name) {
                        next.chunk_indexes.push(IndexEntry {
                            name,
                            chunks: index.chunks.len() as u64,
                            bytes,
                        });
                    }
                }
                for segment in &segments {
                    let data = std::fs::read(&segment.path)?;
                    let (name, _, bytes) =
                        self.write_content("segment", segment_key, &data, true)?;
                    if next.segments.get(&segment.name).map(|e| &e.object) != Some(&name) {
                        next.segments.insert(
                            segment.name.clone(),
                            SegmentEntry {
                                object: name,
                                bytes,
                            },
                        );
                        report.segments_up += 1;
                    }
                }
                next.ledger_watermark = watermark;
                next.seq = remote.as_ref().map(|m| m.seq).unwrap_or(0) + 1;
                next.writer = Writer {
                    device: self.settings.device.clone(),
                    commit: commit.clone(),
                    at_ms: self.clock.now(),
                };
                let live = match mark.as_mut() {
                    Some(m) => self.keep_mark(m)?,
                    None => false,
                };
                if !live {
                    // Our mark ran out: a sweep may have run. Start over,
                    // marked again.
                    if let Some(old) = mark.take() {
                        self.end_write(old);
                    }
                    continue;
                }
                // Publish under the keyring's current version, read now: a
                // rotation since this round began is taken up here. The
                // objects already stored stay readable under their
                // version, which is retired only after the grace period,
                // by a rotation that sees them and rewraps them.
                self.refresh_keys()?;
                let sealer = self.sealer();
                let key = manifest_key(&sealer.task_dir(&task));
                let framed = sealer.seal(&key, &next.encode()?)?;
                let swapped = match &generation {
                    None => self.store.put_if_absent(&key, &framed),
                    Some(g) => self.store.put_if_match(&key, &framed, g),
                };
                match swapped {
                    Ok(_) => {
                        Stats::add(&self.stats.swaps, 1);
                        report.swapped = true;
                        report.seq = next.seq;
                        merge_names(&mut report.pushed, &plan.pushed);
                        self.settle(state, &next, &report.behind);
                        state
                            .remote_chunks
                            .extend(new_chunks.iter().map(ChunkId::hex));
                        return Ok(());
                    }
                    Err(e) if e.is(Kind::Precondition) => {
                        Stats::add(&self.stats.swap_conflicts, 1);
                        self.retrier.sleeper.sleep(self.retrier.backoff(round));
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(Error::transient(format!(
                "{task}: the manifest kept moving; gave up after {} rounds",
                self.settings.max_rounds
            )))
        })();
        if let Some(m) = mark.take() {
            self.end_write(m);
        }
        report.behind.sort();
        result.map(|_| report)
    }

    /// Agree on `manifest` as the base, except for the refs in `behind`:
    /// a remote change this machine has not applied (a ref checked out
    /// here that the remote moved or deleted) keeps its prior base entry
    /// until it is applied. Taking the remote's value as the base would
    /// make the unapplied local ref look like a local change: a deletion
    /// would be pushed back as a new ref, undoing it.
    fn settle(&self, state: &mut TaskState, manifest: &Manifest, behind: &[String]) {
        let prior = std::mem::replace(&mut state.base, manifest.refs.clone());
        for name in behind {
            match prior.get(name) {
                Some(oid) => state.base.insert(name.clone(), oid.clone()),
                None => state.base.remove(name),
            };
        }
        state.seq = manifest.seq;
        state.synced_ms = Some(self.clock.now());
        state
            .imported_packs
            .extend(manifest.packs.iter().map(|p| p.name.clone()));
    }

    /// Set a legal hold on a task: its manifest is not deleted (by
    /// retention or `remove_task`) and nothing it references is collected
    /// until the hold is released.
    pub fn hold(&self, task: &str, reason: &str, by: &str) -> Result<()> {
        check_task_id(task)?;
        let sealer = self.write_sealer()?;
        let key = hold_key(&sealer.task_dir(task));
        let hold = Hold {
            task: task.to_owned(),
            reason: reason.to_owned(),
            by: by.to_owned(),
            at_ms: self.clock.now(),
        };
        let framed = sealer.seal(&key, &serde_json::to_vec(&hold)?)?;
        match self.store.put_if_absent(&key, &framed) {
            Ok(_) => Ok(()),
            Err(e) if e.is(Kind::Precondition) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn release_hold(&self, task: &str) -> Result<bool> {
        check_task_id(task)?;
        let key = hold_key(&self.sealer().task_dir(task));
        match self.store.stat(&key)? {
            Some(entry) => {
                self.store.delete_if_match(&key, &entry.generation)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn held(&self, task: &str) -> Result<Option<Hold>> {
        check_task_id(task)?;
        let key = hold_key(&self.sealer().task_dir(task));
        match self.read(&key) {
            Ok((plain, _)) => Ok(Some(serde_json::from_slice(&plain)?)),
            Err(e) if e.is(Kind::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Delete a task from the remote (its manifest; the collector reclaims
    /// what nothing else references). Refused under a legal hold.
    pub fn remove_task(&self, task: &str) -> Result<bool> {
        if let Some(hold) = self.held(task)? {
            return Err(Error::new(
                Kind::Held,
                format!(
                    "{task} is on legal hold ({}); release it first",
                    hold.reason
                ),
            ));
        }
        match self.manifest(task)? {
            Some((_, generation)) => {
                let key = manifest_key(&self.sealer().task_dir(task));
                self.store.delete_if_match(&key, &generation)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

fn merge_names(into: &mut Vec<String>, from: &[String]) {
    for name in from {
        if !into.contains(name) {
            into.push(name.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// History: a <- b <- c, and a <- x (diverged).
    fn ancestry(a: &str, b: &str) -> Result<bool> {
        let order = ["a", "b", "c"];
        Ok(a == b
            || (a == "a" && b == "x")
            || matches!(
                (order.iter().position(|o| *o == a), order.iter().position(|o| *o == b)),
                (Some(i), Some(j)) if i <= j
            ))
    }

    const MAIN: &str = "refs/heads/main";

    #[test]
    fn fast_forwards_move_the_behind_side() {
        let p = plan(
            &refs(&[(MAIN, "a")]),
            &refs(&[(MAIN, "c")]),
            &refs(&[(MAIN, "a")]),
            "d",
            ancestry,
        )
        .unwrap();
        assert_eq!(p.manifest[MAIN], "c");
        assert_eq!(p.pushed, vec![MAIN]);
        assert!(p.local.is_empty());
        let p = plan(
            &refs(&[(MAIN, "a")]),
            &refs(&[(MAIN, "a")]),
            &refs(&[(MAIN, "c")]),
            "d",
            ancestry,
        )
        .unwrap();
        assert_eq!(p.manifest[MAIN], "c");
        assert_eq!(
            p.local,
            vec![LocalUpdate {
                task_ref: MAIN.into(),
                old: Some("a".into()),
                new: Some("c".into())
            }]
        );
    }

    #[test]
    fn divergence_becomes_a_conflict_branch_once() {
        let local = refs(&[(MAIN, "x")]);
        let remote = refs(&[(MAIN, "c")]);
        let p = plan(&refs(&[(MAIN, "a")]), &local, &remote, "laptop", ancestry).unwrap();
        assert_eq!(p.manifest[MAIN], "c", "the remote's value stays");
        assert_eq!(p.manifest["refs/heads/conflict/laptop/1"], "x");
        assert_eq!(
            p.conflicts,
            vec![(MAIN.to_owned(), "refs/heads/conflict/laptop/1".to_owned())]
        );
        // Recorded already: no second conflict branch, and not overwritten.
        let remote = refs(&[(MAIN, "c"), ("refs/heads/conflict/laptop/1", "x")]);
        let local = refs(&[(MAIN, "x"), ("refs/heads/conflict/laptop/1", "x")]);
        let p = plan(&remote, &local, &remote, "laptop", ancestry).unwrap();
        assert!(p.conflicts.is_empty());
        assert_eq!(p.manifest[MAIN], "c");
        // Another device's divergence takes the next free number of its own.
        let p = plan(
            &refs(&[]),
            &refs(&[(MAIN, "x")]),
            &remote,
            "phone",
            ancestry,
        )
        .unwrap();
        assert_eq!(p.manifest["refs/heads/conflict/phone/1"], "x");
    }

    #[test]
    fn deletions_follow_the_base() {
        let base = refs(&[(MAIN, "a"), ("refs/heads/attempt/1", "b")]);
        // Deleted here, untouched there: deleted from the manifest.
        let p = plan(&base, &refs(&[(MAIN, "a")]), &base, "d", ancestry).unwrap();
        assert!(!p.manifest.contains_key("refs/heads/attempt/1"));
        // Deleted there, untouched here: deleted here.
        let p = plan(&base, &base, &refs(&[(MAIN, "a")]), "d", ancestry).unwrap();
        assert_eq!(p.local[0].new, None);
        // New there: pulled; new here: pushed.
        let p = plan(
            &refs(&[]),
            &refs(&[("refs/heads/n", "a")]),
            &refs(&[("refs/heads/m", "b")]),
            "d",
            ancestry,
        )
        .unwrap();
        assert_eq!(p.manifest.len(), 2);
        assert_eq!(p.pulled, vec!["refs/heads/m"]);
        assert_eq!(p.pushed, vec!["refs/heads/n"]);
    }

    #[test]
    fn devices_are_sanitized() {
        assert_eq!(sanitize_device("My Laptop.local\n"), "my-laptop-local");
        assert_eq!(sanitize_device("..."), "device");
    }
}
