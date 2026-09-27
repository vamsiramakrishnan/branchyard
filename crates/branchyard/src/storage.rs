//! Shared storage for collaborating branches: immutable artifacts and
//! lease-fenced scratch areas. See `docs/storage.md`.
//!
//! **Artifacts** are content-addressed (blake3) and immutable: publishing
//! writes bytes once under their digest and records provenance
//! ([`ArtifactRef`]) in the store. **Reads follow the delegation tree**: a
//! branch reads what it published, what its ancestors published, and what
//! its descendants published; a sibling needs an explicit
//! [`share_artifact`]. Every grant is bound to a branch's **incarnation**,
//! the store's never-reused identity for one created branch, not to its
//! name: a name is free again once its branch is removed, and a new branch
//! that takes it inherits nothing ([`Lineage`]). [`gc_after_removal`]
//! deletes an artifact's metadata and bytes once no live branch can read it
//! any more.
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

use std::collections::{BTreeMap, HashMap, HashSet};
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
/// so its `parent` field, is gone once it is removed). Names are for
/// display; grants are decided on the incarnations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ArtifactRow {
    pub artifact: ArtifactRef,
    pub ancestry: Vec<String>,
    /// The publisher's incarnation. `None` only for a row published before
    /// grants were bound to identities whose publisher could not be bound
    /// when the store was upgraded (see `docs/storage.md`).
    pub publisher_incarnation: Option<i64>,
    /// The incarnations of the publisher's ancestors at publish time.
    pub ancestry_incarnations: Vec<i64>,
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
    pub publisher_incarnation: i64,
    pub ancestry_incarnations: Vec<i64>,
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
    /// As [`ArtifactRow::publisher_incarnation`], for the owner.
    pub owner_incarnation: Option<i64>,
    pub ancestry_incarnations: Vec<i64>,
}

/// A new scratch area, before the backend records its creation time.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NewScratch {
    pub name: String,
    pub owner: String,
    pub owner_incarnation: i64,
    pub ancestry: Vec<String>,
    pub ancestry_incarnations: Vec<i64>,
}

/// One explicit share of an artifact or scratch area: the target's name,
/// for display, and its incarnation, which the grant is bound to (`None`
/// for a share recorded before grants were bound to identities whose target
/// could not be bound on upgrade; it grants nothing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Share {
    pub branch: String,
    pub incarnation: Option<i64>,
}

/// One created branch's identity, as [`StorageBackend::identities`] lists
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub name: String,
    pub incarnation: i64,
    /// The parent's incarnation, bound when the branch's record was first
    /// written with a parent (the parent is alive then: it is creating the
    /// child), so it survives the parent's removal.
    pub parent_incarnation: Option<i64>,
    /// The parent's name from the record, for a branch whose parent was
    /// never bound (a record written directly, or before binding existed).
    pub parent: Option<String>,
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
    /// Every created branch (not a bare reservation) with its incarnation
    /// and its parent's: what grants are decided on ([`Lineage`]).
    fn identities(&self) -> Result<Vec<Identity>, Error>;

    fn create_artifact(&self, new: &NewArtifact) -> Result<ArtifactRow, Error>;
    fn artifact(&self, id: &str) -> Result<Option<ArtifactRow>, Error>;
    /// Every artifact, oldest first.
    fn artifacts(&self) -> Result<Vec<ArtifactRow>, Error>;
    fn artifact_shares(&self, id: &str) -> Result<Vec<Share>, Error>;
    /// Share with `branch`, whose current incarnation is `incarnation`; a
    /// share to an earlier holder of the name is replaced. `false` if the
    /// artifact does not exist.
    fn share_artifact(&self, id: &str, branch: &str, incarnation: i64) -> Result<bool, Error>;
    fn delete_artifact(&self, id: &str) -> Result<(), Error>;
    /// How many artifact rows still reference `digest`, so its bytes are
    /// deleted only once none do.
    fn digest_refcount(&self, digest: &str) -> Result<u64, Error>;

    /// `false` if `name` is already a scratch area.
    fn create_scratch(&self, new: &NewScratch) -> Result<bool, Error>;
    fn scratch(&self, name: &str) -> Result<Option<ScratchRow>, Error>;
    fn scratch_list(&self) -> Result<Vec<ScratchRow>, Error>;
    fn scratch_shares(&self, name: &str) -> Result<Vec<Share>, Error>;
    /// As [`StorageBackend::share_artifact`].
    fn share_scratch(&self, name: &str, branch: &str, incarnation: i64) -> Result<bool, Error>;
    fn delete_scratch(&self, name: &str) -> Result<(), Error>;

    /// Acquire `name`'s writer lock for `branch` at `incarnation`: granted
    /// when free, when that same incarnation already holds it, or when the
    /// holding incarnation is no longer running a turn, removed included
    /// (reclaimed); held otherwise. `None` if `name` is not a scratch area.
    fn scratch_lock(
        &self,
        name: &str,
        branch: &str,
        incarnation: i64,
    ) -> Result<Option<LockOutcome>, Error>;
    /// Release `name`'s lock if `incarnation` holds it. `false` if it did
    /// not.
    fn scratch_unlock(&self, name: &str, incarnation: i64) -> Result<bool, Error>;
    fn scratch_lock_state(&self, name: &str) -> Result<Option<ScratchLock>, Error>;
}

