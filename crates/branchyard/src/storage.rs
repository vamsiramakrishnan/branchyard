//! Shared storage for collaborating branches: immutable artifacts and
//! lease-fenced scratch areas. See `docs/storage.md`.
//!
//! **Artifacts** are content-addressed (blake3) and immutable: publishing
//! writes bytes once under their digest and records provenance
//! ([`ArtifactRef`]) in the store. **Reads follow the delegation tree**: a
//! branch reads what it published, what its ancestors published, and what
//! its descendants published; a sibling needs an explicit
//! [`share_artifact`]. [`gc_after_removal`] deletes an artifact's metadata
//! and bytes once no live branch can read it any more.
//!
//! **Scratch areas** are a named shared directory for a subtree
//! (`by scratch create NAME`), with the same read-authorization rule, and
//! **one writer at a time**: [`lock_scratch`] grants a writer lease that
//! design §7 says a store cannot use to fence an arbitrary live filesystem
//! writer. It fences honest callers only: a lock is granted, or reused by
//! its holder, or reclaimed once the holding branch's own turn has ended
//! (the branch is no longer `running`), which reuses the engine's own
//! liveness signal for the branch's lease instead of a second heartbeat.
//! Nothing stops a process that kept a file open from writing after its
//! branch's lock was reclaimed.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::{now_ms, Store};
use crate::{Error, Yard};

/// Directory artifact bytes are stored under, inside `.branchyard/` (or a
/// server's data directory).
const ARTIFACTS_DIR: &str = "artifacts";
/// Directory scratch areas live under, inside `.branchyard/`.
const SCRATCH_DIR: &str = "scratch";
/// Env var prefix a turn's authorized scratch areas are exposed under:
/// `BRANCHYARD_SCRATCH_<NAME>` (the name upper-cased).
pub const SCRATCH_ENV_PREFIX: &str = "BRANCHYARD_SCRATCH_";
/// Default limit on a published artifact's size and on an upload through
/// `--remote`, in bytes. The server's operator can raise it; see
/// `docs/server.md`.
pub const DEFAULT_ARTIFACT_LIMIT: u64 = 512 * 1024 * 1024;

/// An immutable published artifact's provenance, as `by artifact list/get`,
/// the SDK and the server report it. `id` identifies this publish; `digest`
/// is the blake3 hash (hex, lower case) of its bytes, the artifact's content
/// identity, checked on every read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: String,
    pub digest: String,
    pub size: u64,
    pub name: String,
    pub media_type: String,
    pub publisher_branch: String,
    /// The publisher's turn count when it published, from its record.
    pub turn: u64,
    pub created_at: u64,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// An artifact as the store keeps it: its provenance plus the publisher's
/// ancestor chain, snapshotted at publish time so a read grant to an
/// ancestor survives the publisher's own later removal (its own record, and
/// so its `parent` field, is gone once it is removed).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ArtifactRow {
    pub artifact: ArtifactRef,
    pub ancestry: Vec<String>,
}

/// A new artifact's provenance, before the backend assigns its `id` and
/// `created_at`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct NewArtifact {
    pub digest: String,
    pub size: u64,
    pub name: String,
    pub media_type: String,
    pub publisher_branch: String,
    pub turn: u64,
    pub labels: BTreeMap<String, String>,
    pub ancestry: Vec<String>,
}

/// A scratch area's record, as `by scratch` and the SDK report it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScratchArea {
    pub name: String,
    pub owner_branch: String,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ScratchRow {
    pub area: ScratchArea,
    pub ancestry: Vec<String>,
}

/// A scratch area's writer lock, as granted or found held.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScratchLock {
    pub name: String,
    pub holder_branch: String,
    pub acquired_at: u64,
}

/// What [`StorageBackend::scratch_lock`] found.
pub(crate) enum LockOutcome {
    /// Granted: fresh, re-granted to its current holder, or reclaimed from
    /// a holder whose turn has ended.
    Granted(ScratchLock),
    /// Held by another branch still running a turn.
    Held(ScratchLock),
}

