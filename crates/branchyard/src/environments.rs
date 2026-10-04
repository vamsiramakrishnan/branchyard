//! Prepared environments: `[workspace]` setup run once per environment key,
//! and every later branch with the same key started from its result. See
//! `docs/environments.md`.
//!
//! - **Key.** An environment's key hashes its *recipe* (the setup
//!   commands, the copy globs, the input globs, and where it is built: this
//!   host, or one sandbox provider) with the content of every input file
//!   (lockfiles and manifests: `[workspace] inputs`, or by default the
//!   common ones present). Change a lockfile and the key changes; change
//!   nothing and every new branch has the same key.
//! - **Built once.** The first branch whose key has no environment takes
//!   the key's lock, runs setup in its own worktree as usual, and then
//!   captures what setup produced (the paths [`crate::workspace`] records
//!   as `produced`) into `.branchyard/environments/<key>/tree/`, as the
//!   journaled step `environment`: staged beside it, then renamed into
//!   place. Branches that wanted the same key meanwhile wait for the lock,
//!   then restore it.
//! - **Restored.** A branch whose key has an environment does not run
//!   setup: each produced path is cloned into its worktree where the
//!   filesystem can (`FICLONE`, `clonefile`), copied where it cannot, or,
//!   for a `[workspace] share` directory, linked to the environment's copy.
//! - **Last good build.** A build whose setup fails never replaces a good
//!   environment. It is recorded as failed (`<key>.failed.json`), and the
//!   branch, and every later branch with that key, restores the newest good
//!   environment of the same recipe instead, and says so. `by env rebuild`
//!   tries again.
//! - **Sandboxes.** A sandboxed branch's environment is a provider snapshot
//!   of its sandbox taken right after setup, keyed the same way with the
//!   provider as the place; later sandboxes of that key are branched from
//!   it ([`crate::snapshots::acquire`]), with the same last-good fallback.
//!
//! Environments are files under `.branchyard/environments/`, not store
//! rows: whoever shares the checkout (a server and its workers on one
//! host) shares them, and the key's lock is an advisory `flock`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use branchyard_workspace::materialize::{self, Method, Mode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::state::{Fence, Store};
use crate::workspace::WorkspaceSpec;
use crate::{Error, Yard};
use branchyard_support::time::now_ms;

/// The files that key an environment when `[workspace] inputs` names none,
/// each if present at the top of the worktree.
pub const DEFAULT_INPUTS: &[&str] = &[
    "package.json",
    "pnpm-lock.yaml",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "Cargo.lock",
    "rust-toolchain.toml",
    "pyproject.toml",
    "uv.lock",
    "poetry.lock",
    "Pipfile.lock",
    "requirements.txt",
    "go.mod",
    "go.sum",
    "Gemfile.lock",
    "composer.lock",
    "mix.lock",
    "flake.lock",
    ".tool-versions",
    ".nvmrc",
    ".node-version",
    ".python-version",
];

/// Good environments kept per recipe by pruning.
pub const DEFAULT_KEEP: usize = 3;
/// Environments unused for longer than this are pruned.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);

/// Where an environment is built: on this host.
pub const HOST: &str = "host";

/// The journaled step that captures a built environment.
pub(crate) const STEP_ENVIRONMENT: &str = "environment";

/// How often a branch waiting for another's build looks again.
const LOCK_POLL: Duration = Duration::from_millis(100);
/// Longest wait for another branch's build: as long as setup may run.
const LOCK_WAIT: Duration = crate::workspace::SETUP_TIMEOUT;
const MANIFEST: &str = "manifest.json";
const TREE: &str = "tree";

/// One input file, as it keyed an environment.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentInput {
    /// Relative to the worktree.
    pub path: String,
    /// BLAKE3 of its content, hex.
    pub digest: String,
}

/// Whether an environment can be used.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentState {
    /// Built; branches with its key start from it.
    Good,
    /// Its setup failed; branches with its key use the last good one of
    /// the same recipe.
    Failed,
}

/// A provider snapshot an environment is, for sandboxed branches.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSnapshot {
    /// `microsandbox` or `substrate`.
    pub provider: String,
    /// The paused sandbox's name, or the checkpoint reference.
    pub handle: String,
    pub method: crate::SnapshotMethod,
    /// The provider's options, to release it by.
    #[cfg_attr(feature = "schema", schemars(skip))]
    #[serde(default)]
    pub options: Value,
    /// The provider's details (`crate::snapshots::Detail`).
    #[cfg_attr(feature = "schema", schemars(skip))]
    #[serde(default)]
    pub detail: Value,
}

/// A prepared environment, as `.branchyard/environments/` holds it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentInfo {
    pub key: String,
    /// The hash of what built it, less the inputs' content: environments of
    /// one recipe are each other's last good build.
    pub recipe: String,
    /// `host`, or the sandbox provider it was built in.
    pub place: String,
    pub state: EnvironmentState,
    pub inputs: Vec<EnvironmentInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub share: Vec<String>,
    /// What setup produced, relative to the worktree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub produced: Vec<String>,
    /// The branch whose setup built it (or `by env rebuild`).
    pub built_by: String,
    /// Milliseconds since the Unix epoch.
    pub built_ms: u64,
    pub last_used_ms: u64,
    /// Why its build failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// For a sandbox provider: the snapshot new sandboxes branch from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<EnvironmentSnapshot>,
}

