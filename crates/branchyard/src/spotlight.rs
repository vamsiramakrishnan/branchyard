//! `by try`: a branch's changes applied to the repository's own checkout,
//! where the user's dev server already runs, and taken back out exactly.
//!
//! Applying is refused unless the checkout is clean (nothing staged,
//! modified or untracked, ignored files aside). Clean is not byte for byte
//! `HEAD`, though: line-ending conversion and clean/smudge filters make the
//! bytes on disk differ from the blobs. So before anything is written, the
//! bytes each path the diff touches holds on disk (a file's contents or a
//! link's target) are copied to `.branchyard/try/before/`, named by their
//! unfiltered blob ID, and synced; then the plan is saved to
//! `.branchyard/try/state.json` in the phase `applying`: the branch, its
//! candidate, the diff's base, the checkout's `HEAD`, and for each path the
//! diff touches its entry before (mode, that blob ID and permission bits,
//! or absent) and, once applied, after. The diff is then applied with `git apply`,
//! which changes nothing when any hunk does not apply, and the phase becomes
//! `applied`. `HEAD` and the candidate are pinned by refs under
//! `refs/branchyard-try/` so their objects outlive the branch.
//!
//! Turning it off (`--off`) refuses when a tried file was changed since or
//! `HEAD` moved, unless forced, then records the phase `restoring` and
//! writes every path back from its saved entry: the saved bytes with their
//! permission bits, a symbolic link, or nothing for a path that did not
//! exist, removing the directories applying created. A state left in
//! `applying` or `restoring` by a process that stopped is rolled back the
//! same way by the next `try` call, before anything else.
//!
//! The index is never written: it stays at `HEAD` throughout, so `git
//! status` shows the tried changes as unstaged, and once restored, nothing.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::checkpoint::fault;
use crate::state::now_ms;
use crate::{git, DirLock, Error, Yard};
use branchyard_workspace::Git;

const HEAD_REF: &str = "refs/branchyard-try/head";
const CANDIDATE_REF: &str = "refs/branchyard-try/candidate";

/// Where a try's state is kept, in the repository's `.branchyard/`.
fn dir(yard: &Yard) -> PathBuf {
    crate::state::dir(&yard.root).join("try")
}

fn state_path(yard: &Yard) -> PathBuf {
    dir(yard).join("state.json")
}

/// Where the bytes each tried path held on disk before are kept, by blob ID.
fn before_dir(yard: &Yard) -> PathBuf {
    dir(yard).join("before")
}

/// A try in effect, as `.branchyard/try/state.json` holds it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TryState {
    pub branch: String,
    /// The candidate whose diff was applied.
    pub commit: String,
    /// What the diff is against: the branch's base.
    pub base: String,
    /// The checkout's `HEAD` when it was applied.
    pub head: String,
    /// `applying`, `applied` or `restoring`.
    pub phase: String,
    /// Milliseconds since the Unix epoch.
    pub applied_ms: u64,
    pub files: Vec<TryFile>,
    /// Directories applying created, deepest last.
    #[serde(default)]
    pub created_dirs: Vec<String>,
}

/// One path the try changed, relative to the repository root.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TryFile {
    pub path: String,
    /// What the checkout held before: `None` when the path did not exist.
    pub before: Option<TryEntry>,
    /// What applying left there: `None` when it deleted the path. Unset
    /// until applied.
    pub after: Option<TryEntry>,
}

/// A file's git mode and blob, and for a regular file its permission bits
/// on disk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TryEntry {
    /// `100644`, `100755` or `120000` (a symbolic link).
    pub mode: String,
    /// The blob ID of the bytes on disk (a link's target), hashed without
    /// filters, so it can differ from the blob in `HEAD`.
    pub blob: String,
    /// `st_mode & 0o7777`, for a regular file.
    pub permissions: Option<u32>,
}

const APPLYING: &str = "applying";
const APPLIED: &str = "applied";
const RESTORING: &str = "restoring";

