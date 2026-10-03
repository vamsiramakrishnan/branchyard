//! The bucket's layout, and a task's manifest: the one object per task
//! that changes.
//!
//! ```text
//! keyring.json                    encryption mode, wrapped key versions
//! tasks/<task>/manifest           refs, packs, chunk indexes, segments (CAS)
//! tasks/<task>/hold               a legal hold, when one is set
//! leases/<task>/<attempt>         who runs the attempt, until when (CAS)
//! packs/<name>.pack               git packs (immutable)
//! indexes/<name>                  lists of a task's chunks (immutable)
//! chunks/<aa>/<name>              large-file chunks (immutable)
//! segments/<name>                 conversation segments (immutable)
//! gc/candidates, gc/usage         the collector's marks and the tenant's usage
//! locks/sweep, locks/writers/...  the collector's sweep and active writers
//! ```
//!
//! `<task>` and `<name>` are the ID and the BLAKE3 hash in a plain remote,
//! keyed hashes in a sealed one ([`crate::seal`]). Every object but
//! `keyring.json` is framed by the sealer.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const FORMAT: u32 = 1;

pub fn manifest_key(task_dir: &str) -> String {
    format!("tasks/{task_dir}/manifest")
}

pub fn hold_key(task_dir: &str) -> String {
    format!("tasks/{task_dir}/hold")
}

pub fn lease_key(task_dir: &str, attempt: &str) -> String {
    format!("leases/{task_dir}/{attempt}")
}

pub fn pack_key(name: &str) -> String {
    format!("packs/{name}.pack")
}

pub fn index_key(name: &str) -> String {
    format!("indexes/{name}")
}

pub fn chunk_key(name: &str) -> String {
    format!("chunks/{}/{name}", &name[..2.min(name.len())])
}

pub fn segment_key(name: &str) -> String {
    format!("segments/{name}")
}

/// The prefixes of content-addressed objects, which the collector walks.
pub const CONTENT_PREFIXES: [&str; 4] = ["packs/", "indexes/", "chunks/", "segments/"];

/// Check a task ID: 1 to 128 of `A-Z a-z 0-9 . _ - ~`, not starting with
/// `.` (a branch name maps here with `/` as `~`, see
/// [`crate::source::task_id_for_branch`]).
pub fn check_task_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~'));
    match ok {
        true => Ok(()),
        false => Err(Error::config(format!(
            "{id:?} is not a task ID (1 to 128 of A-Z a-z 0-9 . _ - ~)"
        ))),
    }
}

/// A git pack the task's history is in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackEntry {
    pub name: String,
    pub objects: u64,
    pub bytes: u64,
}

/// A list of chunks the task references.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub name: String,
    pub chunks: u64,
    pub bytes: u64,
}

/// A conversation segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentEntry {
    pub object: String,
    pub bytes: u64,
}

/// Who wrote a manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Writer {
    pub device: String,
    /// Random per swap, so a writer recognizes its own swap after a lost
    /// response.
    pub commit: String,
    pub at_ms: u64,
}

/// One task's state in the remote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub task: String,
    /// One more on each swap.
    pub seq: u64,
    pub writer: Writer,
    /// Refs in the task's own names (`refs/heads/main`,
    /// `refs/heads/attempt/2`, `refs/branchyard/...`,
    /// `refs/heads/conflict/<device>/<n>`), to commits.
    pub refs: BTreeMap<String, String>,
    /// Packs holding every object the refs reach.
    pub packs: Vec<PackEntry>,
    /// Indexes listing every chunk the task's commits point at.
    #[serde(default)]
    pub chunk_indexes: Vec<IndexEntry>,
    /// Segments by name.
    #[serde(default)]
    pub segments: BTreeMap<String, SegmentEntry>,
    /// How far the task's effect ledger is synced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_watermark: Option<u64>,
    /// Days to keep the task after its last change; `None` follows the
    /// remote's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

impl Manifest {
    pub fn empty(task: &str, device: &str, now_ms: u64) -> Manifest {
        Manifest {
            format: FORMAT,
            task: task.to_owned(),
            seq: 0,
            writer: Writer {
                device: device.to_owned(),
                commit: String::new(),
                at_ms: now_ms,
            },
            refs: BTreeMap::new(),
            packs: Vec::new(),
            chunk_indexes: Vec::new(),
            segments: BTreeMap::new(),
            ledger_watermark: None,
            retention_days: None,
        }
    }

    pub fn decode(data: &[u8]) -> Result<Manifest> {
        let manifest: Manifest = serde_json::from_slice(data)?;
        if manifest.format != FORMAT {
            return Err(Error::config(format!(
                "manifest format {} is not one this build reads",
                manifest.format
            )));
        }
        Ok(manifest)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Every content object key it references, except the chunks inside
    /// its indexes.
    pub fn object_keys(&self) -> BTreeSet<String> {
        let mut keys = BTreeSet::new();
        for p in &self.packs {
            keys.insert(pack_key(&p.name));
        }
        for i in &self.chunk_indexes {
            keys.insert(index_key(&i.name));
        }
        for s in self.segments.values() {
            keys.insert(segment_key(&s.object));
        }
        keys
    }

    /// Bytes its packs, indexes and segments hold.
    pub fn bytes(&self) -> u64 {
        self.packs.iter().map(|p| p.bytes).sum::<u64>()
            + self.chunk_indexes.iter().map(|i| i.bytes).sum::<u64>()
            + self.segments.values().map(|s| s.bytes).sum::<u64>()
    }
}

/// A chunk index: the chunks one sync uploaded, by name and size.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkIndex {
    pub chunks: BTreeMap<String, u64>,
}

/// A legal hold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub task: String,
    pub reason: String,
    pub by: String,
    pub at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_and_keys() {
        for ok in ["board-update", "01J9ABC", "feature~login", "a.b_c"] {
            check_task_id(ok).unwrap();
        }
        for bad in ["", ".x", "a/b", "a b", &"x".repeat(129)] {
            assert!(check_task_id(bad).is_err(), "{bad}");
        }
        assert_eq!(chunk_key("abcdef"), "chunks/ab/abcdef");
        let mut m = Manifest::empty("t", "dev", 1);
        m.packs.push(PackEntry {
            name: "p".into(),
            objects: 3,
            bytes: 10,
        });
        let back = Manifest::decode(&m.encode().unwrap()).unwrap();
        assert_eq!(back, m);
        assert!(back.object_keys().contains("packs/p.pack"));
    }
}