/// How a branch's setup related to a prepared environment, on its
/// `workspace` event ([`crate::WorkspaceReport::environment`]).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentUse {
    pub key: String,
    pub origin: EnvironmentOrigin,
    /// The environment actually used: `key`, or the last good one's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<String>,
    /// `clone`, `copy` or `link` for a restore on this host (a mix reads
    /// `copy`); `live_branch` or `checkpoint` for a sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_by: Option<String>,
    /// Paths linked to the environment rather than copied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<String>,
    /// Why the last good environment was used, or why a built one was not
    /// kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Where a branch's environment came from.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentOrigin {
    /// Its setup ran and built the environment for its key.
    Built,
    /// Its key's environment was restored; setup did not run.
    Restored,
    /// Its key's build failed; the newest good environment of the same
    /// recipe was restored instead.
    LastGood,
    /// Its setup ran, and the result was not kept (`reason` says why).
    NotKept,
}

impl EnvironmentUse {
    fn new(key: &str, origin: EnvironmentOrigin) -> EnvironmentUse {
        EnvironmentUse {
            key: key.to_owned(),
            origin,
            used: None,
            method: None,
            built_by: None,
            shared: Vec::new(),
            reason: None,
        }
    }

    /// One line for a log.
    pub fn describe(&self) -> String {
        let key = short(&self.key);
        let used = self.used.as_deref().map(short).unwrap_or(key);
        let how = self
            .method
            .as_deref()
            .map(|m| format!(" by {m}"))
            .unwrap_or_default();
        match self.origin {
            EnvironmentOrigin::Built => format!("environment: built {key}"),
            EnvironmentOrigin::Restored => format!(
                "environment: started from {key}{how}{}",
                self.built_by
                    .as_deref()
                    .map(|b| format!(", built by {b}"))
                    .unwrap_or_default()
            ),
            EnvironmentOrigin::LastGood => format!(
                "environment: using the last good build {used}{how} ({})",
                self.reason.as_deref().unwrap_or("its own failed")
            ),
            EnvironmentOrigin::NotKept => format!(
                "environment: {key} not kept ({})",
                self.reason.as_deref().unwrap_or("unknown")
            ),
        }
    }
}

fn short(key: &str) -> &str {
    &key[..key.len().min(12)]
}

/// `.branchyard/environments` of the repository at `root`.
pub(crate) fn dir(root: &Path) -> PathBuf {
    crate::state::dir(root).join("environments")
}

/// The input files of `spec` in `base`: its `inputs` globs, or the default
/// lockfiles and manifests present, each regular file inside `base` once,
/// sorted.
pub(crate) fn inputs(base: &Path, spec: &WorkspaceSpec) -> Vec<EnvironmentInput> {
    let mut found = BTreeMap::new();
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let base = fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    let patterns: Vec<String> = match spec.inputs.is_empty() {
        true => DEFAULT_INPUTS.iter().map(|p| p.to_string()).collect(),
        false => spec.inputs.clone(),
    };
    for pattern in patterns {
        if crate::workspace::check_glob(&pattern).is_err() {
            continue;
        }
        let full = format!(
            "{}/{pattern}",
            glob::Pattern::escape(&base.display().to_string())
        );
        let Ok(paths) = glob::glob_with(&full, options) else {
            continue;
        };
        for path in paths.flatten() {
            let regular = fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
            let inside = fs::canonicalize(&path).is_ok_and(|p| p.starts_with(&base));
            let Ok(rel) = path.strip_prefix(&base) else {
                continue;
            };
            if !(regular && inside) {
                continue;
            }
            if let Ok(bytes) = fs::read(&path) {
                found.insert(
                    rel.display().to_string(),
                    blake3::hash(&bytes).to_hex().to_string(),
                );
            }
        }
    }
    found
        .into_iter()
        .map(|(path, digest)| EnvironmentInput { path, digest })
        .collect()
}

/// `(key, recipe)` of `spec` built at `place` from `inputs`.
pub(crate) fn key(
    spec: &WorkspaceSpec,
    place: &str,
    inputs: &[EnvironmentInput],
) -> (String, String) {
    let recipe = json!({
        "version": 1,
        "place": place,
        "setup": spec.setup,
        "copy": spec.copy,
        "inputs": match spec.inputs.is_empty() {
            true => DEFAULT_INPUTS.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            false => spec.inputs.clone(),
        },
    });
    let recipe = blake3::hash(recipe.to_string().as_bytes()).to_hex()[..24].to_owned();
    let keyed = json!({ "recipe": recipe, "inputs": inputs });
    let key = blake3::hash(keyed.to_string().as_bytes()).to_hex()[..24].to_owned();
    (key, recipe)
}

fn manifest_path(root: &Path, key: &str) -> PathBuf {
    dir(root).join(key).join(MANIFEST)
}

fn failed_path(root: &Path, key: &str) -> PathBuf {
    dir(root).join(format!("{key}.failed.json"))
}

/// The tree an environment's files are kept in.
pub(crate) fn tree(root: &Path, key: &str) -> PathBuf {
    dir(root).join(key).join(TREE)
}

fn read(path: &Path) -> Option<EnvironmentInfo> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Write `info` to `path` through a temporary file and a rename.
fn write(path: &Path, info: &EnvironmentInfo) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp.{}.{}", std::process::id(), nonce()));
    let text = serde_json::to_string_pretty(info).unwrap_or_default() + "\n";
    fs::File::create(&temporary)?.write_all(text.as_bytes())?;
    fs::rename(&temporary, path)
}

fn nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}{}",
        now_ms() % 1_000_000_000,
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// `key`'s good environment.
pub(crate) fn good(root: &Path, key: &str) -> Option<EnvironmentInfo> {
    read(&manifest_path(root, key)).filter(|i| i.state == EnvironmentState::Good && i.key == key)
}

