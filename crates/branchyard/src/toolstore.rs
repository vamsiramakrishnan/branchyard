//! The shared tool store: skills and connector packages named by their
//! content, in one read-only directory that hosts and sandboxes mount
//! instead of copying a tree into every turn's home.
//!
//! ```text
//! <store>/
//!   store.json                         the index: id, kind, digest, size, path
//!   skills/<name>@<digest>/            a built-in skill, as embedded
//!   connectors/<id>@<bundle-hash>/     the harness package Anvil wrote for a bundle
//! ```
//!
//! A skill's `<digest>` and an entry's `digest` in the index are the
//! BLAKE3 content hash [`crate::connectors::packager::hash_dir`] gives a
//! directory, so a pulled entry is checked against the index
//! ([`verify`]). A connector's `<bundle-hash>` is the hash of the bundle's
//! source, the key of the package cache, so a changed bundle is a new entry.
//!
//! The engine uses an entry when the store has one for the turn's skill
//! or granted connector and the turn's provider sees the store at the same
//! path (a provider's `sees_store`); otherwise it
//! copies, as before. [`publish`] fills a store; [`mirror_to_gcs`],
//! [`push_oci`], [`pull_gcs`] and [`pull_oci`] move it through `gcloud` and
//! `oras`. See `docs/tools-model.md` section 4.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use branchyard_provision::skills::Skill;
use serde::{Deserialize, Serialize};

use crate::connectors::packager::{self, Bundle, Packager};

/// The directory of skills in a store.
pub const SKILLS_DIR: &str = "skills";
/// The directory of connector packages in a store.
pub const CONNECTORS_DIR: &str = "connectors";
/// The index, at the store's root.
pub const INDEX_FILE: &str = "store.json";
/// Where a host keeps the store when nothing says otherwise.
pub const DEFAULT_ROOT: &str = "/opt/branchyard/store";
/// The artifact type of an entry pushed to a registry.
pub const OCI_ENTRY_TYPE: &str = "application/vnd.branchyard.tool.v1";
/// The artifact type of the index pushed to a registry.
pub const OCI_INDEX_TYPE: &str = "application/vnd.branchyard.tool-index.v1+json";

/// A store a yard links turns to ([`crate::Yard::use_tool_store`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolStore {
    /// The store's root, the same path on the host and in every sandbox
    /// that sees it.
    pub root: PathBuf,
    /// The Cloud Storage bucket (`BUCKET` or `BUCKET/PREFIX`) a Kubernetes
    /// sandbox mounts at `root` with the Cloud Storage FUSE CSI driver.
    pub bucket: Option<String>,
}

impl ToolStore {
    /// A store at `root` with no bucket.
    pub fn new(root: impl Into<PathBuf>) -> ToolStore {
        ToolStore {
            root: root.into(),
            bucket: None,
        }
    }

    /// The store's entry for built-in `skill`, when it has one.
    pub(crate) fn skill(&self, skill: &Skill) -> Option<PathBuf> {
        self.skill_entry(skill.name, &skill_digest(skill))
    }

    /// The store's entry for the skill `name` whose files hash to `digest`,
    /// when it has one.
    pub(crate) fn skill_entry(&self, name: &str, digest: &str) -> Option<PathBuf> {
        let dir = self.root.join(SKILLS_DIR).join(format!("{name}@{digest}"));
        dir.is_dir().then_some(dir)
    }

    /// The store's package of connector `id` built from the bundle with
    /// hash `bundle_hash`, when it has one.
    pub(crate) fn connector(&self, id: &str, bundle_hash: &str) -> Option<PathBuf> {
        let dir = self
            .root
            .join(CONNECTORS_DIR)
            .join(format!("{id}@{bundle_hash}"));
        dir.is_dir().then_some(dir)
    }
}

/// The store a turn of `record` may link to: the yard's, when its root
/// exists and the turn's provider sees it at that path. `None` means the
/// turn copies.
pub(crate) fn for_turn(
    yard: &crate::Yard,
    record: &crate::state::Record,
) -> Option<std::sync::Arc<ToolStore>> {
    yard.tool_store().filter(|store| {
        store.root.is_dir() && crate::providers::of(record.provider.as_ref()).sees_store(yard)
    })
}

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A built-in skill.
    Skill,
    /// A connector's harness package.
    Connector,
}

impl Kind {
    /// The store directory entries of this kind live in.
    pub fn directory(self) -> &'static str {
        match self {
            Kind::Skill => SKILLS_DIR,
            Kind::Connector => CONNECTORS_DIR,
        }
    }
}