/// Who is who, for deciding grants: every created branch's name and
/// incarnation, and its parent's incarnation, read once from the store.
///
/// A parent is the one bound when the child's record was first written
/// ([`Identity::parent_incarnation`]); failing that, the live branch
/// holding the recorded parent name, but only if it is older than the child
/// (a lower incarnation): a parent always exists before its child, so a
/// younger holder of the name took it after the real parent was removed.
/// Incarnations only grow, so parent links only point to lower ones and the
/// walk upward always ends.
#[derive(Debug, Default)]
pub(crate) struct Lineage {
    ids: HashMap<String, i64>,
    live: HashSet<i64>,
    /// Live incarnation -> its parent's incarnation, which may be removed.
    parents: HashMap<i64, i64>,
}

impl Lineage {
    pub(crate) fn load(store: &Store) -> Result<Lineage, Error> {
        Ok(Lineage::from_identities(store.storage().identities()?))
    }

    pub(crate) fn from_identities(identities: Vec<Identity>) -> Lineage {
        let ids: HashMap<String, i64> = identities
            .iter()
            .map(|i| (i.name.clone(), i.incarnation))
            .collect();
        let parents = identities
            .iter()
            .filter_map(|i| {
                let parent = i.parent_incarnation.or_else(|| {
                    let named = ids.get(i.parent.as_deref()?)?;
                    Some(*named)
                })?;
                (parent < i.incarnation).then_some((i.incarnation, parent))
            })
            .collect();
        let live = ids.values().copied().collect();
        Lineage { ids, live, parents }
    }

    /// The incarnation of the live branch named `name`.
    pub(crate) fn id(&self, name: &str) -> Result<i64, Error> {
        self.ids
            .get(name)
            .copied()
            .ok_or_else(|| Error::UnknownBranch(name.to_owned()))
    }

    fn live(&self, incarnation: i64) -> bool {
        self.live.contains(&incarnation)
    }

    /// Whether `descendant` is `ancestor` or descends from it, walking up
    /// from `descendant` through live branches; the walk may end on a
    /// removed parent, so `ancestor` itself need not be live.
    pub(crate) fn descends(&self, ancestor: i64, descendant: i64) -> bool {
        let mut cur = descendant;
        loop {
            if cur == ancestor {
                return true;
            }
            match self.parents.get(&cur) {
                Some(parent) => cur = *parent,
                None => return false,
            }
        }
    }