/// `key`'s recorded failure.
pub(crate) fn failed(root: &Path, key: &str) -> Option<EnvironmentInfo> {
    read(&failed_path(root, key)).filter(|i| i.key == key)
}

/// The newest good environment of `recipe` other than `except`.
pub(crate) fn last_good(root: &Path, recipe: &str, except: &str) -> Option<EnvironmentInfo> {
    list(root)
        .into_iter()
        .filter(|i| i.state == EnvironmentState::Good && i.recipe == recipe && i.key != except)
        .max_by_key(|i| i.built_ms)
}

/// Every environment and recorded failure, newest built first.
pub fn list(root: &Path) -> Vec<EnvironmentInfo> {
    let Ok(entries) = fs::read_dir(dir(root)) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if let Some(key) = name.strip_suffix(".failed.json") {
            found.extend(failed(root, key));
        } else if entry.path().is_dir() {
            found.extend(good(root, &name));
        }
    }
    found.sort_by(|a, b| b.built_ms.cmp(&a.built_ms).then(a.key.cmp(&b.key)));
    found
}

/// Note that `info` was used now.
pub(crate) fn touch(root: &Path, info: &EnvironmentInfo) {
    let mut info = info.clone();
    info.last_used_ms = now_ms();
    let _ = write(&manifest_path(root, &info.key), &info);
}

/// An advisory lock on one key, held while its environment is built, and
/// by pruning. Released when dropped, or when its process dies.
pub(crate) struct KeyLock {
    _file: fs::File,
}

impl KeyLock {
    fn path(root: &Path, key: &str) -> PathBuf {
        dir(root).join(format!(".{key}.lock"))
    }

    /// Take `key`'s lock if nobody holds it.
    pub fn try_take(root: &Path, key: &str) -> Result<Option<KeyLock>, String> {
        use rustix::fs::{flock, FlockOperation};
        fs::create_dir_all(dir(root)).map_err(|e| e.to_string())?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(Self::path(root, key))
            .map_err(|e| format!("could not open the environment's lock: {e}"))?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(KeyLock { _file: file })),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(e) => Err(format!("could not lock the environment: {e}")),
        }
    }

    /// Take `key`'s lock, waiting while another holds it, until `stop`
    /// gives a reason or [`LOCK_WAIT`] passes.
    pub fn take(
        root: &Path,
        key: &str,
        stop: &dyn Fn() -> Option<String>,
    ) -> Result<KeyLock, String> {
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            if let Some(lock) = Self::try_take(root, key)? {
                return Ok(lock);
            }
            if let Some(why) = stop() {
                return Err(format!(
                    "stopped waiting for environment {}: {why}",
                    short(key)
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "environment {} was still being built after {}s",
                    short(key),
                    LOCK_WAIT.as_secs()
                ));
            }
            std::thread::sleep(LOCK_POLL);
        }
    }
}

/// What a branch's setup does about its environment.
pub(crate) enum Plan {
    /// Restore `0`; setup does not run.
    Restore(EnvironmentInfo),
    /// `key` failed before: restore the last good build instead.
    LastGood {
        key: String,
        good: EnvironmentInfo,
        reason: String,
    },
    /// Run setup, then capture it as `key`'s environment, holding its lock.
    Build {
        key: String,
        recipe: String,
        inputs: Vec<EnvironmentInput>,
        lock: KeyLock,
    },
}

/// Decide what a branch whose worktree is `worktree` does, on this host.
/// Waits for another branch building the same key.
pub(crate) fn plan(
    root: &Path,
    spec: &WorkspaceSpec,
    worktree: &Path,
    stop: &dyn Fn() -> Option<String>,
) -> Result<Plan, String> {
    let inputs = inputs(worktree, spec);
    let (key, recipe) = key(spec, HOST, &inputs);
    let decide = |root: &Path| -> Option<Plan> {
        if let Some(info) = good(root, &key) {
            return Some(Plan::Restore(info));
        }
        let failure = failed(root, &key)?;
        let good = last_good(root, &recipe, &key)?;
        Some(Plan::LastGood {
            key: key.clone(),
            reason: failure_reason(&key, &failure),
            good,
        })
    };
    if let Some(plan) = decide(root) {
        return Ok(plan);
    }
    let lock = KeyLock::take(root, &key, stop)?;
    // Built, or failed, while this branch waited.
    if let Some(plan) = decide(root) {
        return Ok(plan);
    }
    Ok(Plan::Build {
        key,
        recipe,
        inputs,
        lock,
    })
}

pub(crate) fn failure_reason(key: &str, failure: &EnvironmentInfo) -> String {
    format!(
        "environment {} failed to build in {}: {}; run `by env rebuild` to try again",
        short(key),
        failure.built_by,
        failure.reason.as_deref().unwrap_or("setup failed")
    )
}

/// Record that `key`'s build by `branch` failed because `reason`. A good
/// environment of the key, if any, is left alone.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_failure(
    root: &Path,
    spec: &WorkspaceSpec,
    key: &str,
    recipe: &str,
    place: &str,
    inputs: Vec<EnvironmentInput>,
    branch: &str,
    reason: &str,
) {
    let now = now_ms();
    let info = EnvironmentInfo {
        key: key.to_owned(),
        recipe: recipe.to_owned(),
        place: place.to_owned(),
        state: EnvironmentState::Failed,
        inputs,
        setup: spec.setup.clone(),
        copy: spec.copy.clone(),
        share: spec.share.clone(),
        produced: Vec::new(),
        built_by: branch.to_owned(),
        built_ms: now,
        last_used_ms: now,
        reason: Some(reason.to_owned()),
        snapshot: None,
    };
    let _ = write(&failed_path(root, key), &info);
}

