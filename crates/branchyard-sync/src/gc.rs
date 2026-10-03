//! Garbage collection: two-phase mark and sweep from every manifest, with
//! a grace period, per-task retention, legal holds and the tenant's usage.
//!
//! One collection:
//!
//! 1. **Sweep lock.** Take `locks/sweep` (a lease object with an expiry),
//!    then look for live writers (`locks/writers/*`, which each sync
//!    writes before it uploads and removes after it swaps). If any is
//!    live, release the lock and stop: the collection is deferred. A
//!    writer checks for the sweep lock after marking itself, so a writer
//!    and a sweep never overlap (each object store here is strongly
//!    consistent for these operations). Expired writer marks are removed.
//! 2. **Retention.** A task whose manifest has not changed for longer
//!    than its retention (the manifest's `retention_days`, else the
//!    remote's setting) is deleted, unless it is on legal hold.
//! 3. **Mark.** Everything any manifest references (its packs, chunk
//!    indexes and every chunk they list, its segments) is live. Every
//!    other content object becomes a candidate, stamped with when it was
//!    first seen unreferenced and its generation; a candidate referenced
//!    again is unmarked.
//! 4. **Sweep.** A candidate first seen unreferenced longer ago than the
//!    grace period, still unreferenced now and still at the same
//!    generation, is deleted (conditionally). The grace period is longer
//!    than any upload takes, so an object uploaded for a manifest not yet
//!    swapped is never collected from under its writer.
//! 5. **Usage.** The bytes and objects left are written to `gc/usage`,
//!    which quota checks read.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::engine::{LockRecord, Remote};
use crate::error::{Error, Kind, Result};
use crate::manifest::{chunk_key, ChunkIndex, Manifest, CONTENT_PREFIXES};
use crate::source::ChunkId;
use crate::store::{Entry, ObjectStore as _};

/// The tenant's stored bytes, as the collector last counted them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub bytes: u64,
    pub objects: u64,
    pub at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Candidate {
    marked_ms: u64,
    generation: String,
    size: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Candidates {
    marks: BTreeMap<String, Candidate>,
}

/// What a collection did (or, dry, would do).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcReport {
    pub dry_run: bool,
    /// Why nothing was swept, when nothing was.
    pub deferred: Option<String>,
    pub manifests: usize,
    pub held: Vec<String>,
    /// Tasks deleted for retention.
    pub expired_tasks: Vec<String>,
    pub referenced: usize,
    pub unreferenced: usize,
    /// Unreferenced objects still within their grace period.
    pub in_grace: usize,
    pub deleted: Vec<String>,
    pub bytes_freed: u64,
    pub usage: Usage,
}

const SWEEP: &str = "locks/sweep";
const CANDIDATES: &str = "gc/candidates";
const USAGE: &str = "gc/usage";

impl Remote {
    fn write_framed(&self, key: &str, data: &[u8]) -> Result<()> {
        let framed = self.sealer().seal(key, data)?;
        match self.store.stat(key)? {
            Some(entry) => self
                .store
                .put_if_match(key, &framed, &entry.generation)
                .map(|_| ()),
            None => self.store.put_if_absent(key, &framed).map(|_| ()),
        }
    }