    /// `incarnation`'s ancestors, oldest first, as far up as the walk goes.
    fn ancestors(&self, incarnation: i64) -> Vec<i64> {
        let mut chain = Vec::new();
        let mut cur = incarnation;
        while let Some(parent) = self.parents.get(&cur) {
            chain.push(*parent);
            cur = *parent;
        }
        chain.reverse();
        chain
    }

    /// Every incarnation some live branch is, or descends from: an object
    /// published or owned by one of these is readable by a live branch
    /// through the descendant rule (or as its own).
    fn upheld(&self) -> HashSet<i64> {
        let mut upheld = HashSet::new();
        for id in &self.live {
            upheld.insert(*id);
            upheld.extend(self.ancestors(*id));
        }
        upheld
    }
}

/// What an artifact or scratch area grants reads to, by identity.
pub(crate) struct Grants<'a> {
    /// The publisher or owner.
    pub owner: Option<i64>,
    /// Its ancestors when it published or created.
    pub ancestry: &'a [i64],
    pub shares: &'a [Share],
}

impl Grants<'_> {
    fn of_artifact<'a>(row: &'a ArtifactRow, shares: &'a [Share]) -> Grants<'a> {
        Grants {
            owner: row.publisher_incarnation,
            ancestry: &row.ancestry_incarnations,
            shares,
        }
    }

    fn of_scratch<'a>(row: &'a ScratchRow, shares: &'a [Share]) -> Grants<'a> {
        Grants {
            owner: row.owner_incarnation,
            ancestry: &row.ancestry_incarnations,
            shares,
        }
    }

    /// Whether the branch at incarnation `reader` may read it: it is the
    /// owner, an ancestor of the owner captured at publish time (robust to
    /// the owner's later removal), a descendant of the owner (walked up
    /// from `reader`, so it needs no record of the owner to still exist),
    /// or the target of an explicit share. Identities only: a branch that
    /// took a removed branch's name is a different incarnation.
    pub(crate) fn readable(&self, lineage: &Lineage, reader: i64) -> bool {
        self.owner == Some(reader)
            || self.ancestry.contains(&reader)
            || self.shares.iter().any(|s| s.incarnation == Some(reader))
            || self.owner.is_some_and(|o| lineage.descends(o, reader))
    }

    /// Whether any live branch may read it.
    fn reachable(&self, lineage: &Lineage, upheld: &HashSet<i64>) -> bool {
        self.owner.is_some_and(|o| upheld.contains(&o))
            || self.ancestry.iter().any(|a| lineage.live(*a))
            || self
                .shares
                .iter()
                .any(|s| s.incarnation.is_some_and(|i| lineage.live(i)))
    }
}

/// `branch`'s ancestor chain by name, oldest first, walking live records
/// upward from its recorded parent: for display only. Stops at the first
/// ancestor already removed; what it collected up to there is still
/// correct.
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

/// A temporary upload file, removed on drop unless it was installed.
struct TempBlob(Option<PathBuf>);