/// A shared path of `spec` that `path` is, or is under.
fn shared_by<'a>(share: &'a [String], path: &str) -> Option<&'a str> {
    share
        .iter()
        .map(|s| s.trim_end_matches('/'))
        .find(|s| path == *s || path.starts_with(&format!("{s}/")))
}

/// Restore `info` into `worktree`: every produced path cloned or copied,
/// every one under `share` linked. `share` is the branch's own
/// configuration, not the one `info` was built with: it is not in the key,
/// so a directory no longer shared is copied. What was in the worktree at
/// those paths and is not tracked (an earlier attempt's) is replaced.
/// Returns how, and the shared paths.
pub(crate) fn restore(
    root: &Path,
    info: &EnvironmentInfo,
    worktree: &Path,
    share: &[String],
) -> Result<(Option<Method>, Vec<String>), String> {
    let source = tree(root, &info.key);
    let mut shared: Vec<String> = share
        .iter()
        .map(|s| s.trim_end_matches('/').to_owned())
        .filter(|s| fs::symlink_metadata(source.join(s)).is_ok())
        .collect();
    shared.sort();
    shared.dedup();
    let copied: Vec<String> = info
        .produced
        .iter()
        .filter(|p| shared_by(&shared, p).is_none())
        .cloned()
        .collect();
    let tracked = tracked(worktree, copied.iter().chain(&shared));
    for rel in copied.iter().chain(&shared) {
        if tracked.contains(rel) {
            continue;
        }
        let target = worktree.join(rel);
        if let Ok(meta) = fs::symlink_metadata(&target) {
            let removed = match meta.is_dir() {
                true => fs::remove_dir_all(&target),
                false => fs::remove_file(&target),
            };
            removed.map_err(|e| format!("could not replace {rel}: {e}"))?;
        }
    }
    let mut placed = materialize::materialize(&source, worktree, &copied, Mode::Copy);
    let linked = materialize::materialize(&source, worktree, &shared, Mode::Share);
    placed.placed.extend(linked.placed);
    placed.skipped.extend(linked.skipped);
    if let Some((path, why)) = placed.skipped.first() {
        return Err(format!(
            "could not restore {path} from environment {}: {why}",
            short(&info.key)
        ));
    }
    touch(root, info);
    Ok((placed.method(), shared))
}

/// Which of `paths` git tracks in `worktree`.
fn tracked<'a>(worktree: &Path, paths: impl Iterator<Item = &'a String>) -> BTreeSet<String> {
    let args: Vec<String> = paths.map(|p| format!(":(literal){p}")).collect();
    if args.is_empty() {
        return BTreeSet::new();
    }
    let out = std::process::Command::new("git")
        .args(["ls-files", "-z", "--cached", "--"])
        .args(&args)
        .current_dir(worktree)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(out) = out else {
        return BTreeSet::new();
    };
    let files: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    // A path is tracked when it, or a file under it, is.
    args.iter()
        .map(|a| a.trim_start_matches(":(literal)").to_owned())
        .filter(|p| {
            files
                .iter()
                .any(|f| f == p || f.starts_with(&format!("{p}/")))
        })
        .collect()
}

/// Remove from `worktree` what a failed setup left at `produced` paths,
/// except `keep` (files copied into it), before the last good build is
/// restored there.
pub(crate) fn clear_failed(worktree: &Path, produced: &[String], keep: &[String]) {
    let tracked = tracked(worktree, produced.iter());
    for rel in produced {
        if tracked.contains(rel) || keep.contains(rel) || !check_relative(rel) {
            continue;
        }
        let target = worktree.join(rel);
        let _ = match fs::symlink_metadata(&target).map(|m| m.is_dir()) {
            Ok(true) => fs::remove_dir_all(&target),
            Ok(false) => fs::remove_file(&target),
            Err(_) => Ok(()),
        };
    }
}

fn check_relative(rel: &str) -> bool {
    materialize::safe_relative(rel).is_some_and(|safe| safe == rel)
}

/// What a finished build captures.
pub(crate) struct Capture<'a> {
    pub root: &'a Path,
    pub spec: &'a WorkspaceSpec,
    pub key: &'a str,
    pub recipe: &'a str,
    pub place: &'a str,
    pub inputs: Vec<EnvironmentInput>,
    pub branch: &'a str,
    /// The worktree setup ran in, when this host sees what it produced.
    pub worktree: Option<&'a Path>,
    pub produced: &'a [String],
    pub snapshot: Option<EnvironmentSnapshot>,
}

/// Capture a build: move what setup produced from the worktree into a
/// staging directory, record it, and rename it into place as `key`'s
/// environment, replacing one of the same key (a rebuild) only then. With
/// `journal`, the step `environment` is journaled first, so recovery can
/// remove a half-built one. On failure what was moved goes back.
///
/// For a sandbox, `capture.snapshot` is the planned snapshot (its handle
/// the name a live branch will give it), journaled before `take` makes it.
/// On failure after `take` succeeded, the snapshot it made comes back with
/// the reason, for the caller to release.
pub(crate) fn capture(
    mut capture: Capture<'_>,
    journal: Option<(&Store, &Fence)>,
    take: Option<&dyn Fn() -> Result<EnvironmentSnapshot, String>>,
) -> Result<EnvironmentInfo, (String, Option<Box<EnvironmentSnapshot>>)> {
    let base = dir(capture.root);
    let staging = base.join(format!(".staging-{}-{}", capture.key, nonce()));
    if let Some((store, fence)) = journal {
        let intent = json!({
            "key": capture.key,
            "staging": staging,
            "worktree": capture.worktree,
            "produced": capture.produced,
            "snapshot": capture.snapshot,
        });
        store
            .backend()
            .begin_step(fence, fence.turn, STEP_ENVIRONMENT, &intent)
            .map_err(|e| (e.to_string(), None))?;
    }
    let result = match take.map(|take| take()) {
        Some(Err(why)) => Err((why, None)),
        taken => {
            let taken = taken.and_then(Result::ok);
            if taken.is_some() {
                capture.snapshot = taken.clone();
            }
            stage(&capture, &staging).map_err(|why| (why, taken.map(Box::new)))
        }
    };
    if let Some((store, fence)) = journal {
        let outcome = match &result {
            Ok(_) => json!({ "key": capture.key }),
            Err((error, _)) => json!({ "error": error }),
        };
        let _ = store
            .backend()
            .finish_step(fence, fence.turn, STEP_ENVIRONMENT, &outcome);
    }
    result
}