    fn take_sweep_lock(&self) -> Result<Option<String>> {
        let now = self.clock.now();
        let record = LockRecord {
            holder: self.settings.device.clone(),
            expires_ms: now + self.settings.writer_ttl.as_millis() as u64,
        };
        let framed = self.sealer().seal(SWEEP, &serde_json::to_vec(&record)?)?;
        match self.read(SWEEP) {
            Err(e) if e.is(Kind::NotFound) => match self.store.put_if_absent(SWEEP, &framed) {
                Ok(g) => Ok(Some(g)),
                Err(e) if e.is(Kind::Precondition) => Ok(None),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
            Ok((plain, generation)) => {
                let held: LockRecord = serde_json::from_slice(&plain)?;
                if self.lock_live(&held) {
                    return Ok(None);
                }
                match self.store.put_if_match(SWEEP, &framed, &generation) {
                    Ok(g) => Ok(Some(g)),
                    Err(e) if e.is(Kind::Precondition) => Ok(None),
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// Collect garbage, as the module says. `dry_run` takes no lock and
    /// changes nothing.
    pub fn gc(&self, dry_run: bool) -> Result<GcReport> {
        let mut report = GcReport {
            dry_run,
            ..GcReport::default()
        };
        let lock = match dry_run {
            true => None,
            false => match self.take_sweep_lock()? {
                Some(g) => Some(g),
                None => {
                    report.deferred = Some("another collector holds the sweep lock".into());
                    return Ok(report);
                }
            },
        };
        let result = self.collect(&mut report, dry_run);
        if let Some(generation) = lock {
            let _ = self.store.delete_if_match(SWEEP, &generation);
        }
        result.map(|_| report)
    }

    fn collect(&self, report: &mut GcReport, dry_run: bool) -> Result<()> {
        let now = self.clock.now();
        // 1. Writers.
        let mut live_writers = 0;
        for entry in self.store.list("locks/writers/")? {
            let record: Option<LockRecord> = self
                .read(&entry.key)
                .ok()
                .and_then(|(plain, _)| serde_json::from_slice(&plain).ok());
            match record {
                Some(r) if self.lock_live(&r) => live_writers += 1,
                _ if !dry_run => {
                    let _ = self.store.delete_if_match(&entry.key, &entry.generation);
                }
                _ => {}
            }
        }
        if live_writers > 0 {
            report.deferred = Some(format!(
                "{live_writers} writer(s) are uploading; try again when they finish"
            ));
            if !dry_run {
                return Ok(());
            }
        }
        // 2. Retention and holds.
        let sealer = self.sealer();
        let task_entries = self.store.list("tasks/")?;
        let holds: BTreeSet<String> = task_entries
            .iter()
            .filter(|e| e.key.ends_with("/hold"))
            .map(|e| e.key.trim_end_matches("/hold").to_owned())
            .collect();
        let mut manifests: Vec<Manifest> = Vec::new();
        for entry in task_entries.iter().filter(|e| e.key.ends_with("/manifest")) {
            let (plain, generation) = match self.read(&entry.key) {
                Ok(r) => r,
                Err(e) if e.is(Kind::NotFound) => continue,
                Err(e) => return Err(e),
            };
            let manifest = Manifest::decode(&plain)?;
            let dir = entry.key.trim_end_matches("/manifest");
            let held = holds.contains(dir);
            if held {
                report.held.push(manifest.task.clone());
            }
            let retention_ms = manifest
                .retention_days
                .map(|d| d as u64 * 86_400_000)
                .or_else(|| self.settings.retention.map(|r| r.as_millis() as u64));
            if let (false, Some(keep)) = (held, retention_ms) {
                if manifest.writer.at_ms + keep <= now {
                    if !dry_run {
                        match self.store.delete_if_match(&entry.key, &generation) {
                            Ok(()) => {}
                            // Changed meanwhile: it is not idle.
                            Err(e) if e.is(Kind::Precondition) => {
                                manifests.push(manifest);
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    report.expired_tasks.push(manifest.task.clone());
                    continue;
                }
            }
            manifests.push(manifest);
        }
        report.manifests = manifests.len();
        // 3. Mark.
        let mut referenced: BTreeSet<String> = BTreeSet::new();
        for manifest in &manifests {
            referenced.extend(manifest.object_keys());
            for index in &manifest.chunk_indexes {
                let data = self.read_content(
                    "index",
                    &crate::manifest::index_key(&index.name),
                    &index.name,
                )?;
                let list: ChunkIndex = serde_json::from_slice(&data)?;
                for hex in list.chunks.keys() {
                    let id = ChunkId::parse(hex)?;
                    referenced.insert(chunk_key(&sealer.name("chunk", &id.hash())));
                }
            }
        }
        report.referenced = referenced.len();
        let mut listing: Vec<Entry> = Vec::new();
        for prefix in CONTENT_PREFIXES {
            listing.extend(self.store.list(prefix)?);
        }
        let (old, candidates_generation) = match self.read(CANDIDATES) {
            Ok((plain, g)) => (serde_json::from_slice::<Candidates>(&plain)?, Some(g)),
            Err(e) if e.is(Kind::NotFound) => (Candidates::default(), None),
            Err(e) => return Err(e),
        };
        let _ = candidates_generation;
        let grace = self.settings.grace.as_millis() as u64;
        let mut next = Candidates::default();
        let mut sweep: Vec<(String, Candidate)> = Vec::new();
        for entry in &listing {
            if referenced.contains(&entry.key) {
                continue;
            }
            report.unreferenced += 1;
            let candidate = match old.marks.get(&entry.key) {
                Some(c) if c.generation == entry.generation => c.clone(),
                _ => Candidate {
                    marked_ms: now,
                    generation: entry.generation.clone(),
                    size: entry.size,
                },
            };
            if candidate.marked_ms + grace <= now && report.deferred.is_none() {
                sweep.push((entry.key.clone(), candidate));
            } else {
                report.in_grace += 1;
                next.marks.insert(entry.key.clone(), candidate);
            }
        }
        // 4. Sweep.
        let mut freed = BTreeSet::new();
        for (key, candidate) in sweep {
            if dry_run {
                report.deleted.push(key.clone());
                report.bytes_freed += candidate.size;
                continue;
            }
            match self.store.delete_if_match(&key, &candidate.generation) {
                Ok(()) => {
                    report.bytes_freed += candidate.size;
                    report.deleted.push(key.clone());
                    freed.insert(key);
                }
                Err(e) if e.is(Kind::Precondition) => {}
                Err(e) => return Err(e),
            }
        }
        // 5. Marks and usage.
        report.usage = Usage {
            bytes: listing
                .iter()
                .filter(|e| !freed.contains(&e.key))
                .map(|e| e.size)
                .sum(),
            objects: (listing.len() - freed.len()) as u64,
            at_ms: now,
        };
        if !dry_run {
            self.write_framed(CANDIDATES, &serde_json::to_vec(&next)?)?;
            self.write_framed(USAGE, &serde_json::to_vec(&report.usage)?)?;
        }
        Ok(())
    }
}

/// Refuse a collection setting that would be unsafe.
pub fn check_grace(grace: std::time::Duration, writer_ttl: std::time::Duration) -> Result<()> {
    match grace >= writer_ttl {
        true => Ok(()),
        false => Err(Error::config(format!(
            "the grace period ({}s) must be at least a writer's mark ({}s)",
            grace.as_secs(),
            writer_ttl.as_secs()
        ))),
    }
}