impl Drop for TempBlob {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Copy the bytes readable from `path` once, hashing them as they are
/// written to a temporary file under `dir`'s artifact directory, and
/// install exactly that file at its content address. Returns the digest
/// and size. Opening `path` once and never again means the stored bytes are
/// the bytes that were hashed, whatever happens to `path` meanwhile.
fn store_blob(dir: &Path, path: &Path) -> Result<(String, u64), Error> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let io = |what: &str, p: &Path, e: std::io::Error| {
        Error::State(format!("{what} {}: {e}", p.display()))
    };
    let mut file = std::fs::File::open(path).map_err(|e| io("open", path, e))?;
    let root = dir.join(ARTIFACTS_DIR);
    std::fs::create_dir_all(&root).map_err(|e| io("create", &root, e))?;
    let temp_path = root.join(format!(
        ".upload.{}.{}.{}.tmp",
        std::process::id(),
        now_ms(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|e| io("create", &temp_path, e))?;
    let mut temp = TempBlob(Some(temp_path.clone()));
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size: u64 = 0;
    loop {
        let n = file.read(&mut buf).map_err(|e| io("read", path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])
            .map_err(|e| io("write", &temp_path, e))?;
        size += n as u64;
    }
    out.sync_all().map_err(|e| io("sync", &temp_path, e))?;
    drop(out);
    let digest = hasher.finalize().to_hex().to_string();
    let dest = blob_path(dir, &digest);
    // Content-addressed: an existing blob of the right size holds these
    // bytes already (reads re-check the digest); one of another size is
    // damaged and is replaced by this one.
    if std::fs::metadata(&dest).is_ok_and(|m| m.len() == size) {
        return Ok((digest, size));
    }
    let parent = dest.parent().expect("blob_path has a parent");
    std::fs::create_dir_all(parent).map_err(|e| io("create", parent, e))?;
    // Immutable once written: never opened for writing again.
    if let Ok(meta) = std::fs::metadata(&temp_path) {
        let mut perms = meta.permissions();
        perms.set_readonly(true);
        let _ = std::fs::set_permissions(&temp_path, perms);
    }
    std::fs::rename(&temp_path, &dest).map_err(|e| io("install", &dest, e))?;
    temp.0 = None;
    Ok((digest, size))
}

/// Publish the bytes at `path` as a new artifact of `branch`. Reads them
/// once, hashing them into content-addressed storage under their blake3
/// digest, and records provenance whether or not another artifact already
/// has the same digest (bytes are kept until no artifact references them;
/// see [`gc_after_removal`]).
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
    let lineage = Lineage::load(&store)?;
    let publisher = lineage.id(branch)?;
    let (digest, size) = store_blob(store.dir(), path)?;
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
        publisher_incarnation: publisher,
        ancestry_incarnations: lineage.ancestors(publisher),
    })?;
    Ok(row.artifact)
}

/// Every artifact `reader` may read: what it published, what its ancestors
/// or descendants published, and what was explicitly shared to it.
pub(crate) fn list(yard: &Yard, reader: &str) -> Result<Vec<ArtifactRef>, Error> {
    let store = yard.store();
    store.read(reader)?; // a known branch
    let lineage = Lineage::load(&store)?;
    let me = lineage.id(reader)?;
    let storage = store.storage();
    let mut found = Vec::new();
    for row in storage.artifacts()? {
        let shares = storage.artifact_shares(&row.artifact.id)?;
        if Grants::of_artifact(&row, &shares).readable(&lineage, me) {
            found.push(row.artifact);
        }
    }
    Ok(found)
}

/// The artifact `id` if `reader` may read it.
fn readable_artifact(store: &Store, reader: &str, id: &str) -> Result<ArtifactRow, Error> {
    store.read(reader)?;
    let lineage = Lineage::load(store)?;
    let me = lineage.id(reader)?;
    let storage = store.storage();
    let row = storage
        .artifact(id)?
        .ok_or_else(|| denied_unknown("artifact", id))?;
    let shares = storage.artifact_shares(id)?;
    if !Grants::of_artifact(&row, &shares).readable(&lineage, me) {
        return Err(denied_read(
            "artifact",
            reader,
            id,
            &row.artifact.publisher_branch,
        ));
    }
    Ok(row)
}