fn stage(capture: &Capture<'_>, staging: &Path) -> Result<EnvironmentInfo, String> {
    let into = staging.join(TREE);
    fs::create_dir_all(&into).map_err(|e| format!("could not stage the environment: {e}"))?;
    let mut moved: Vec<String> = Vec::new();
    let undo = |moved: &[String]| {
        if let Some(worktree) = capture.worktree {
            for rel in moved {
                let _ = fs::rename(into.join(rel), worktree.join(rel));
            }
        }
        branchyard_support::cleanup_dir(staging);
    };
    let mut produced = Vec::new();
    if let Some(worktree) = capture.worktree {
        for rel in capture.produced {
            if !check_relative(rel) {
                continue;
            }
            let from = worktree.join(rel);
            if fs::symlink_metadata(&from).is_err() {
                continue;
            }
            let to = into.join(rel);
            if let Some(parent) = to.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    undo(&moved);
                    return Err(format!("could not stage {rel}: {e}"));
                }
            }
            // A rename on one filesystem; a clone or copy across two.
            let placed = fs::rename(&from, &to)
                .map(|()| true)
                .or_else(|_| materialize::clone_or_copy(&from, &to).map(|_| false));
            match placed {
                Ok(true) => moved.push(rel.clone()),
                Ok(false) => {}
                Err(e) => {
                    undo(&moved);
                    return Err(format!("could not stage {rel}: {e}"));
                }
            }
            produced.push(rel.clone());
        }
    }
    let now = now_ms();
    let info = EnvironmentInfo {
        key: capture.key.to_owned(),
        recipe: capture.recipe.to_owned(),
        place: capture.place.to_owned(),
        state: EnvironmentState::Good,
        inputs: capture.inputs.clone(),
        setup: capture.spec.setup.clone(),
        copy: capture.spec.copy.clone(),
        share: capture.spec.share.clone(),
        produced,
        built_by: capture.branch.to_owned(),
        built_ms: now,
        last_used_ms: now,
        reason: None,
        snapshot: capture.snapshot.clone(),
    };
    if let Err(e) = write(&staging.join(MANIFEST), &info) {
        undo(&moved);
        return Err(format!("could not record the environment: {e}"));
    }
    let target = dir(capture.root).join(capture.key);
    let old = dir(capture.root).join(format!(".old-{}-{}", capture.key, nonce()));
    let replaced = fs::symlink_metadata(&target).is_ok();
    if replaced {
        if let Err(e) = fs::rename(&target, &old) {
            undo(&moved);
            return Err(format!("could not replace the environment: {e}"));
        }
    }
    if let Err(e) = fs::rename(staging, &target) {
        if replaced {
            let _ = fs::rename(&old, &target);
        }
        undo(&moved);
        return Err(format!("could not keep the environment: {e}"));
    }
    if replaced {
        branchyard_support::cleanup_dir(&old);
    }
    branchyard_support::cleanup_file(failed_path(capture.root, capture.key));
    Ok(info)
}

/// A pending `environment` step left by an engine that stopped: remove its
/// staging directory, and destroy a planned provider snapshot. What was
/// moved out of the worktree is not put back: the branch's setup did not
/// complete, so its next turn runs it again. Returns what to report.
pub(crate) fn recover_step(yard: &Yard, step: Option<&crate::state::StepRow>) -> Option<String> {
    let step = step?;
    if step.outcome.is_some() {
        return None;
    }
    let key = step.intent.get("key")?.as_str()?.to_owned();
    if let Some(staging) = step.intent.get("staging").and_then(Value::as_str) {
        let staging = Path::new(staging);
        // Only ever a directory of ours.
        if staging.starts_with(dir(&yard.root)) {
            branchyard_support::cleanup_dir(staging);
        }
    }
    let mut said = format!("removed the half-built environment {}", short(&key));
    if let Some(snapshot) = step
        .intent
        .get("snapshot")
        .and_then(|s| serde_json::from_value::<EnvironmentSnapshot>(s.clone()).ok())
    {
        said.push_str(&release_snapshot(yard, &snapshot).map_or_else(
            |e| {
                format!(
                    " (its snapshot {} could not be released: {e})",
                    snapshot.handle
                )
            },
            |()| format!(" and its snapshot {}", snapshot.handle),
        ));
    }
    Some(said)
}

fn release_snapshot(yard: &Yard, snapshot: &EnvironmentSnapshot) -> Result<(), String> {
    let options: crate::Provider =
        serde_json::from_value(snapshot.options.clone()).map_err(|e| e.to_string())?;
    let provider = crate::snapshots::open(yard, &options)?;
    crate::snapshots::release_environment(provider.as_ref(), snapshot)
}