pub(crate) fn load(yard: &Yard) -> Result<Option<TryState>, Error> {
    match fs::read(state_path(yard)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::State(format!("unreadable {}: {e}", state_path(yard).display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write the state durably: a temporary file, synced, renamed over it.
fn save(yard: &Yard, state: &TryState) -> Result<(), Error> {
    let dir = dir(yard);
    fs::create_dir_all(&dir)?;
    let temp = dir.join("state.json.tmp");
    let mut file = fs::File::create(&temp)?;
    file.write_all(&serde_json::to_vec_pretty(state).unwrap_or_default())?;
    file.sync_all()?;
    fs::rename(&temp, state_path(yard))?;
    fs::File::open(&dir)?.sync_all()?;
    Ok(())
}

fn clear(yard: &Yard) -> Result<(), Error> {
    match fs::remove_file(state_path(yard)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    git::delete_ref(&yard.root, HEAD_REF)?;
    git::delete_ref(&yard.root, CANDIDATE_REF)?;
    remove_before(yard)
}

fn remove_before(yard: &Yard) -> Result<(), Error> {
    match fs::remove_dir_all(before_dir(yard)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Keep the bytes `path` holds on disk, durably, under their blob ID.
fn keep_before(yard: &Yard, path: &str, entry: &TryEntry) -> Result<(), Error> {
    let full = yard.root.join(path);
    let bytes = if entry.mode == "120000" {
        fs::read_link(&full)?.into_os_string().into_encoded_bytes()
    } else {
        fs::read(&full)?
    };
    let dir = before_dir(yard);
    fs::create_dir_all(&dir)?;
    let mut file = fs::File::create(dir.join(&entry.blob))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

/// The bytes saved for `entry` before the try, or, for a state written
/// before they were saved, its blob from git.
fn before_bytes(yard: &Yard, entry: &TryEntry) -> Result<Vec<u8>, Error> {
    match fs::read(before_dir(yard).join(&entry.blob)) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => blob_bytes(&yard.root, &entry.blob),
        Err(e) => Err(e.into()),
    }
}

fn lock(yard: &Yard) -> Result<DirLock, Error> {
    DirLock::acquire(&dir(yard), "by try")
}

/// Roll back a try a stopped process left half-applied or half-restored.
/// Returns what was done, if anything.
pub(crate) fn recover(yard: &Yard) -> Result<Option<String>, Error> {
    let _lock = lock(yard)?;
    recover_locked(yard)
}

fn recover_locked(yard: &Yard) -> Result<Option<String>, Error> {
    let Some(state) = load(yard)? else {
        return Ok(None);
    };
    if state.phase == APPLIED {
        return Ok(None);
    }
    let was = state.phase.clone();
    restore(yard, state.clone())?;
    Ok(Some(format!(
        "a try of {} was left {was} by a process that stopped; the checkout was restored \
         to what it held before",
        state.branch
    )))
}

pub(crate) fn status(yard: &Yard) -> Result<Option<TryState>, Error> {
    let _lock = lock(yard)?;
    recover_locked(yard)?;
    load(yard)
}

/// Apply `name`'s candidate to the checkout, first turning off a try of
/// another branch. Trying the branch already tried at the same candidate
/// changes nothing.
pub(crate) fn on(yard: &Yard, name: &str) -> Result<TryState, Error> {
    let _lock = lock(yard)?;
    recover_locked(yard)?;
    let record = yard.store().read(name)?;
    let candidate = record
        .info
        .candidate
        .clone()
        .ok_or_else(|| Error::NoCandidate(name.to_owned()))?;
    if let Some(current) = load(yard)? {
        if current.branch == name && current.commit == candidate.commit {
            return Ok(current);
        }
        off_locked(yard, false)?;
    }
    let root = &yard.root;
    let dirty = git::status(root)?;
    if !dirty.trim().is_empty() {
        return Err(Error::Denied(format!(
            "the checkout at {} has uncommitted changes; by try applies a branch only to a \
             clean checkout, so that turning it off restores exactly what was there. Commit or \
             stash them first:\n{}",
            root.display(),
            dirty.trim_end()
        )));
    }
    let head = git::commit(root, "HEAD")?
        .ok_or_else(|| Error::Git("the checkout has no commit checked out".into()))?;
    let base = record.info.base.clone();
    let paths = git::changed_paths(root, &base, &candidate.commit)?;
    // Left by a try whose state was cleared before its bytes were.
    remove_before(yard)?;
    let mut files = Vec::new();
    let mut created_dirs = Vec::new();
    for path in &paths {
        // Refuses a path that is not a file in `HEAD`.
        entry_at(root, &head, path)?;
        // What is on disk, which filters can make differ from `HEAD`.
        let before = current(root, path)?;
        if before.as_ref().is_some_and(|entry| entry.mode == "040000") {
            return Err(Error::Denied(format!(
                "{path} is a directory in the checkout; by try handles files only"
            )));
        }
        if let Some(entry) = &before {
            keep_before(yard, path, entry)?;
        }
        if before.is_none() {
            let mut parent = Path::new(path).parent();
            let mut new_dirs = Vec::new();
            while let Some(dir) = parent.filter(|d| !d.as_os_str().is_empty()) {
                if root.join(dir).exists() {
                    break;
                }
                new_dirs.push(dir.to_string_lossy().into_owned());
                parent = dir.parent();
            }
            new_dirs.reverse();
            for dir in new_dirs {
                if !created_dirs.contains(&dir) {
                    created_dirs.push(dir);
                }
            }
        }
        files.push(TryFile {
            path: path.clone(),
            before,
            after: None,
        });
    }
    if !files.is_empty() {
        fs::File::open(before_dir(yard))
            .and_then(|d| d.sync_all())
            .ok();
    }
    let patch = git::run(
        root,
        &[
            "diff",
            "--binary",
            "--full-index",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            &base,
            &candidate.commit,
            "--",
        ],
    )?;
    if patch.is_empty() {
        return Err(Error::State(format!(
            "{name}'s candidate changes nothing against its base"
        )));
    }
    // Refused here, nothing has been written.
    if let Err(error) = apply(root, &patch, true) {
        return Err(Error::Denied(format!(
            "{name}'s changes do not apply to the checkout, which has moved on from the \
             branch's base where they touch it; nothing was changed. {error}"
        )));
    }
    git::update_ref(root, HEAD_REF, &head)?;
    git::update_ref(root, CANDIDATE_REF, &candidate.commit)?;
    let mut state = TryState {
        branch: name.to_owned(),
        commit: candidate.commit.clone(),
        base,
        head,
        phase: APPLYING.to_owned(),
        applied_ms: now_ms(),
        files,
        created_dirs,
    };
    save(yard, &state)?;
    fault("try-after-intent");
    if let Err(error) = apply(root, &patch, false) {
        // Nothing, or only part, was written: put back what was there.
        restore(yard, state)?;
        return Err(Error::Denied(format!(
            "{name}'s changes could not be applied; the checkout was restored. {error}"
        )));
    }
    fault("try-after-apply");
    for file in &mut state.files {
        file.after = current(root, &file.path)?;
    }
    state.phase = APPLIED.to_owned();
    save(yard, &state)?;
    Ok(state)
}

/// Take the try out. Refused when a tried file changed since it was applied
/// or `HEAD` moved, unless `force`: those edits would be lost.
pub(crate) fn off(yard: &Yard, force: bool) -> Result<Option<TryState>, Error> {
    let _lock = lock(yard)?;
    recover_locked(yard)?;
    off_locked(yard, force)
}

fn off_locked(yard: &Yard, force: bool) -> Result<Option<TryState>, Error> {
    let Some(state) = load(yard)? else {
        return Ok(None);
    };
    if !force {
        let root = &yard.root;
        let head = git::commit(root, "HEAD")?.unwrap_or_default();
        if head != state.head {
            return Err(Error::Denied(format!(
                "the checkout's HEAD moved from {} to {head} while {} was tried; restoring the \
                 files it changed may undo work. Turn it off with --force to restore them anyway",
                short(&state.head),
                state.branch
            )));
        }
        let mut changed = Vec::new();
        for file in &state.files {
            if current(root, &file.path)? != file.after {
                changed.push(file.path.clone());
            }
        }
        if !changed.is_empty() {
            return Err(Error::Denied(format!(
                "these files changed since {} was tried, and restoring them would discard \
                 that: {}. Save what you need, then turn it off with --force",
                state.branch,
                changed.join(", ")
            )));
        }
    }
    restore(yard, state.clone())?;
    Ok(Some(state))
}

/// Put every path back as it was before the try, then forget it.
fn restore(yard: &Yard, mut state: TryState) -> Result<(), Error> {
    let root = &yard.root;
    if state.phase != RESTORING {
        state.phase = RESTORING.to_owned();
        save(yard, &state)?;
    }
    for (n, file) in state.files.iter().enumerate() {
        if n == 1 {
            fault("try-mid-restore");
        }
        write_entry(yard, &file.path, file.before.as_ref())?;
    }
    for dir in state.created_dirs.iter().rev() {
        // Only if empty: anything else in it is not the try's.
        let _ = fs::remove_dir(root.join(dir));
    }
    // The index keeps HEAD's entries; refresh their stat data.
    let _ = git::run(root, &["update-index", "-q", "--refresh"]);
    clear(yard)
}

fn apply(root: &Path, patch: &str, check: bool) -> Result<(), Error> {
    let mut command = Git::new(root).args(["apply", "--whitespace=nowarn"]);
    if check {
        command = command.arg("--check");
    }
    let (out, _) = command.stdin(patch).output().map_err(git::error)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::Git(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ))
    }
}

/// The entry `path` has in `commit`, if any.
fn entry_at(root: &Path, commit: &str, path: &str) -> Result<Option<TryEntry>, Error> {
    let out = git::run(root, &["ls-tree", "-z", "--full-tree", commit, "--", path])?;
    let Some(line) = out.split('\0').find(|l| !l.is_empty()) else {
        return Ok(None);
    };
    // `<mode> <type> <object>\t<path>`
    let (meta, _) = line
        .split_once('\t')
        .ok_or_else(|| Error::Git(format!("unexpected ls-tree output {line:?}")))?;
    let mut fields = meta.split(' ');
    let (Some(mode), Some(kind), Some(blob)) = (fields.next(), fields.next(), fields.next()) else {
        return Err(Error::Git(format!("unexpected ls-tree output {line:?}")));
    };
    if kind != "blob" {
        return Err(Error::Denied(format!(
            "{path} is a {kind} in the checkout; by try handles files only"
        )));
    }
    Ok(Some(TryEntry {
        mode: mode.to_owned(),
        blob: blob.to_owned(),
        permissions: None,
    }))
}

#[cfg(unix)]
fn permissions(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn permissions(_path: &Path) -> Option<u32> {
    None
}

/// What `path` holds now, as an entry: its blob as git would hash it.
fn current(root: &Path, path: &str) -> Result<Option<TryEntry>, Error> {
    let full = root.join(path);
    let meta = match fs::symlink_metadata(&full) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        let target = fs::read_link(&full)?;
        let blob = hash(root, target.as_os_str().as_encoded_bytes())?;
        return Ok(Some(TryEntry {
            mode: "120000".into(),
            blob,
            permissions: None,
        }));
    }
    if !meta.is_file() {
        return Ok(Some(TryEntry {
            mode: "040000".into(),
            blob: String::new(),
            permissions: None,
        }));
    }
    let permissions = permissions(&full);
    let executable = permissions.is_some_and(|p| p & 0o111 != 0);
    let blob = hash(root, &fs::read(&full)?)?;
    Ok(Some(TryEntry {
        mode: if executable { "100755" } else { "100644" }.into(),
        blob,
        permissions,
    }))
}

/// The blob ID of `bytes`, exactly as stored (no filters).
fn hash(root: &Path, bytes: &[u8]) -> Result<String, Error> {
    let out = Git::new(root)
        .args(["hash-object", "--no-filters", "--stdin"])
        .stdin(bytes)
        .run()
        .map_err(git::error)?;
    Ok(out.trim().to_owned())
}

fn blob_bytes(root: &Path, blob: &str) -> Result<Vec<u8>, Error> {
    Git::new(root)
        .args(["cat-file", "blob", blob])
        .run_bytes()
        .map_err(|e| Error::Git(format!("could not read blob {blob}: {e}")))
}

/// Make `path` hold `entry`, or not exist.
fn write_entry(yard: &Yard, path: &str, entry: Option<&TryEntry>) -> Result<(), Error> {
    let full = yard.root.join(path);
    let existing = fs::symlink_metadata(&full).ok();
    if let Some(meta) = &existing {
        if meta.is_dir() {
            fs::remove_dir_all(&full)?;
        } else {
            fs::remove_file(&full)?;
        }
    }
    let Some(entry) = entry else {
        return Ok(());
    };
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = before_bytes(yard, entry)?;
    if entry.mode == "120000" {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let target = std::ffi::OsStr::from_bytes(&bytes);
            std::os::unix::fs::symlink(target, &full)?;
            return Ok(());
        }
        #[cfg(not(unix))]
        return Err(Error::Unsupported(
            "restoring a symbolic link needs a Unix host".into(),
        ));
    }
    fs::write(&full, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = entry.permissions.unwrap_or(match entry.mode.as_str() {
            "100755" => 0o755,
            _ => 0o644,
        });
        fs::set_permissions(&full, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}
