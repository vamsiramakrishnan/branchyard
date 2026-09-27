//! Portable, verified artifact bundles: `by artifact export`/`import` and
//! [`crate::Yard::export_artifacts`]/[`crate::Yard::import_artifacts`]. See
//! `docs/storage.md` "Portable bundles".
//!
//! Idea from Straitjacket's evidence capsule (`src/ctx/capsule.py`,
//! Apache-2.0): a self-contained, content-addressed bundle a receiving
//! process can verify without trusting the sender; see `docs/comparison.md`
//! Absorption plan → Straitjacket. Nothing is copied, only the shape: a
//! deterministic archive ([`crate::tarball`]) holding an index of each
//! member's provenance and its blake3 digest, checked member by member
//! against both on export and on import, plus a completeness check (every
//! indexed member present, no extras).
//!
//! Import creates new artifacts owned by the importing branch, bound to
//! its incarnation like any other publish ([`crate::storage::publish`]);
//! the bundle's original provenance is kept in the new artifact's labels
//! (`bundle.origin_*`), never assumed to still identify a live branch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::storage::{self, ArtifactRef};
use crate::{Error, Yard};

const INDEX_MEMBER: &str = "index.json";
/// Bumped only if a future change to the bundle's shape is incompatible
/// with this reader.
const BUNDLE_VERSION: u32 = 1;

fn member_path(id: &str) -> String {
    format!("artifacts/{id}")
}