/// Branches whose worktrees link into an environment, and warm pool slots
/// that do, by key.
fn in_use(yard: &Yard) -> BTreeMap<String, Vec<String>> {
    let mut using: BTreeMap<String, Vec<String>> = crate::pool::linking(yard);
    for record in yard.store().list().unwrap_or_default() {
        if let Some(workspace) = &record.workspace {
            if let (Some(key), false) = (&workspace.environment, workspace.spec.share.is_empty()) {
                using
                    .entry(key.clone())
                    .or_default()
                    .push(record.info.name.clone());
            }
        }
    }
    using
}

/// What `by env prune` removed or kept.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pruned {
    /// Environments (and failures) removed, each with why.
    pub removed: Vec<(String, String)>,
    /// Environments that would have gone but were kept, each with why.
    pub kept: Vec<(String, String)>,
}

/// Remove environments beyond the newest `keep` of each recipe, or unused
/// for longer than `max_age`, and failures older than it, never the newest
/// good one of a recipe (its last good build) nor one a branch's worktree
/// links into; with `only`, just those keys (a newest one included). Also
/// removes what stopped builds left behind.
pub(crate) fn prune(yard: &Yard, keep: usize, max_age: Duration, only: &[String]) -> Pruned {
    let root = &yard.root;
    let mut pruned = Pruned::default();
    let now = now_ms();
    let old = |ms: u64| now.saturating_sub(ms) > max_age.as_millis() as u64;
    let using = in_use(yard);
    let all = list(root);
    let mut by_recipe: BTreeMap<&str, Vec<&EnvironmentInfo>> = BTreeMap::new();
    for info in all.iter().filter(|i| i.state == EnvironmentState::Good) {
        by_recipe.entry(&info.recipe).or_default().push(info);
    }
    for infos in by_recipe.values_mut() {
        infos.sort_by(|a, b| b.built_ms.cmp(&a.built_ms));
        for (rank, info) in infos.iter().enumerate() {
            let why = match only.is_empty() {
                false if only.contains(&info.key) => "asked for".to_owned(),
                false => continue,
                true if rank == 0 => continue,
                true if rank >= keep => format!("beyond the newest {keep} of its recipe"),
                true if old(info.last_used_ms) => {
                    format!("unused for more than {} days", max_age.as_secs() / 86400)
                }
                true => continue,
            };
            if let Some(branches) = using.get(&info.key) {
                pruned.kept.push((
                    info.key.clone(),
                    format!("{why}, but linked by {}", branches.join(", ")),
                ));
                continue;
            }
            match remove(yard, info) {
                Ok(()) => pruned.removed.push((info.key.clone(), why)),
                Err(e) => pruned.kept.push((info.key.clone(), e)),
            }
        }
    }
    for info in all.iter().filter(|i| i.state == EnvironmentState::Failed) {
        let asked = only.contains(&info.key);
        if asked || (only.is_empty() && old(info.built_ms)) {
            branchyard_support::cleanup_file(failed_path(root, &info.key));
            pruned
                .removed
                .push((format!("{}.failed", info.key), "a recorded failure".into()));
        }
    }
    // Leftovers of stopped builds: staging and replaced directories whose
    // key nobody is building.
    if let Ok(entries) = fs::read_dir(dir(root)) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".build-") {
                // A rebuild's worktree: its key is not known from its name,
                // so it goes once it is older than any build may run.
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > LOCK_WAIT + Duration::from_secs(60));
                if stale {
                    let _lock = crate::git::lock();
                    let _ = crate::git::run(
                        root,
                        &[
                            "worktree",
                            "remove",
                            "--force",
                            &entry.path().display().to_string(),
                        ],
                    );
                    branchyard_support::cleanup_dir(entry.path());
                }
                continue;
            }
            let rest = name
                .strip_prefix(".staging-")
                .or_else(|| name.strip_prefix(".old-"));
            let Some(rest) = rest else { continue };
            let key = rest.split('-').next().unwrap_or_default();
            if let Ok(Some(_lock)) = KeyLock::try_take(root, key) {
                branchyard_support::cleanup_dir(entry.path());
            }
        }
    }
    pruned
}

/// Remove one environment, under its key's lock, releasing its provider
/// snapshot.
fn remove(yard: &Yard, info: &EnvironmentInfo) -> Result<(), String> {
    let Some(_lock) = KeyLock::try_take(&yard.root, &info.key)? else {
        return Err("it is being built".into());
    };
    if let Some(snapshot) = &info.snapshot {
        release_snapshot(yard, snapshot)?;
    }
    // The lock file stays: another process may be waiting on it.
    fs::remove_dir_all(dir(&yard.root).join(&info.key)).map_err(|e| e.to_string())
}

/// Prune after a build, with the defaults.
pub(crate) fn prune_after_build(yard: &Yard) {
    let _ = prune(yard, DEFAULT_KEEP, DEFAULT_MAX_AGE, &[]);
}

/// What `Yard::rebuild_environment` did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentBuild {
    /// The environment built; `None` when setup failed.
    pub environment: Option<EnvironmentInfo>,
    /// The key it was built for.
    pub key: String,
    /// Setup as it ran.
    pub report: crate::WorkspaceReport,
}