/// Durable storage for artifacts and scratch areas: one abstraction,
/// implemented for [`crate::sqlite::Sqlite`] and [`crate::pg::Postgres`],
/// alongside their [`crate::state::Backend`] implementation and sharing its
/// connection. Kept separate from `Backend` so this feature's tables and
/// methods do not enlarge that trait.
pub(crate) trait StorageBackend: Send + Sync + fmt::Debug {
    fn create_artifact(&self, new: &NewArtifact) -> Result<ArtifactRow, Error>;
    fn artifact(&self, id: &str) -> Result<Option<ArtifactRow>, Error>;
    /// Every artifact, oldest first.
    fn artifacts(&self) -> Result<Vec<ArtifactRow>, Error>;
    fn artifact_shares(&self, id: &str) -> Result<Vec<String>, Error>;
    /// `false` if the artifact does not exist.
    fn share_artifact(&self, id: &str, branch: &str) -> Result<bool, Error>;
    fn delete_artifact(&self, id: &str) -> Result<(), Error>;
    /// How many artifact rows still reference `digest`, so its bytes are
    /// deleted only once none do.
    fn digest_refcount(&self, digest: &str) -> Result<u64, Error>;

    /// `false` if `name` is already a scratch area.
    fn create_scratch(&self, name: &str, owner: &str, ancestry: &[String]) -> Result<bool, Error>;
    fn scratch(&self, name: &str) -> Result<Option<ScratchRow>, Error>;
    fn scratch_list(&self) -> Result<Vec<ScratchRow>, Error>;
    fn scratch_shares(&self, name: &str) -> Result<Vec<String>, Error>;
    fn share_scratch(&self, name: &str, branch: &str) -> Result<bool, Error>;
    fn delete_scratch(&self, name: &str) -> Result<(), Error>;

    /// Acquire `name`'s writer lock for `branch`: granted when free, when
    /// `branch` already holds it, or when the current holder's branch is no
    /// longer running a turn (reclaimed); held otherwise. `None` if `name`
    /// is not a scratch area.
    fn scratch_lock(&self, name: &str, branch: &str) -> Result<Option<LockOutcome>, Error>;
    /// Release `name`'s lock if `branch` holds it. `false` if it did not.
    fn scratch_unlock(&self, name: &str, branch: &str) -> Result<bool, Error>;
    fn scratch_lock_state(&self, name: &str) -> Result<Option<ScratchLock>, Error>;
}

/// Whether `reader` may read something `publisher` published or owns:
/// itself, an ancestor of `publisher` captured in `ancestry` at publish
/// time (robust to `publisher`'s later removal), a live descendant of
/// `publisher` (walked from `reader` upward by recorded name, so it needs
/// no record of `publisher` itself to still exist), or an explicit share.
pub(crate) fn readable(
    store: &Store,
    reader: &str,
    publisher: &str,
    ancestry: &[String],
    shares: &[String],
) -> bool {
    reader == publisher
        || ancestry.iter().any(|a| a == reader)
        || shares.iter().any(|s| s == reader)
        || is_descendant(store, publisher, reader)
}

/// Whether `descendant` descends from `ancestor`, walking `descendant`'s
/// recorded parent names upward. Stops, answering false, at the first
/// ancestor whose own record no longer exists; it never needs `ancestor`'s.
fn is_descendant(store: &Store, ancestor: &str, descendant: &str) -> bool {
    let mut cur = descendant.to_owned();
    loop {
        if cur == ancestor {
            return true;
        }
        let Ok(record) = store.read(&cur) else {
            return false;
        };
        match record.info.parent {
            Some(parent) => cur = parent,
            None => return false,
        }
    }
}