/// One entry of the index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The skill's name or the connector's id.
    pub id: String,
    /// Skill or connector.
    pub kind: Kind,
    /// The content hash of the entry's directory.
    pub digest: String,
    /// The bytes in its files.
    pub size: u64,
    /// The entry's directory below the store root, `kind/id@key`.
    pub path: String,
}

impl Entry {
    /// The part of the path after `@`: a skill's digest, a connector's
    /// bundle hash. It tags the entry in a registry.
    pub fn key(&self) -> &str {
        self.path.rsplit_once('@').map_or("", |(_, key)| key)
    }
}

/// `store.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    /// Always 1.
    pub version: u32,
    /// Every entry, sorted by kind and id.
    pub entries: Vec<Entry>,
}

/// BLAKE3 over files given as (relative path, content): the same hash
/// [`packager::hash_dir`] gives the directory they make.
pub fn digest_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> String {
    let mut files: Vec<_> = files.into_iter().collect();
    files.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = blake3::Hasher::new();
    for (path, bytes) in files {
        hasher.update(path.as_bytes());
        hasher.update(&[0]);
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    hasher.finalize().to_hex().to_string()
}

/// The content hash of an embedded skill.
pub fn skill_digest(skill: &Skill) -> String {
    digest_files(skill.files.iter().map(|(p, c)| (*p, c.as_bytes())))
}

/// Fill the store at `store` with the built-in skills and, given
/// `connectors` (a packager and the cache it packages through, keyed by
/// bundle hash), the packages of every bundle it serves, then write the
/// index. An entry already there is kept, so
/// publishing twice changes nothing. Entries from earlier versions stay
/// until removed by hand; the index lists only what is current.
pub fn publish(store: &Path, connectors: Option<(&dyn Packager, &Path)>) -> Result<Index, String> {
    let mut entries = Vec::new();
    for skill in crate::capabilities::catalogued()? {
        entries.push(publish_skill(store, skill)?);
    }
    if let Some((packager, cache)) = connectors {
        for bundle in packager.served()? {
            entries.push(publish_connector(store, packager, cache, &bundle)?);
        }
    }
    entries.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
    let index = Index {
        version: 1,
        entries,
    };
    write_index(store, &index)?;
    Ok(index)
}

fn publish_skill(store: &Path, skill: &Skill) -> Result<Entry, String> {
    let digest = skill_digest(skill);
    let path = format!("{SKILLS_DIR}/{}@{digest}", skill.name);
    let dir = store.join(&path);
    if !dir.is_dir() {
        let tmp = temp_beside(&dir)?;
        let written = (|| -> Result<(), String> {
            for (rel, content) in skill.files {
                let file = tmp.join(rel);
                if let Some(parent) = file.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("create {}: {e}", parent.display()))?;
                }
                fs::write(&file, content).map_err(|e| format!("write {}: {e}", file.display()))?;
            }
            Ok(())
        })();
        settle(written, &tmp, &dir)?;
    }
    Ok(Entry {
        id: skill.name.to_owned(),
        kind: Kind::Skill,
        digest: digest.clone(),
        size: skill.files.iter().map(|(_, c)| c.len() as u64).sum(),
        path,
    })
}

fn publish_connector(
    store: &Path,
    packager: &dyn Packager,
    cache: &Path,
    bundle: &Bundle,
) -> Result<Entry, String> {
    let path = format!("{CONNECTORS_DIR}/{}@{}", bundle.id, bundle.hash);
    let dir = store.join(&path);
    if !dir.is_dir() {
        let package = crate::connectors::cached_package(packager, cache, bundle)?;
        let tmp = temp_beside(&dir)?;
        let copied = packager::copy_tree(&package, &tmp);
        settle(copied, &tmp, &dir)?;
    }
    Ok(Entry {
        id: bundle.id.clone(),
        kind: Kind::Connector,
        digest: packager::hash_dir(&dir)?,
        size: size_of(&dir)?,
        path,
    })
}

/// A fresh empty directory beside `dir`, to be renamed to it.
fn temp_beside(dir: &Path) -> Result<PathBuf, String> {
    let parent = dir.parent().ok_or("a store entry has no parent")?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    let tmp = parent.join(format!(
        ".tmp-{}",
        crate::connectors::keys::random_id().map_err(|e| e.to_string())?
    ));
    fs::create_dir_all(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    Ok(tmp)
}

/// Rename `tmp` to `dir` when `written` succeeded; another publisher may
/// have got there first, which is as good.
fn settle(written: Result<(), String>, tmp: &Path, dir: &Path) -> Result<(), String> {
    if let Err(e) = written {
        branchyard_support::cleanup_dir(tmp);
        return Err(e);
    }
    if fs::rename(tmp, dir).is_err() {
        branchyard_support::cleanup_dir(tmp);
        if !dir.is_dir() {
            return Err(format!("could not place {}", dir.display()));
        }
    }
    Ok(())
}

/// The bytes in the regular files under `dir`.
pub fn size_of(dir: &Path) -> Result<u64, String> {
    let mut total = 0;
    for entry in fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            total += size_of(&entry.path())?;
        } else if kind.is_file() {
            total += entry.metadata().map_err(|e| e.to_string())?.len();
        }
    }
    Ok(total)
}