/// Build `spec`'s host environment now, from the repository's `HEAD`, in a
/// temporary worktree, replacing the key's environment only if setup
/// succeeds. Not journaled: a build cut short leaves a `.build-` worktree
/// and a staging directory that `prune` removes.
pub(crate) fn rebuild(yard: &Yard, spec: &WorkspaceSpec) -> Result<EnvironmentBuild, Error> {
    if !spec.prepare || spec.setup.is_empty() {
        return Err(Error::State(
            "only a [workspace] with setup and prepare = true has an environment".into(),
        ));
    }
    if yard.hub.scripts_denied() {
        return Err(Error::State(crate::workspace::DENIED.into()));
    }
    let root = &yard.root;
    let work = dir(root).join(format!(".build-{}", nonce()));
    fs::create_dir_all(dir(root))?;
    {
        let _lock = crate::git::lock();
        crate::git::run(
            root,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                &work.display().to_string(),
                "HEAD",
            ],
        )?;
    }
    let cleanup = || {
        let _lock = crate::git::lock();
        let _ = crate::git::run(
            root,
            &["worktree", "remove", "--force", &work.display().to_string()],
        );
        branchyard_support::cleanup_dir(&work);
    };
    let inputs = inputs(&work, spec);
    let (key, recipe) = key(spec, HOST, &inputs);
    let lock = match KeyLock::take(root, &key, &|| None) {
        Ok(lock) => lock,
        Err(e) => {
            cleanup();
            return Err(Error::State(e));
        }
    };
    let built = build_in(
        yard,
        spec,
        &work,
        "by env rebuild",
        "env-rebuild",
        &key,
        &recipe,
        inputs,
    );
    drop(lock);
    cleanup();
    let built = built?;
    if built.environment.is_some() {
        prune_after_build(yard);
    }
    Ok(EnvironmentBuild {
        environment: built.environment,
        key,
        report: built.report,
    })
}

/// What [`build_in`] did.
pub(crate) struct Built {
    pub environment: Option<EnvironmentInfo>,
    pub report: crate::WorkspaceReport,
    /// What the copy phase placed in the worktree.
    pub copied: Vec<String>,
}

/// Build `key`'s host environment in `work`, a detached worktree of the
/// repository, holding the key's lock: copy files, run setup, and capture
/// what it produced (moved out of `work`), or record the key's failure.
/// `by` names the builder; setup sees `branch` as `BRANCHYARD_BRANCH`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_in(
    yard: &Yard,
    spec: &WorkspaceSpec,
    work: &Path,
    by: &str,
    branch: &str,
    key: &str,
    recipe: &str,
    inputs: Vec<EnvironmentInput>,
) -> Result<Built, Error> {
    let root = &yard.root;
    let copied = crate::workspace::copy(root, work, &spec.copy, None);
    let mut report = crate::workspace::WorkspaceReport::new(crate::WorkspacePhase::Setup, None);
    report.ran_in = Some(crate::RanIn::Host);
    let placed: Vec<String> = copied.copied.clone();
    let before = crate::workspace::untracked(work);
    if !copied.ok {
        report.ok = false;
        report.error = Some(copied.failure());
    } else {
        let env = vec![
            (crate::ENV_BRANCH.to_owned(), branch.to_owned()),
            (crate::ENV_WORKTREE.to_owned(), work.display().to_string()),
            (crate::ENV_ROOT.to_owned(), root.display().to_string()),
        ];
        crate::workspace::run_commands(
            &mut report,
            &spec.setup,
            work,
            &env,
            None,
            crate::workspace::SETUP_TIMEOUT,
            &|_| Ok(()),
            &|| None,
        )?;
    }
    let environment = if report.ok {
        let produced: Vec<String> = crate::workspace::untracked(work)
            .into_iter()
            .filter(|p| !before.contains(p) && !placed.contains(p))
            .collect();
        let captured = capture(
            Capture {
                root,
                spec,
                key,
                recipe,
                place: HOST,
                inputs,
                branch: by,
                worktree: Some(work),
                produced: &produced,
                snapshot: None,
            },
            None,
            None,
        );
        match captured.map_err(|(why, _)| why) {
            Ok(info) => {
                let mut used = EnvironmentUse::new(key, EnvironmentOrigin::Built);
                used.built_by = Some(info.built_by.clone());
                report.environment = Some(Box::new(used));
                Some(info)
            }
            Err(why) => {
                report.ok = false;
                report.error = Some(why);
                None
            }
        }
    } else {
        record_failure(root, spec, key, recipe, HOST, inputs, by, &report.failure());
        None
    };
    Ok(Built {
        environment,
        report,
        copied: placed,
    })
}

/// The host key `spec` has for the repository's checkout at its root.
pub(crate) fn current_key(root: &Path, spec: &WorkspaceSpec) -> String {
    key(spec, HOST, &inputs(root, spec)).0
}

/// A sandboxed branch's environment, when it has one: its key's snapshot
/// on provider `place`, or, when the key's build failed, the last good one
/// of its recipe with the reason.
pub(crate) struct SandboxEnvironment {
    pub key: String,
    pub info: EnvironmentInfo,
    pub reason: Option<String>,
}

/// The environment a sandboxed branch whose worktree is `worktree` starts
/// its sandbox from, on provider `place`.
pub(crate) fn for_sandbox(
    root: &Path,
    spec: &WorkspaceSpec,
    worktree: &Path,
    place: &str,
) -> Option<SandboxEnvironment> {
    if !spec.prepare {
        return None;
    }
    let inputs = inputs(worktree, spec);
    let (key, recipe) = key(spec, place, &inputs);
    if let Some(info) = good(root, &key).filter(|i| i.snapshot.is_some()) {
        return Some(SandboxEnvironment {
            key,
            info,
            reason: None,
        });
    }
    let failure = failed(root, &key)?;
    let info = last_good(root, &recipe, &key).filter(|i| i.snapshot.is_some())?;
    Some(SandboxEnvironment {
        reason: Some(failure_reason(&key, &failure)),
        key,
        info,
    })
}

/// `(key, recipe, inputs)` of a sandboxed branch's environment.
pub(crate) fn sandbox_key(
    spec: &WorkspaceSpec,
    worktree: &Path,
    place: &str,
) -> (String, String, Vec<EnvironmentInput>) {
    let inputs = inputs(worktree, spec);
    let (key, recipe) = key(spec, place, &inputs);
    (key, recipe, inputs)
}