/// `branch`'s ancestor chain, oldest first, walking live records upward
/// from its recorded parent. Stops at the first ancestor already removed;
/// what it collected up to there is still correct.
pub(crate) fn ancestry_of(store: &Store, branch: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut cur = branch.to_owned();
    loop {
        let Ok(record) = store.read(&cur) else {
            break;
        };
        match record.info.parent {
            Some(parent) => {
                chain.push(parent.clone());
                cur = parent;
            }
            None => break,
        }
    }
    chain.reverse();
    chain
}

fn denied_unknown(kind: &str, id: &str) -> Error {
    Error::Denied(format!("no {kind} named {id}"))
}

fn denied_read(kind: &str, reader: &str, id: &str, publisher: &str) -> Error {
    Error::Denied(format!(
        "{reader} may not read {kind} {id}, published by {publisher}: it is not an ancestor or \
         descendant of {publisher} and it was not given an explicit share"
    ))
}

/// Where artifact bytes for `digest` are stored under `dir` (a
/// `.branchyard` directory, or a server's data directory).
fn blob_path(dir: &Path, digest: &str) -> PathBuf {
    let prefix = &digest[..digest.len().min(2)];
    dir.join(ARTIFACTS_DIR).join(prefix).join(digest)
}

/// Publish the bytes at `path` as a new artifact of `branch`. Copies them
/// once into content-addressed storage under their blake3 digest, and
/// records provenance whether or not another artifact already has the same
/// digest (bytes are kept until no artifact references them; see
/// [`gc_after_removal`]).
pub(crate) fn publish(
    yard: &Yard,
    branch: &str,
    path: &Path,
    name: Option<String>,
    media_type: Option<String>,
    labels: BTreeMap<String, String>,
) -> Result<ArtifactRef, Error> {
    let store = yard.store();
    let record = store.read(branch)?;
    let mut file = std::fs::File::open(path)
        .map_err(|e| Error::State(format!("open {}: {e}", path.display())))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size: u64 = 0;
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| Error::State(format!("read {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    let digest = hasher.finalize().to_hex().to_string();
    let dest = blob_path(store.dir(), &digest);
    if !dest.exists() {
        let dir = dest.parent().expect("blob_path has a parent");
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::State(format!("create {}: {e}", dir.display())))?;
        let temp = dir.join(format!(".{digest}.{}.tmp", now_ms()));
        std::fs::copy(path, &temp)
            .map_err(|e| Error::State(format!("copy {}: {e}", path.display())))?;
        // Immutable once written: never opened for writing again.
        let mut perms = std::fs::metadata(&temp)
            .map_err(|e| Error::State(format!("stat {}: {e}", temp.display())))?
            .permissions();
        perms.set_readonly(true);
        let _ = std::fs::set_permissions(&temp, perms);
        std::fs::rename(&temp, &dest)
            .map_err(|e| Error::State(format!("install {}: {e}", dest.display())))?;
    }
    let name = name.unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| digest.clone())
    });
    let row = store.storage().create_artifact(&NewArtifact {
        digest,
        size,
        name,
        media_type: media_type.unwrap_or_else(|| "application/octet-stream".into()),
        publisher_branch: branch.to_owned(),
        turn: record.info.turns as u64,
        labels,
        ancestry: ancestry_of(&store, branch),
    })?;
    Ok(row.artifact)
}

/// Every artifact `reader` may read: what it published, what its ancestors
/// or descendants published, and what was explicitly shared to it.
pub(crate) fn list(yard: &Yard, reader: &str) -> Result<Vec<ArtifactRef>, Error> {
    let store = yard.store();
    store.read(reader)?; // a known branch
    let storage = store.storage();
    let mut found = Vec::new();
    for row in storage.artifacts()? {
        let shares = storage.artifact_shares(&row.artifact.id)?;
        if readable(
            &store,
            reader,
            &row.artifact.publisher_branch,
            &row.ancestry,
            &shares,
        ) {
            found.push(row.artifact);
        }
    }
    Ok(found)
}