/// One artifact's provenance as recorded in a bundle's index. Mirrors
/// [`ArtifactRef`] plus nothing else: the digest it already carries is the
/// bundle's own integrity check, re-verified independently of the
/// exporting store.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BundleEntry {
    pub id: String,
    pub digest: String,
    pub size: u64,
    pub name: String,
    pub media_type: String,
    pub publisher_branch: String,
    pub turn: u64,
    pub created_at: u64,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl From<ArtifactRef> for BundleEntry {
    fn from(a: ArtifactRef) -> BundleEntry {
        BundleEntry {
            id: a.id,
            digest: a.digest,
            size: a.size,
            name: a.name,
            media_type: a.media_type,
            publisher_branch: a.publisher_branch,
            turn: a.turn,
            created_at: a.created_at,
            labels: a.labels,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BundleIndex {
    version: u32,
    entries: Vec<BundleEntry>,
}

/// A file removed on drop, so an export or import that fails partway never
/// leaves a stray temp file behind.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn temp_path(dir: &Path, what: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    dir.join(format!(
        ".bundle-{what}.{}.{}.{}.tmp",
        std::process::id(),
        crate::state::now_ms(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

fn io(what: &str, path: &Path, e: std::io::Error) -> Error {
    Error::State(format!("{what} {}: {e}", path.display()))
}

/// Export every artifact in `ids` that `reader` may read into a
/// deterministic tar at `out`: `index.json` first, then each artifact's
/// bytes at `artifacts/<id>`, sorted by id, with the fixed mtime/uid/gid
/// mode [`crate::tarball`] always writes, so exporting the same artifacts
/// twice produces byte-identical files. Each member's bytes are hashed as
/// they are read and checked against the artifact's own recorded digest
/// before being written, so a locally corrupted blob is refused rather
/// than exported.
pub(crate) fn export_artifacts(
    yard: &Yard,
    reader: &str,
    ids: &[String],
    out: &Path,
) -> Result<Vec<BundleEntry>, Error> {
    let store = yard.store();
    let mut sorted: Vec<String> = ids.to_vec();
    sorted.sort();
    sorted.dedup();
    if sorted.is_empty() {
        return Err(Error::Denied(
            "export needs at least one artifact id".into(),
        ));
    }
    let mut entries = Vec::with_capacity(sorted.len());
    let mut tar_members = Vec::with_capacity(sorted.len() + 1);
    for id in &sorted {
        let tmp = TempFile(temp_path(store.dir(), "export"));
        let artifact = storage::get(yard, reader, id, &tmp.0)?;
        let bytes = std::fs::read(&tmp.0).map_err(|e| io("read", &tmp.0, e))?;
        drop(tmp);
        if bytes.len() as u64 != artifact.size
            || blake3::hash(&bytes).to_hex().as_str() != artifact.digest
        {
            return Err(Error::State(format!(
                "artifact {id}'s stored bytes no longer match its recorded digest; refusing to \
                 export a corrupted member"
            )));
        }
        tar_members.push((member_path(id), bytes));
        entries.push(BundleEntry::from(artifact));
    }
    let index = BundleIndex {
        version: BUNDLE_VERSION,
        entries: entries.clone(),
    };
    let index_bytes = serde_json::to_vec_pretty(&index)
        .map_err(|e| Error::State(format!("encode bundle index: {e}")))?;
    let mut members = vec![(INDEX_MEMBER.to_owned(), index_bytes)];
    members.extend(tar_members);
    let archive = crate::tarball::write(&members)?;
    if let Some(dir) = out.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| io("create", dir, e))?;
        }
    }
    std::fs::write(out, &archive).map_err(|e| io("write", out, e))?;
    Ok(entries)
}

/// Import a bundle written by [`export_artifacts`], publishing each member
/// as a new artifact owned by `branch`. Refuses the whole import if:
/// the archive has no index; a member the index names is missing from the
/// archive; the archive has a member the index does not name (an extra);
/// or a member's bytes do not match the size or blake3 digest its index
/// entry records (tampered or corrupted). Nothing is published until every
/// member has passed these checks. Each new artifact's labels record its
/// original provenance (`bundle.origin_id`, `bundle.origin_publisher`,
/// `bundle.origin_created_at`), since the original branch may no longer
/// exist by the time it is imported.
pub(crate) fn import_artifacts(
    yard: &Yard,
    branch: &str,
    path: &Path,
) -> Result<Vec<ArtifactRef>, Error> {
    let bytes = std::fs::read(path).map_err(|e| io("read", path, e))?;
    let members = crate::tarball::read(&bytes)?;
    let mut by_path: BTreeMap<String, Vec<u8>> = members.into_iter().collect();
    let index_bytes = by_path
        .remove(INDEX_MEMBER)
        .ok_or_else(|| Error::State(format!("{}: no {INDEX_MEMBER} member", path.display())))?;
    let index: BundleIndex = serde_json::from_slice(&index_bytes)
        .map_err(|e| Error::State(format!("{}: malformed bundle index: {e}", path.display())))?;
    if index.version > BUNDLE_VERSION {
        return Err(Error::State(format!(
            "{}: bundle format version {} is newer than this build understands ({BUNDLE_VERSION})",
            path.display(),
            index.version
        )));
    }
    if index.entries.is_empty() {
        return Err(Error::State(format!(
            "{}: bundle index names no artifacts",
            path.display()
        )));
    }
    let expected: BTreeMap<String, &BundleEntry> = index
        .entries
        .iter()
        .map(|e| (member_path(&e.id), e))
        .collect();
    for member_name in by_path.keys() {
        if !expected.contains_key(member_name) {
            return Err(Error::State(format!(
                "{}: extra member {member_name} is not in the bundle's index; refusing the whole \
                 import",
                path.display()
            )));
        }
    }
    for (member_name, entry) in &expected {
        let bytes = by_path.get(member_name).ok_or_else(|| {
            Error::State(format!(
                "{}: the index names artifact {}, but the archive has no member {member_name}",
                path.display(),
                entry.id
            ))
        })?;
        if bytes.len() as u64 != entry.size {
            return Err(Error::State(format!(
                "{}: member {} is {} bytes, but the index says {}",
                path.display(),
                entry.id,
                bytes.len(),
                entry.size
            )));
        }
        let digest = blake3::hash(bytes).to_hex().to_string();
        if digest != entry.digest {
            return Err(Error::State(format!(
                "{}: member {}'s bytes do not match the index's digest ({digest} vs {}); \
                 tampered or corrupted, refusing the whole import",
                path.display(),
                entry.id,
                entry.digest
            )));
        }
    }
    // Every member verified against the index and its own digest: safe to
    // publish. `index.entries` order (id-sorted, from export) is kept so a
    // repeated import is stable to read back.
    let store = yard.store();
    let mut imported = Vec::with_capacity(index.entries.len());
    for entry in &index.entries {
        let bytes = &by_path[&member_path(&entry.id)];
        let tmp = TempFile(temp_path(store.dir(), "import"));
        std::fs::write(&tmp.0, bytes).map_err(|e| io("write", &tmp.0, e))?;
        let mut labels = entry.labels.clone();
        labels.insert("bundle.origin_id".into(), entry.id.clone());
        labels.insert(
            "bundle.origin_publisher".into(),
            entry.publisher_branch.clone(),
        );
        labels.insert(
            "bundle.origin_created_at".into(),
            entry.created_at.to_string(),
        );
        let artifact = storage::publish(
            yard,
            branch,
            &tmp.0,
            Some(entry.name.clone()),
            Some(entry.media_type.clone()),
            labels,
        )?;
        imported.push(artifact);
    }
    Ok(imported)
}
