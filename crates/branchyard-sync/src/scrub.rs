//! Scrubbing: read a sample of the remote's objects back and check each
//! against its name, and check that every object a manifest references
//! is there. A corrupt chunk is repaired from a local chunk directory
//! when one holds a copy that verifies; packs, indexes and segments are
//! reported (a pack is rebuilt by the next sync after its manifest entry
//! is dropped, which is not automatic yet).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::engine::Remote;
use crate::error::{Kind, Result};
use crate::manifest::{chunk_key, Manifest, CONTENT_PREFIXES};
use crate::source::{read_chunk, ChunkId};
use crate::store::ObjectStore as _;
use crate::util::SplitMix;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrubReport {
    pub manifests: usize,
    pub objects: usize,
    pub checked: usize,
    /// `(key, why)`.
    pub corrupt: Vec<(String, String)>,
    /// Referenced by a manifest and not in the remote.
    pub missing: Vec<String>,
    pub repaired: Vec<String>,
}

impl ScrubReport {
    pub fn clean(&self) -> bool {
        self.corrupt.is_empty() && self.missing.is_empty()
    }
}

/// The kind and name a content key carries.
fn kind_and_name(key: &str) -> Option<(&'static str, &str)> {
    if let Some(rest) = key.strip_prefix("packs/") {
        return Some(("pack", rest.strip_suffix(".pack")?));
    }
    if let Some(rest) = key.strip_prefix("indexes/") {
        return Some(("index", rest));
    }
    if let Some(rest) = key.strip_prefix("chunks/") {
        return Some(("chunk", rest.split_once('/')?.1));
    }
    if let Some(rest) = key.strip_prefix("segments/") {
        return Some(("segment", rest));
    }
    None
}

impl Remote {
    /// Check `sample` content objects (all of them when `sample` is at
    /// least their number), chosen with `seed`, and every manifest.
    /// `chunk_dirs` are local chunk directories to repair chunks from.
    pub fn scrub(
        &self,
        sample: usize,
        seed: u64,
        chunk_dirs: &[std::path::PathBuf],
    ) -> Result<ScrubReport> {
        let mut report = ScrubReport::default();
        let mut listing = Vec::new();
        for prefix in CONTENT_PREFIXES {
            listing.extend(self.store.list(prefix)?);
        }
        let present: BTreeSet<String> = listing.iter().map(|e| e.key.clone()).collect();
        report.objects = listing.len();
        for entry in self.store.list("tasks/")? {
            if !entry.key.ends_with("/manifest") {
                continue;
            }
            report.manifests += 1;
            let manifest = match self.read(&entry.key) {
                Ok((plain, _)) => match Manifest::decode(&plain) {
                    Ok(m) => m,
                    Err(e) => {
                        report.corrupt.push((entry.key.clone(), e.to_string()));
                        continue;
                    }
                },
                Err(e) if e.is(Kind::NotFound) => continue,
                Err(e) => {
                    report.corrupt.push((entry.key.clone(), e.to_string()));
                    continue;
                }
            };
            for key in manifest.object_keys() {
                if !present.contains(&key) {
                    report.missing.push(key);
                }
            }
        }
        // A deterministic sample: a partial Fisher-Yates shuffle.
        let mut rng = SplitMix::new(seed);
        let n = sample.min(listing.len());
        for i in 0..n {
            let j = i + rng.below_or_at((listing.len() - 1 - i) as u64) as usize;
            listing.swap(i, j);
        }
        for entry in listing.into_iter().take(n) {
            let Some((kind, name)) = kind_and_name(&entry.key) else {
                continue;
            };
            report.checked += 1;
            match self.read_content(kind, &entry.key, name) {
                Ok(_) => {}
                Err(e) if e.is(Kind::Corrupt) => {
                    let mut repaired = false;
                    if kind == "chunk" {
                        repaired = self.repair_chunk(&entry, name, chunk_dirs)?;
                    }
                    match repaired {
                        true => report.repaired.push(entry.key.clone()),
                        false => report.corrupt.push((entry.key.clone(), e.to_string())),
                    }
                }
                Err(e) if e.is(Kind::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        report.missing.sort();
        report.missing.dedup();
        Ok(report)
    }

    fn repair_chunk(
        &self,
        entry: &crate::store::Entry,
        name: &str,
        chunk_dirs: &[std::path::PathBuf],
    ) -> Result<bool> {
        let sealer = self.sealer();
        for dir in chunk_dirs {
            let Ok(shards) = std::fs::read_dir(dir) else {
                continue;
            };
            for shard in shards.flatten() {
                let Ok(files) = std::fs::read_dir(shard.path()) else {
                    continue;
                };
                for file in files.flatten() {
                    let Ok(id) = ChunkId::parse(&file.file_name().to_string_lossy()) else {
                        continue;
                    };
                    if sealer.name("chunk", &id.hash()) != name {
                        continue;
                    }
                    let Ok(data) = read_chunk(dir, &id) else {
                        continue;
                    };
                    let key = chunk_key(name);
                    let framed = sealer.seal(&key, &data)?;
                    return match self.store.put_if_match(&key, &framed, &entry.generation) {
                        Ok(_) => Ok(true),
                        Err(e) if e.is(Kind::Precondition) => Ok(false),
                        Err(e) => Err(e),
                    };
                }
            }
        }
        Ok(false)
    }
}