/// Write `index` to the store's `store.json`.
pub fn write_index(store: &Path, index: &Index) -> Result<(), String> {
    fs::create_dir_all(store).map_err(|e| format!("create {}: {e}", store.display()))?;
    let text = serde_json::to_string_pretty(index).map_err(|e| e.to_string())?;
    let tmp = store.join(format!("{INDEX_FILE}.tmp"));
    fs::write(&tmp, format!("{text}\n")).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, store.join(INDEX_FILE)).map_err(|e| format!("write {INDEX_FILE}: {e}"))
}

/// The index at `store`, checked for names that would leave the store.
pub fn read_index(store: &Path) -> Result<Index, String> {
    let path = store.join(INDEX_FILE);
    let text = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let index: Index = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not an index: {e}", path.display()))?;
    if index.version != 1 {
        return Err(format!(
            "{} is version {}; this build reads version 1",
            path.display(),
            index.version
        ));
    }
    for entry in &index.entries {
        check_entry(entry)?;
    }
    Ok(index)
}

/// Whether `entry` names a directory directly below its kind's directory,
/// and nothing else: an index comes from a bucket or a registry, so its
/// names are not trusted.
fn check_entry(entry: &Entry) -> Result<(), String> {
    let plain = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && !s.starts_with('.')
    };
    let expected = format!("{}/{}@{}", entry.kind.directory(), entry.id, entry.key());
    match plain(&entry.id) && plain(entry.key()) && entry.path == expected {
        true => Ok(()),
        false => Err(format!(
            "the index names {:?}, which is not a store entry",
            entry.path
        )),
    }
}

/// Check every entry of `index` against the files in `store`: the
/// directory is there and its content hash is the recorded digest. The
/// error names the first path that fails.
pub fn verify(store: &Path, index: &Index) -> Result<(), String> {
    for entry in &index.entries {
        let dir = store.join(&entry.path);
        if !dir.is_dir() {
            return Err(format!("{} is missing", dir.display()));
        }
        if packager::hash_dir(&dir)? != entry.digest {
            return Err(format!(
                "{} does not match its digest in {INDEX_FILE}",
                dir.display()
            ));
        }
    }
    Ok(())
}

/// Entries of `index` that `store` does not have a directory for.
pub fn missing(store: &Path, index: &Index) -> Vec<Entry> {
    index
        .entries
        .iter()
        .filter(|e| !store.join(&e.path).is_dir())
        .cloned()
        .collect()
}

// Mirrors and registries, through the operator's own CLIs.

/// Run `program` with `args` in `dir` (this process's directory when
/// `None`) and fail with its last line of stderr.
fn run(program: &str, args: &[String], dir: Option<&Path>, hint: &str) -> Result<(), String> {
    let mut command = branchyard_support::child::command(program);
    command.args(args).stdin(Stdio::null());
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = command.output().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("{program} is not on PATH; {hint}"),
        _ => format!("could not run {program}: {e}"),
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = stderr.trim().lines().last().unwrap_or("").trim();
    Err(format!(
        "{program} {} failed ({}){}",
        args.first().map_or("", String::as_str),
        output.status,
        match last.is_empty() {
            true => String::new(),
            false => format!(": {last}"),
        }
    ))
}

const GCLOUD_HINT: &str = "install the Google Cloud CLI (https://cloud.google.com/sdk)";
const ORAS_HINT: &str = "install ORAS (https://oras.land) or use --gcs";

/// The command lines that move a store: `gcloud` and `oras`, found on
/// `PATH` unless a caller names them (a test names fakes, so it changes no
/// process-wide state).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Programs {
    /// The Google Cloud CLI.
    pub gcloud: String,
    /// The ORAS CLI.
    pub oras: String,
}

impl Default for Programs {
    fn default() -> Programs {
        Programs {
            gcloud: "gcloud".into(),
            oras: "oras".into(),
        }
    }
}