/// Copy artifact `id`'s bytes to `out` for `reader`, checked against the
/// recorded digest, and return its provenance. Refused unless `reader` may
/// read it.
pub(crate) fn get(yard: &Yard, reader: &str, id: &str, out: &Path) -> Result<ArtifactRef, Error> {
    let store = yard.store();
    store.read(reader)?;
    let storage = store.storage();
    let row = storage
        .artifact(id)?
        .ok_or_else(|| denied_unknown("artifact", id))?;
    let shares = storage.artifact_shares(id)?;
    if !readable(
        &store,
        reader,
        &row.artifact.publisher_branch,
        &row.ancestry,
        &shares,
    ) {
        return Err(denied_read(
            "artifact",
            reader,
            id,
            &row.artifact.publisher_branch,
        ));
    }
    let src = blob_path(store.dir(), &row.artifact.digest);
    let bytes = std::fs::read(&src)
        .map_err(|e| Error::State(format!("read artifact {id}: {}: {e}", src.display())))?;
    if bytes.len() as u64 != row.artifact.size
        || blake3::hash(&bytes).to_hex().as_str() != row.artifact.digest
    {
        return Err(Error::State(format!(
            "artifact {id}'s stored bytes no longer match its recorded digest"
        )));
    }
    if let Some(dir) = out.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::State(format!("create {}: {e}", dir.display())))?;
        }
    }
    let mut file = std::fs::File::create(out)
        .map_err(|e| Error::State(format!("create {}: {e}", out.display())))?;
    file.write_all(&bytes)
        .map_err(|e| Error::State(format!("write {}: {e}", out.display())))?;
    Ok(row.artifact)
}

/// Share artifact `id`, published or already shared to `actor`, with
/// `to`: the explicit grant siblings need. `actor` must itself be able to
/// read it (the common parent shares to authorize a sibling read).
pub(crate) fn share_artifact(yard: &Yard, actor: &str, id: &str, to: &str) -> Result<(), Error> {
    let store = yard.store();
    store.read(actor)?;
    store.read(to)?;
    let storage = store.storage();
    let row = storage
        .artifact(id)?
        .ok_or_else(|| denied_unknown("artifact", id))?;
    let shares = storage.artifact_shares(id)?;
    if !readable(
        &store,
        actor,
        &row.artifact.publisher_branch,
        &row.ancestry,
        &shares,
    ) {
        return Err(denied_read(
            "artifact",
            actor,
            id,
            &row.artifact.publisher_branch,
        ));
    }
    storage.share_artifact(id, to)?;
    Ok(())
}

/// After `removed` is deleted: an artifact or scratch area it published or
/// owns is deleted too, bytes and metadata, unless another live branch can
/// still read it (an ancestor, a live descendant, or an explicit share).
/// "Artifacts survive branch removal only if referenced by a live branch"
/// (`docs/design.md` §7).
pub(crate) fn gc_after_removal(store: &Store, removed: &str) -> Result<(), Error> {
    let storage = store.storage();
    let live: Vec<String> = store.list()?.into_iter().map(|r| r.info.name).collect();
    let still_reachable = |publisher: &str, ancestry: &[String], shares: &[String]| {
        live.iter()
            .any(|reader| reader != removed && readable(store, reader, publisher, ancestry, shares))
    };
    for row in storage.artifacts()? {
        if row.artifact.publisher_branch != removed {
            continue;
        }
        let shares = storage.artifact_shares(&row.artifact.id)?;
        if still_reachable(&row.artifact.publisher_branch, &row.ancestry, &shares) {
            continue;
        }
        storage.delete_artifact(&row.artifact.id)?;
        if storage.digest_refcount(&row.artifact.digest)? == 0 {
            let path = blob_path(store.dir(), &row.artifact.digest);
            let _ = std::fs::remove_file(&path);
        }
    }
    for row in storage.scratch_list()? {
        if row.area.owner_branch != removed {
            continue;
        }
        let shares = storage.scratch_shares(&row.area.name)?;
        if still_reachable(&row.area.owner_branch, &row.ancestry, &shares) {
            continue;
        }
        storage.delete_scratch(&row.area.name)?;
        let _ = std::fs::remove_dir_all(scratch_dir(store, &row.area.name));
    }
    Ok(())
}