/// Copy artifact `id`'s bytes to `out` for `reader`, checked against the
/// recorded digest, and return its provenance. Refused unless `reader` may
/// read it.
pub(crate) fn get(yard: &Yard, reader: &str, id: &str, out: &Path) -> Result<ArtifactRef, Error> {
    let store = yard.store();
    let row = readable_artifact(&store, reader, id)?;
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
/// `to`: the explicit grant siblings need, bound to `to`'s current
/// incarnation. `actor` must itself be able to read it (the common parent
/// shares to authorize a sibling read).
pub(crate) fn share_artifact(yard: &Yard, actor: &str, id: &str, to: &str) -> Result<(), Error> {
    let store = yard.store();
    store.read(to)?;
    readable_artifact(&store, actor, id)?;
    let target = Lineage::load(&store)?.id(to)?;
    store.storage().share_artifact(id, to, target)?;
    Ok(())
}

/// Delete every artifact and scratch area no live branch can read any
/// more, and artifact bytes no remaining row references. Run after each
/// removal. "Artifacts survive branch removal only if referenced by a live
/// branch" (`docs/design.md` §7).
///
/// Every row is re-examined, not only the removed branch's own: removing an
/// ancestor can strand what a descendant published long ago, removed
/// earlier while that ancestor kept it alive. One pass is a read of the
/// branch identities plus a set lookup per grant, so it costs a scan of the
/// repository's artifact and scratch rows per removal; it also finishes the
/// work of a removal whose collection was cut short. Rows are read before
/// the identities, so a row published by a branch created after the
/// identities were read cannot be in the scan.
pub(crate) fn gc_after_removal(store: &Store) -> Result<(), Error> {
    let storage = store.storage();
    let artifacts = storage.artifacts()?;
    let scratch = storage.scratch_list()?;
    let lineage = Lineage::load(store)?;
    let upheld = lineage.upheld();
    for row in artifacts {
        let shares = storage.artifact_shares(&row.artifact.id)?;
        if Grants::of_artifact(&row, &shares).reachable(&lineage, &upheld) {
            continue;
        }
        storage.delete_artifact(&row.artifact.id)?;
        if storage.digest_refcount(&row.artifact.digest)? == 0 {
            let path = blob_path(store.dir(), &row.artifact.digest);
            let _ = std::fs::remove_file(&path);
        }
    }
    for row in scratch {
        let shares = storage.scratch_shares(&row.area.name)?;
        if Grants::of_scratch(&row, &shares).reachable(&lineage, &upheld) {
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
    let lineage = Lineage::load(&store)?;
    let id = lineage.id(owner)?;
    let created = store.storage().create_scratch(&NewScratch {
        name: name.to_owned(),
        owner: owner.to_owned(),
        owner_incarnation: id,
        ancestry: ancestry_of(&store, owner),
        ancestry_incarnations: lineage.ancestors(id),
    })?;
    if !created {
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
    let lineage = Lineage::load(&store)?;
    let me = lineage.id(reader)?;
    let storage = store.storage();
    let mut found = Vec::new();
    for row in storage.scratch_list()? {
        let shares = storage.scratch_shares(&row.area.name)?;
        if Grants::of_scratch(&row, &shares).readable(&lineage, me) {
            found.push(row.area);
        }
    }
    Ok(found)
}

/// `branch`'s incarnation, if it may reach scratch area `name`.
fn reachable_scratch(store: &Store, branch: &str, name: &str) -> Result<i64, Error> {
    store.read(branch)?;
    let lineage = Lineage::load(store)?;
    let me = lineage.id(branch)?;
    let storage = store.storage();
    let row = storage
        .scratch(name)?
        .ok_or_else(|| denied_unknown("scratch area", name))?;
    let shares = storage.scratch_shares(name)?;
    if !Grants::of_scratch(&row, &shares).readable(&lineage, me) {
        return Err(denied_read(
            "scratch area",
            branch,
            name,
            &row.area.owner_branch,
        ));
    }
    Ok(me)
}

/// Fail unless `branch` may reach scratch area `name`.
pub(crate) fn check_readable(yard: &Yard, branch: &str, name: &str) -> Result<(), Error> {
    reachable_scratch(&yard.store(), branch, name).map(|_| ())
}

/// Share scratch area `name`, owned or already shared to `actor`, with
/// `to`, bound to `to`'s current incarnation.
pub(crate) fn share_scratch(yard: &Yard, actor: &str, name: &str, to: &str) -> Result<(), Error> {
    let store = yard.store();
    store.read(to)?;
    reachable_scratch(&store, actor, name)?;
    let target = Lineage::load(&store)?.id(to)?;
    store.storage().share_scratch(name, to, target)?;
    Ok(())
}

/// Acquire scratch area `name`'s writer lock for `branch`. Refused unless
/// `branch` may reach it; refused with [`Error::Running`] naming the
/// current holder while its turn is still running.
pub(crate) fn lock_scratch(yard: &Yard, branch: &str, name: &str) -> Result<ScratchLock, Error> {
    let store = yard.store();
    let me = reachable_scratch(&store, branch, name)?;
    match store.storage().scratch_lock(name, branch, me)? {
        Some(LockOutcome::Granted(lock)) => Ok(lock),
        Some(LockOutcome::Held(lock)) => Err(Error::Running(format!(
            "scratch area {name} (held by {})",
            lock.holder_branch
        ))),
        None => Err(denied_unknown("scratch area", name)),
    }
}

/// Release scratch area `name`'s lock if `branch`, this incarnation of it,
/// holds it.
pub(crate) fn unlock_scratch(yard: &Yard, branch: &str, name: &str) -> Result<(), Error> {
    let store = yard.store();
    store.read(branch)?;
    let me = Lineage::load(&store)?.id(branch)?;
    store.storage().scratch_unlock(name, me)?;
    Ok(())
}

/// The current holder of scratch area `name`'s lock, if any, whether or
/// not its turn is still running.
pub(crate) fn scratch_lock_state(yard: &Yard, name: &str) -> Result<Option<ScratchLock>, Error> {
    yard.store().storage().scratch_lock_state(name)
}

/// A created branch, as the upgrade to identity-bound grants (store schema
/// 2) sees it.
pub(crate) struct LegacyBranch {
    pub name: String,
    pub incarnation: i64,
    pub created_ms: u64,
    pub parent: Option<String>,
}

/// Binds the name-only grants a schema 1 store recorded to incarnations,
/// once, in the upgrade's own transaction. The rule, deny by default:
///
/// - a grant binds to the branch holding the name at upgrade time, and to
///   no one if no branch does;
/// - a publisher, owner, ancestor or lock holder binds only if that branch
///   was created no later than the artifact, area or lock (it had to exist
///   then), so a name already reused before the upgrade binds nothing;
/// - a share binds to the current holder of its target's name (shares
///   recorded no time; this is the one grant the upgrade cannot check);
/// - a branch's parent binds only to an older (lower incarnation) holder
///   of its recorded parent name.
///
/// What binds nothing grants nothing afterwards: such a row is readable
/// only through the grants that did bind, and is collected at the next
/// removal if none did.
pub(crate) struct LegacyBinder {
    by_name: HashMap<String, (i64, u64)>,
}

impl LegacyBinder {
    pub(crate) fn new(branches: &[LegacyBranch]) -> LegacyBinder {
        LegacyBinder {
            by_name: branches
                .iter()
                .map(|b| (b.name.clone(), (b.incarnation, b.created_ms)))
                .collect(),
        }
    }

    /// The incarnation holding `name`, if it was created no later than
    /// `not_after_ms` (when given).
    pub(crate) fn bind(&self, name: &str, not_after_ms: Option<u64>) -> Option<i64> {
        let (incarnation, created_ms) = self.by_name.get(name)?;
        not_after_ms
            .is_none_or(|t| *created_ms <= t)
            .then_some(*incarnation)
    }

    /// Every name in `names` that binds, created no later than
    /// `not_after_ms`.
    pub(crate) fn bind_all(&self, names: &[String], not_after_ms: u64) -> Vec<i64> {
        names
            .iter()
            .filter_map(|n| self.bind(n, Some(not_after_ms)))
            .collect()
    }

    pub(crate) fn parent(&self, branch: &LegacyBranch) -> Option<i64> {
        let parent = self.bind(branch.parent.as_deref()?, None)?;
        (parent < branch.incarnation).then_some(parent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str, incarnation: i64, parent: Option<(&str, i64)>) -> Identity {
        Identity {
            name: name.into(),
            incarnation,
            parent_incarnation: parent.map(|(_, i)| i),
            parent: parent.map(|(n, _)| n.to_owned()),
        }
    }

    fn grants<'a>(owner: i64, ancestry: &'a [i64], shares: &'a [Share]) -> Grants<'a> {
        Grants {
            owner: Some(owner),
            ancestry,
            shares,
        }
    }

    #[test]
    fn ancestor_and_descendant_reads_are_allowed_siblings_are_not() {
        // root(1) -> a(2) -> aa(4); root -> b(3).
        let lineage = Lineage::from_identities(vec![
            identity("root", 1, None),
            identity("a", 2, Some(("root", 1))),
            identity("b", 3, Some(("root", 1))),
            identity("aa", 4, Some(("a", 2))),
        ]);
        let ancestry = lineage.ancestors(4);
        assert_eq!(ancestry, vec![1, 2]);
        let by_aa = grants(4, &ancestry, &[]);
        // Ancestors of the publisher (aa) read what it published.
        assert!(by_aa.readable(&lineage, 1));
        assert!(by_aa.readable(&lineage, 2));
        // A descendant of the publisher (a) reads what it published, found
        // live without needing a's own ancestry snapshot.
        assert!(grants(2, &[], &[]).readable(&lineage, 4));
        // A sibling does not, until shared.
        assert!(!by_aa.readable(&lineage, 3));
        let shared = [Share {
            branch: "b".into(),
            incarnation: Some(3),
        }];
        assert!(grants(4, &ancestry, &shared).readable(&lineage, 3));
    }

    #[test]
    fn grants_survive_the_publishers_removal_but_not_to_a_reused_name() {
        // a(2), root's child, published and was removed; its child ab(3)
        // lives on. A new, unrelated branch took the name a (5).
        let lineage = Lineage::from_identities(vec![
            identity("root", 1, None),
            identity("ab", 3, Some(("a", 2))),
            identity("a", 5, None),
        ]);
        let by_a = grants(2, &[1], &[]);
        assert!(by_a.readable(&lineage, 1), "the ancestor still reads");
        assert!(by_a.readable(&lineage, 3), "the descendant still reads");
        assert!(
            !by_a.readable(&lineage, 5),
            "the new holder of the name does not"
        );
        let upheld = lineage.upheld();
        assert!(by_a.reachable(&lineage, &upheld));
    }

    #[test]
    fn an_unbound_parent_is_the_older_live_holder_of_its_name_only() {
        // c(4) names parent p, never bound. The live p is younger (6): it
        // took the name after c's real parent was removed, so it is not
        // c's parent.
        let lineage = Lineage::from_identities(vec![
            Identity {
                name: "c".into(),
                incarnation: 4,
                parent_incarnation: None,
                parent: Some("p".into()),
            },
            identity("p", 6, None),
        ]);
        assert!(lineage.ancestors(4).is_empty());
        assert!(!grants(6, &[], &[]).readable(&lineage, 4));
        // An older live p is.
        let lineage = Lineage::from_identities(vec![
            identity("p", 3, None),
            Identity {
                name: "c".into(),
                incarnation: 4,
                parent_incarnation: None,
                parent: Some("p".into()),
            },
        ]);
        assert_eq!(lineage.ancestors(4), vec![3]);
    }

    #[test]
    fn an_unbound_legacy_grant_reads_nothing_and_keeps_nothing_alive() {
        let lineage = Lineage::from_identities(vec![identity("kid", 7, None)]);
        let shares = [Share {
            branch: "kid".into(),
            incarnation: None,
        }];
        let legacy = Grants {
            owner: None,
            ancestry: &[],
            shares: &shares,
        };
        assert!(!legacy.readable(&lineage, 7));
        assert!(!legacy.reachable(&lineage, &lineage.upheld()));
    }

    #[test]
    fn scratch_names_are_validated() {
        assert!(validate_scratch_name("shared-cache").is_ok());
        assert!(validate_scratch_name("Shared").is_err());
        assert!(validate_scratch_name("1x").is_err());
        assert!(validate_scratch_name("").is_err());
    }
}