/// Check a `gs://BUCKET[/PREFIX]` location.
pub fn check_gcs(location: &str) -> Result<(), String> {
    let rest = location.strip_prefix("gs://").unwrap_or("");
    match rest.split('/').next().unwrap_or("").is_empty() || location.contains(' ') {
        true => Err(format!(
            "{location:?} is not a Cloud Storage location; use gs://BUCKET/PREFIX"
        )),
        false => Ok(()),
    }
}

/// Check a `REGISTRY/REPOSITORY` reference with no tag or scheme.
pub fn check_oci(base: &str) -> Result<(), String> {
    let tail = base.rsplit('/').next().unwrap_or("");
    let bad = base.is_empty()
        || !base.contains('/')
        || base.contains("://")
        || base.contains('@')
        || tail.contains(':')
        || base.ends_with('/')
        || base.bytes().any(|b| b.is_ascii_whitespace());
    match bad {
        true => Err(format!(
            "{base:?} is not REGISTRY/REPOSITORY, such as us-docker.pkg.dev/PROJECT/tools/store"
        )),
        false => Ok(()),
    }
}

/// `gcloud storage rsync --recursive STORE gs://...`
pub fn mirror_to_gcs(programs: &Programs, store: &Path, location: &str) -> Result<(), String> {
    check_gcs(location)?;
    run(
        &programs.gcloud,
        &[
            "storage".into(),
            "rsync".into(),
            "--recursive".into(),
            store.display().to_string(),
            location.into(),
        ],
        None,
        GCLOUD_HINT,
    )
}

/// `gcloud storage rsync --recursive gs://... STORE`, then the index is
/// read and every entry in it checked.
pub fn pull_gcs(programs: &Programs, location: &str, store: &Path) -> Result<Index, String> {
    check_gcs(location)?;
    fs::create_dir_all(store).map_err(|e| format!("create {}: {e}", store.display()))?;
    run(
        &programs.gcloud,
        &[
            "storage".into(),
            "rsync".into(),
            "--recursive".into(),
            location.into(),
            store.display().to_string(),
        ],
        None,
        GCLOUD_HINT,
    )?;
    let index = read_index(store)?;
    verify(store, &index)?;
    Ok(index)
}

/// The reference of `entry` below `base`: `base/kind/id:key`.
fn oci_ref(base: &str, entry: &Entry) -> String {
    format!(
        "{base}/{}/{}:{}",
        entry.kind.directory(),
        entry.id,
        entry.key()
    )
}

fn oci_index_ref(base: &str) -> String {
    format!("{base}/index:latest")
}

/// Push every entry of the store's index, and the index itself, to
/// `base` (`REGISTRY/REPOSITORY`) with `oras push`. Each entry is one
/// artifact, `base/skills/NAME:DIGEST` or `base/connectors/ID:BUNDLE-HASH`,
/// holding its directory.
pub fn push_oci(programs: &Programs, store: &Path, base: &str) -> Result<Index, String> {
    check_oci(base)?;
    let index = read_index(store)?;
    for entry in &index.entries {
        let parent = store.join(entry.kind.directory());
        let name = format!("{}@{}", entry.id, entry.key());
        run(
            &programs.oras,
            &[
                "push".into(),
                "--artifact-type".into(),
                OCI_ENTRY_TYPE.into(),
                oci_ref(base, entry),
                name,
            ],
            Some(&parent),
            ORAS_HINT,
        )?;
    }
    run(
        &programs.oras,
        &[
            "push".into(),
            "--artifact-type".into(),
            OCI_INDEX_TYPE.into(),
            oci_index_ref(base),
            format!("{INDEX_FILE}:application/json"),
        ],
        Some(store),
        ORAS_HINT,
    )?;
    Ok(index)
}

/// Fill `store` from `base` with `oras pull`: the index first, then each
/// entry the store lacks, then every entry is checked against the index.
pub fn pull_oci(programs: &Programs, base: &str, store: &Path) -> Result<Index, String> {
    check_oci(base)?;
    fs::create_dir_all(store).map_err(|e| format!("create {}: {e}", store.display()))?;
    run(
        &programs.oras,
        &[
            "pull".into(),
            oci_index_ref(base),
            "--output".into(),
            store.display().to_string(),
        ],
        None,
        ORAS_HINT,
    )?;
    let index = read_index(store)?;
    for entry in missing(store, &index) {
        let into = store.join(entry.kind.directory());
        run(
            &programs.oras,
            &[
                "pull".into(),
                oci_ref(base, &entry),
                "--output".into(),
                into.display().to_string(),
            ],
            None,
            ORAS_HINT,
        )?;
    }
    verify(store, &index)?;
    Ok(index)
}

#[cfg(test)]
mod tests;