/// A usable scratch area name: lowercase `[a-z0-9-]`, starting with a
/// letter, at most 48 characters (kept short: it becomes an environment
/// variable suffix and a directory and mount name).
pub(crate) fn validate_scratch_name(name: &str) -> Result<(), Error> {
    let ok = !name.is_empty()
        && name.len() <= 48
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    match ok {
        true => Ok(()),
        false => Err(Error::Denied(format!(
            "{name:?} is not a usable scratch area name: lowercase letters, digits and `-`, \
             starting with a letter, at most 48 characters"
        ))),
    }
}

/// Where scratch area `name` lives on disk.
pub(crate) fn scratch_dir(store: &Store, name: &str) -> PathBuf {
    store.dir().join(SCRATCH_DIR).join(name)
}

/// The environment variable a turn's authorized scratch areas are exposed
/// under.
pub(crate) fn scratch_env_var(name: &str) -> String {
    format!(
        "{SCRATCH_ENV_PREFIX}{}",
        name.to_uppercase().replace('-', "_")
    )
}

/// Create scratch area `name`, owned by `owner`.
pub(crate) fn create_scratch(yard: &Yard, owner: &str, name: &str) -> Result<ScratchArea, Error> {
    validate_scratch_name(name)?;
    let store = yard.store();
    store.read(owner)?;
    let ancestry = ancestry_of(&store, owner);
    if !store.storage().create_scratch(name, owner, &ancestry)? {
        return Err(Error::Denied(format!("scratch area {name} already exists")));
    }
    let dir = scratch_dir(&store, name);
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::State(format!("create {}: {e}", dir.display())))?;
    Ok(ScratchArea {
        name: name.to_owned(),
        owner_branch: owner.to_owned(),
        created_at: now_ms() / 1000,
    })
}

/// Every scratch area `reader` may reach.
pub(crate) fn authorized_scratch(yard: &Yard, reader: &str) -> Result<Vec<ScratchArea>, Error> {
    let store = yard.store();
    let storage = store.storage();
    let mut found = Vec::new();
    for row in storage.scratch_list()? {
        let shares = storage.scratch_shares(&row.area.name)?;
        if readable(
            &store,
            reader,
            &row.area.owner_branch,
            &row.ancestry,
            &shares,
        ) {
            found.push(row.area);
        }
    }
    Ok(found)
}

/// Share scratch area `name`, owned or already shared to `actor`, with
/// `to`.
pub(crate) fn share_scratch(yard: &Yard, actor: &str, name: &str, to: &str) -> Result<(), Error> {
    let store = yard.store();
    store.read(actor)?;
    store.read(to)?;
    let storage = store.storage();
    let row = storage
        .scratch(name)?
        .ok_or_else(|| denied_unknown("scratch area", name))?;
    let shares = storage.scratch_shares(name)?;
    if !readable(
        &store,
        actor,
        &row.area.owner_branch,
        &row.ancestry,
        &shares,
    ) {
        return Err(denied_read(
            "scratch area",
            actor,
            name,
            &row.area.owner_branch,
        ));
    }
    storage.share_scratch(name, to)?;
    Ok(())
}

/// Acquire scratch area `name`'s writer lock for `branch`. Refused unless
/// `branch` may reach it; refused with [`Error::Running`] naming the
/// current holder while its turn is still running.
pub(crate) fn lock_scratch(yard: &Yard, branch: &str, name: &str) -> Result<ScratchLock, Error> {
    let store = yard.store();
    store.read(branch)?;
    let storage = store.storage();
    let row = storage
        .scratch(name)?
        .ok_or_else(|| denied_unknown("scratch area", name))?;
    let shares = storage.scratch_shares(name)?;
    if !readable(
        &store,
        branch,
        &row.area.owner_branch,
        &row.ancestry,
        &shares,
    ) {
        return Err(denied_read(
            "scratch area",
            branch,
            name,
            &row.area.owner_branch,
        ));
    }
    match storage.scratch_lock(name, branch)? {
        Some(LockOutcome::Granted(lock)) => Ok(lock),
        Some(LockOutcome::Held(lock)) => Err(Error::Running(format!(
            "scratch area {name} (held by {})",
            lock.holder_branch
        ))),
        None => Err(denied_unknown("scratch area", name)),
    }
}