/// After setup ran in a sandboxed branch's sandbox with `prepare`: when it
/// failed, record the key's failure (later branches use the last good
/// build); when it succeeded and the key has no environment yet, keep a
/// live branch of the sandbox, paused, as the key's environment, with what
/// setup produced in a mounted worktree. Notes what happened on `report`;
/// returns the key when it was built.
pub(crate) fn after_sandbox_setup(
    yard: &Yard,
    record: &crate::state::Record,
    fence: &Fence,
    runner: &crate::workspace::Runner<'_>,
    made: &[String],
    report: &mut crate::WorkspaceReport,
) -> Option<String> {
    let crate::workspace::Runner::Sandbox {
        provider,
        name: sandbox,
        mounted,
        ..
    } = runner
    else {
        return None;
    };
    let options = record.provider.as_ref()?;
    let spec = &record.workspace.as_ref()?.spec;
    let root = &yard.root;
    let place = crate::snapshots::provider_key(options);
    let worktree = &record.info.worktree;
    let (key, recipe, inputs) = sandbox_key(spec, worktree, &place);
    if !report.ok {
        record_failure(
            root,
            spec,
            &key,
            &recipe,
            &place,
            inputs,
            &record.info.name,
            &report.failure(),
        );
        return None;
    }
    let mut used = EnvironmentUse::new(&key, EnvironmentOrigin::NotKept);
    let not_kept = |report: &mut crate::WorkspaceReport, mut used: EnvironmentUse, why: String| {
        used.reason = Some(why);
        report.environment = Some(Box::new(used));
        None
    };
    let _lock = match KeyLock::try_take(root, &key) {
        Ok(Some(lock)) => lock,
        Ok(None) => return not_kept(report, used, "another branch is building it".into()),
        Err(why) => return not_kept(report, used, why),
    };
    if good(root, &key).is_some() {
        return not_kept(report, used, "another branch built it meanwhile".into());
    }
    let (method, planned) =
        match crate::snapshots::plan_environment(&provider.capabilities(), sandbox, &key) {
            Ok(plan) => plan,
            Err(why) => return not_kept(report, used, why),
        };
    let planned = EnvironmentSnapshot {
        provider: crate::snapshots::provider_name(options).to_owned(),
        handle: planned,
        method,
        options: serde_json::to_value(options).unwrap_or_default(),
        detail: Value::Null,
    };
    let take = || {
        crate::snapshots::take_environment(*provider, sandbox, &planned.handle).map(
            |(handle, detail)| EnvironmentSnapshot {
                handle,
                detail,
                ..planned.clone()
            },
        )
    };
    let store = yard.store();
    let captured = capture(
        Capture {
            root,
            spec,
            key: &key,
            recipe: &recipe,
            place: &place,
            inputs,
            branch: &record.info.name,
            worktree: mounted.then_some(worktree.as_path()),
            produced: made,
            snapshot: Some(planned.clone()),
        },
        Some((&store, fence)),
        Some(&take),
    );
    match captured {
        Ok(info) => {
            // What setup made moved into the environment: it comes back
            // as copies (a link into this host's `.branchyard` would not
            // resolve in the sandbox).
            if *mounted {
                if let Err(why) = restore(root, &info, worktree, &[]) {
                    report.ok = false;
                    report.error = Some(why);
                }
            }
            used.origin = EnvironmentOrigin::Built;
            used.method = serde_json::to_value(method)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned));
            used.built_by = Some(info.built_by);
            report.environment = Some(Box::new(used));
            prune_after_build(yard);
            Some(key)
        }
        Err((why, taken)) => {
            if let Some(taken) = taken {
                let _ = crate::snapshots::release_environment(*provider, &taken);
            }
            not_kept(report, used, why)
        }
    }
}

pub(crate) fn new_use(key: &str, origin: EnvironmentOrigin) -> EnvironmentUse {
    EnvironmentUse::new(key, origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_follow_the_recipe_and_the_inputs_content() {
        let spec = WorkspaceSpec {
            setup: vec!["pnpm install".into()],
            prepare: true,
            ..WorkspaceSpec::default()
        };
        let a = vec![EnvironmentInput {
            path: "pnpm-lock.yaml".into(),
            digest: "1".into(),
        }];
        let b = vec![EnvironmentInput {
            path: "pnpm-lock.yaml".into(),
            digest: "2".into(),
        }];
        let (ka, ra) = key(&spec, HOST, &a);
        let (kb, rb) = key(&spec, HOST, &b);
        assert_ne!(ka, kb);
        assert_eq!(ra, rb);
        assert_eq!(key(&spec, HOST, &a).0, ka);
        let (kc, rc) = key(&spec, "microsandbox", &a);
        assert_ne!(kc, ka);
        assert_ne!(rc, ra);
        let mut other = spec.clone();
        other.setup.push("pnpm build".into());
        assert_ne!(key(&other, HOST, &a).1, ra);
        // Sharing does not change what setup builds.
        let mut shared = spec.clone();
        shared.share = vec!["node_modules".into()];
        assert_eq!(key(&shared, HOST, &a), (ka, ra));
    }

    #[test]
    fn shared_paths_cover_what_is_under_them() {
        let share = vec!["node_modules/".to_owned()];
        assert_eq!(shared_by(&share, "node_modules"), Some("node_modules"));
        assert_eq!(shared_by(&share, "node_modules/x"), Some("node_modules"));
        assert_eq!(shared_by(&share, "node_modules_x"), None);
    }
}