/// Release scratch area `name`'s lock if `branch` holds it.
pub(crate) fn unlock_scratch(yard: &Yard, branch: &str, name: &str) -> Result<(), Error> {
    let store = yard.store();
    store.read(branch)?;
    store.storage().scratch_unlock(name, branch)?;
    Ok(())
}

/// The current holder of scratch area `name`'s lock, if any, whether or
/// not its turn is still running.
pub(crate) fn scratch_lock_state(yard: &Yard, name: &str) -> Result<Option<ScratchLock>, Error> {
    yard.store().storage().scratch_lock_state(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Store;
    use crate::{BranchInfo, BranchStatus};
    use std::path::PathBuf;

    struct Temp(PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_store(name: &str) -> (Temp, Store) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "branchyard-storage-unit-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let store = Store::open(&dir).unwrap();
        (Temp(dir), store)
    }

    fn record(name: &str, parent: Option<&str>) -> crate::state::Record {
        crate::state::Record {
            info: BranchInfo {
                name: name.into(),
                git_branch: format!("by/{name}"),
                worktree: PathBuf::from("/w"),
                prompt: "p".into(),
                harness: "h".into(),
                profile: "p".into(),
                session: None,
                parent: parent.map(str::to_owned),
                children: Vec::new(),
                depth: 0,
                base: "b".into(),
                candidate: None,
                status: BranchStatus::Ready,
                turns: 1,
                cost_usd: None,
                created_at: 0,
                stalled: false,
                superseded_by: None,
            },
            created_ms: 0,
            check: None,
            command: None,
            home: None,
            cost_baseline: None,
            provider: None,
            grant: None,
            provision: None,
        }
    }

    #[test]
    fn ancestor_and_descendant_reads_are_allowed_siblings_are_not() {
        let (_t, store) = temp_store("grants");
        store.write(&record("root", None)).unwrap();
        store.write(&record("a", Some("root"))).unwrap();
        store.write(&record("b", Some("root"))).unwrap();
        store.write(&record("aa", Some("a"))).unwrap();
        let ancestry = ancestry_of(&store, "aa");
        assert_eq!(ancestry, vec!["root".to_owned(), "a".to_owned()]);
        // Ancestors of the publisher (aa) read what it published.
        assert!(readable(&store, "root", "aa", &ancestry, &[]));
        assert!(readable(&store, "a", "aa", &ancestry, &[]));
        // A descendant of the publisher (a) reads what it published, found
        // live without needing a's own ancestry snapshot.
        assert!(readable(&store, "aa", "a", &ancestry_of(&store, "a"), &[]));
        // A sibling does not, until shared.
        assert!(!readable(&store, "b", "aa", &ancestry, &[]));
        assert!(readable(&store, "b", "aa", &ancestry, &["b".into()]));
    }

    #[test]
    fn ancestry_survives_the_publishers_removal() {
        let (_t, store) = temp_store("gc-ancestry");
        store.write(&record("root", None)).unwrap();
        store.write(&record("a", Some("root"))).unwrap();
        let ancestry = ancestry_of(&store, "a");
        store.delete("a").unwrap();
        // root can still read what a published, even though a is gone.
        assert!(readable(&store, "root", "a", &ancestry, &[]));
    }

    #[test]
    fn scratch_names_are_validated() {
        assert!(validate_scratch_name("shared-cache").is_ok());
        assert!(validate_scratch_name("Shared").is_err());
        assert!(validate_scratch_name("1x").is_err());
        assert!(validate_scratch_name("").is_err());
    }
}
