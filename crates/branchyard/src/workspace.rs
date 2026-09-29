//! A branch's workspace lifecycle: what makes a fresh worktree ready to
//! work, and what cleans up after it. See `docs/workspace.md`.
//!
//! - **Copy.** Untracked files matching [`WorkspaceSpec::copy`] are copied
//!   from the repository root into the new worktree (a `.env`, say). Only
//!   regular files inside the repository are copied: a glob that could
//!   leave it, a symbolic link (never followed, never recreated), a path
//!   under `.git` or `.branchyard`, and a file reached through a linked
//!   directory outside the repository are refused and named in the event.
//!   Files git tracks are left alone, since the worktree has the branch's
//!   own. Copied files are left out of every snapshot of the branch
//!   ([`branchyard_workspace::Workspace::excluding`]), so they never reach
//!   a candidate even when no ignore rule covers them.
//! - **Setup.** [`WorkspaceSpec::setup`]'s commands run, one after another
//!   with `sh -c`, in the worktree, before the harness starts on the
//!   branch's first turn. A failure fails the branch with the command's
//!   output (its last [`OUTPUT_TAIL`] bytes) in an [`Activity::Workspace`]
//!   event.
//! - **Teardown.** [`WorkspaceSpec::teardown`]'s commands run in the
//!   worktree when the branch is removed, best-effort: a failure is
//!   recorded and the removal goes on.
//!
//! Every command gets `BRANCHYARD_BRANCH`, `BRANCHYARD_WORKTREE`,
//! `BRANCHYARD_ROOT` and `BRANCHYARD_PORT`, a TCP port reserved for the
//! branch in the store ([`crate::state::PortBackend`]), so it is the same
//! on every turn and after a restart, differs from every other branch's
//! in the store, and is released when the branch is removed. The harness
//! gets the same variables.
//!
//! Setup is a journaled step (`setup`), intent before effect. Its commands
//! are recorded as processes of the turn and carry the turn's spawn
//! marker, so recovery kills them when their engine stops. A branch whose
//! setup did not complete keeps [`WorkspaceState::ready`] false, and its
//! next turn runs copy and setup again from the start; setup commands must
//! therefore be idempotent, as package installs are. Recovery never runs
//! them itself.
//!
//! Nothing here decides whether a repository's scripts may run: the caller
//! does ([`crate::TaskOptions::workspace`]), after its own trust decision.
//! [`crate::Yard::deny_workspace_scripts`] makes a yard refuse to run any.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::record::Recorder;
use crate::state::{Begun, Fence, ProcessRow, Record, Store};
use crate::{harness, proc, Activity, Error, Yard};

/// The variable naming the branch's worktree.
pub const ENV_WORKTREE: &str = "BRANCHYARD_WORKTREE";
/// The variable holding the branch's reserved port.
pub const ENV_PORT: &str = "BRANCHYARD_PORT";

/// The ports reserved for branches: 20000 to 29999, below the usual
/// ephemeral range and above the usual development servers.
pub const PORT_RANGE: (u16, u16) = (20000, 29999);

/// The most command output an event keeps: the last bytes.
pub const OUTPUT_TAIL: usize = 16 * 1024;

/// How long setup may run before it is killed and the branch fails.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How long teardown may run before it is killed.
pub const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The journaled step that copies files and runs setup.
pub(crate) const STEP_SETUP: &str = "setup";

/// How often a running script is checked for its deadline and a cancel.
const TICK: Duration = Duration::from_millis(100);

/// What prepares a new branch's worktree and cleans up after it; see the
/// module documentation. Stored with the branch, so every later turn,
/// fork, reincarnation and delegated child uses what was decided when it
/// was created.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSpec {
    /// Globs, relative to the repository root, of untracked files to copy
    /// into each new worktree, such as `.env` or `config/*.local.json`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy: Vec<String>,
    /// Commands run with `sh -c` in each new worktree, in order, before its
    /// first turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup: Vec<String>,
    /// Commands run with `sh -c` in the worktree when the branch is
    /// removed, best-effort.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub teardown: Vec<String>,
    /// What the configuration this came from hashed to when it was trusted,
    /// for display; the engine does not check it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

impl WorkspaceSpec {
    /// Whether it runs any command.
    pub fn has_scripts(&self) -> bool {
        !self.setup.is_empty() || !self.teardown.is_empty()
    }
}

/// A branch's workspace as stored with its record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceState {
    pub spec: WorkspaceSpec,
    /// Copy and setup completed. False until they do; a turn that finds it
    /// false runs them (again) first.
    #[serde(default)]
    pub ready: bool,
    /// The paths copied (a matched directory once), relative to the
    /// worktree; left out of snapshots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copied: Vec<String>,
}

impl WorkspaceState {
    pub fn new(spec: WorkspaceSpec) -> WorkspaceState {
        WorkspaceState {
            spec,
            ready: false,
            copied: Vec::new(),
        }
    }
}

/// The paths a branch's snapshots leave out.
pub(crate) fn excluded(record: &Record) -> Vec<String> {
    record
        .workspace
        .as_ref()
        .map(|w| w.copied.clone())
        .unwrap_or_default()
}

/// Which part of the lifecycle an [`Activity::Workspace`] event reports.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePhase {
    /// Untracked files copied into the new worktree.
    Copy,
    /// The setup commands, before the first turn.
    Setup,
    /// The teardown commands, at removal.
    Teardown,
    /// A named run script, from `by workspace run`.
    Run,
}

/// What one phase of a branch's workspace lifecycle did.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceReport {
    pub phase: WorkspacePhase,
    pub ok: bool,
    /// The commands that ran, in order, up to and including one that failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    /// Paths copied, relative to the repository root; a directory a glob
    /// matched is named once for everything copied under it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copied: Vec<String>,
    /// Paths or globs not copied, each with why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<String>,
    /// The last command's exit code; `None` when it was killed or never
    /// started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The commands' combined standard output and error, at most its last
    /// [`OUTPUT_TAIL`] bytes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
    /// Why it failed when no exit code says so: a timeout, a cancel, a
    /// command that could not start, scripts denied on this yard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub duration_ms: u64,
    /// The branch's reserved port, as the commands saw it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl WorkspaceReport {
    /// A report of `phase` that has not run anything yet.
    pub fn new(phase: WorkspacePhase, port: Option<u16>) -> WorkspaceReport {
        WorkspaceReport {
            phase,
            ok: true,
            commands: Vec::new(),
            copied: Vec::new(),
            refused: Vec::new(),
            exit_code: None,
            output: String::new(),
            error: None,
            duration_ms: 0,
            port,
        }
    }

    /// One line on why it failed, for a branch's status.
    pub fn failure(&self) -> String {
        let what = match self.phase {
            WorkspacePhase::Copy => "copying files".to_owned(),
            WorkspacePhase::Setup | WorkspacePhase::Teardown | WorkspacePhase::Run => format!(
                "`{}`",
                self.commands.last().map(String::as_str).unwrap_or("")
            ),
        };
        let how = match (&self.error, self.exit_code) {
            (Some(error), _) => error.clone(),
            (None, Some(code)) => format!("exited with status {code}"),
            (None, None) => "failed".to_owned(),
        };
        let phase = match self.phase {
            WorkspacePhase::Copy | WorkspacePhase::Setup => "workspace setup",
            WorkspacePhase::Teardown => "workspace teardown",
            WorkspacePhase::Run => "workspace run script",
        };
        format!("{phase} failed: {what} {how}; its output is in the branch's log")
    }
}

/// A branch's workspace as [`crate::Yard::workspace`] reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub branch: String,
    pub worktree: PathBuf,
    /// What the branch was created with; `None` when it has no workspace
    /// lifecycle.
    pub spec: Option<WorkspaceSpec>,
    /// Copy and setup completed.
    pub ready: bool,
    /// Paths copied, relative to the worktree (a matched directory once).
    pub copied: Vec<String>,
    /// The branch's reserved port, if one is reserved.
    pub port: Option<u16>,
}

/// The first port to try for `branch` of the repository at `root`, spread
/// over [`PORT_RANGE`] so branches of different repositories rarely start
/// from the same one.
pub(crate) fn port_start(root: &Path, branch: &str) -> u16 {
    let hash = blake3::hash(format!("{}\0{branch}", root.display()).as_bytes());
    let bytes = hash.as_bytes();
    let n = u16::from_le_bytes([bytes[0], bytes[1]]);
    let (low, high) = PORT_RANGE;
    low + n % (high - low + 1)
}

/// Whether nothing on this host listens on `port`.
pub(crate) fn port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The next port to try after `port`, wrapping within [`PORT_RANGE`].
pub(crate) fn next_port(port: u16) -> u16 {
    let (low, high) = PORT_RANGE;
    match port >= high {
        true => low,
        false => port + 1,
    }
}

/// Reserve `branch`'s port, or return the one it has.
pub(crate) fn reserve_port(store: &Store, root: &Path, branch: &str) -> Result<u16, Error> {
    store
        .ports()
        .reserve_port(branch, port_start(root, branch), &port_free)
}

/// The variables a branch's scripts and harness get.
pub(crate) fn variables(
    yard: &Yard,
    branch: &str,
    worktree: &Path,
    port: Option<u16>,
) -> Vec<(String, String)> {
    let mut vars = vec![
        (crate::ENV_BRANCH.to_owned(), branch.to_owned()),
        (ENV_WORKTREE.to_owned(), worktree.display().to_string()),
        (crate::ENV_ROOT.to_owned(), yard.root.display().to_string()),
    ];
    if let Some(port) = port {
        vars.push((ENV_PORT.to_owned(), port.to_string()));
    }
    vars
}

/// Prepare the worktree of the turn's branch when its workspace is not
/// ready: reserve its port, copy, and run setup, as the journaled `setup`
/// step. `Ok(Err(reason))` fails the branch; the report is recorded
/// either way. Stops early, and fails, when the turn is cancelled or its
/// lease lost.
pub(crate) fn prepare(
    yard: &Yard,
    record: &mut Record,
    fence: &Fence,
    recorder: &mut Recorder,
    lost: &dyn Fn() -> bool,
) -> Result<Result<(), String>, Error> {
    let Some(state) = record.workspace.clone() else {
        return Ok(Ok(()));
    };
    if state.ready {
        return Ok(Ok(()));
    }
    let store = yard.store();
    let name = record.info.name.clone();
    let worktree = record.info.worktree.clone();
    if !worktree.is_dir() {
        // The `create` step failed and already said so.
        return Ok(Ok(()));
    }
    let port = reserve_port(&store, &yard.root, &name)?;
    let marker = format!(
        "{}-{}-{}-setup",
        store.owner().id,
        fence.incarnation,
        fence.generation
    );
    let intent = json!({
        "copy": state.spec.copy,
        "setup": state.spec.setup,
        "port": port,
        "spawn": marker,
        "host": proc::host(),
    });
    // A pending intent is an earlier attempt of this turn that stopped
    // midway; setup is idempotent, so it runs again.
    if let Begun::Done(_) = store
        .backend()
        .begin_step(fence, fence.turn, STEP_SETUP, &intent)?
    {
        return Ok(Ok(()));
    }
    let copied = copy(&yard.root, &worktree, &state.spec.copy, Some(port));
    let copied_ok = copied.ok;
    let copied_paths = copied.copied.clone();
    if !state.spec.copy.is_empty() {
        recorder.record(Activity::Workspace(copied.clone()))?;
    }
    let mut outcome = json!({ "ok": copied_ok, "copied": copied_paths });
    let result = if !copied_ok {
        Err(copied.failure())
    } else if state.spec.setup.is_empty() {
        Ok(())
    } else {
        let mut report = WorkspaceReport::new(WorkspacePhase::Setup, Some(port));
        if yard.hub.scripts_denied() {
            report.ok = false;
            report.error = Some(DENIED.to_owned());
        } else {
            let env = variables(yard, &name, &worktree, Some(port));
            let cancel = || {
                if lost() {
                    return Some("its engine lost the branch's lease".to_owned());
                }
                store
                    .backend()
                    .cancel_requested(fence)
                    .ok()
                    .flatten()
                    .map(|by| format!("cancelled by {by}"))
            };
            let on_spawn = |row: &ProcessRow| store.backend().record_process(fence, row);
            run_commands(
                &mut report,
                &state.spec.setup,
                &worktree,
                &env,
                Some(&marker),
                SETUP_TIMEOUT,
                &on_spawn,
                &cancel,
            )?;
        }
        outcome = json!({
            "ok": report.ok,
            "copied": copied_paths,
            "exit_code": report.exit_code,
            "error": report.error,
        });
        let failed = (!report.ok).then(|| report.failure());
        recorder.record(Activity::Workspace(report))?;
        match failed {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    };
    if let Some(workspace) = record.workspace.as_mut() {
        workspace.copied = copied_paths;
        workspace.ready = result.is_ok();
    }
    store.write_fenced(record, fence)?;
    store
        .backend()
        .finish_step(fence, fence.turn, STEP_SETUP, &outcome)?;
    Ok(result)
}

/// Why a yard that denies workspace scripts did not run them.
pub(crate) const DENIED: &str = "workspace scripts are not allowed here: the server running this \
     branch runs a repository's scripts only when its operator allows them \
     (allow_workspace_scripts)";

/// Run the branch's teardown in its worktree, best-effort, and return its
/// report; `None` when it has none or the worktree is gone. Its processes
/// are recorded under `fence` when one is given, so recovery kills them.
pub(crate) fn teardown(
    yard: &Yard,
    record: &Record,
    fence: Option<&Fence>,
) -> Result<Option<WorkspaceReport>, Error> {
    let Some(state) = &record.workspace else {
        return Ok(None);
    };
    let worktree = &record.info.worktree;
    if state.spec.teardown.is_empty() || !worktree.is_dir() {
        return Ok(None);
    }
    let store = yard.store();
    let port = store.ports().port(&record.info.name)?;
    let mut report = WorkspaceReport::new(WorkspacePhase::Teardown, port);
    if yard.hub.scripts_denied() {
        report.ok = false;
        report.error = Some(DENIED.to_owned());
        return Ok(Some(report));
    }
    let env = variables(yard, &record.info.name, worktree, port);
    let on_spawn = |row: &ProcessRow| match fence {
        Some(fence) => store.backend().record_process(fence, row),
        None => Ok(()),
    };
    run_commands(
        &mut report,
        &state.spec.teardown,
        worktree,
        &env,
        None,
        TEARDOWN_TIMEOUT,
        &on_spawn,
        &|| None,
    )?;
    Ok(Some(report))
}

/// Copy the untracked files `globs` match from `root` into `worktree`. See
/// the module documentation for what is refused. Not ok when a glob itself
/// is refused or a file could not be written.
pub(crate) fn copy(
    root: &Path,
    worktree: &Path,
    globs: &[String],
    port: Option<u16>,
) -> WorkspaceReport {
    let started = Instant::now();
    let mut report = WorkspaceReport::new(WorkspacePhase::Copy, port);
    let (root, worktree) = match (fs::canonicalize(root), fs::canonicalize(worktree)) {
        (Ok(root), Ok(worktree)) => (root, worktree),
        (Err(e), _) | (_, Err(e)) => {
            report.ok = false;
            report.error = Some(format!("could not resolve the repository or worktree: {e}"));
            return report;
        }
    };
    let options = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let mut found = BTreeSet::new();
    let mut matched_dirs = BTreeSet::new();
    for pattern in globs {
        if let Err(why) = check_glob(pattern) {
            report.ok = false;
            report.refused.push(format!("{pattern}: {why}"));
            continue;
        }
        let full = format!(
            "{}/{pattern}",
            glob::Pattern::escape(&root.display().to_string())
        );
        let paths = match glob::glob_with(&full, options) {
            Ok(paths) => paths,
            Err(e) => {
                report.ok = false;
                report.refused.push(format!("{pattern}: {e}"));
                continue;
            }
        };
        for path in paths.flatten() {
            let is_dir = fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
            if let (true, Ok(rel)) = (is_dir, path.strip_prefix(&root)) {
                matched_dirs.insert(rel.display().to_string());
            }
            collect(&root, &path, &mut found, &mut report.refused);
        }
    }
    let tracked = tracked(&root, &found);
    let mut copied = Vec::new();
    for rel in found.into_iter().filter(|rel| !tracked.contains(rel)) {
        match copy_one(&root, &worktree, &rel) {
            Ok(()) => copied.push(rel),
            Err(why) => {
                report.ok = false;
                report.refused.push(format!("{rel}: {why}"));
            }
        }
    }
    // A directory is named whole only when the branch tracks nothing in
    // it: leaving it out of a snapshot must never drop the agent's edits.
    let whole: BTreeSet<String> = matched_dirs
        .into_iter()
        .filter(|dir| tracks_nothing(&worktree, dir))
        .collect();
    report.copied = collapse(copied, &whole);
    report.duration_ms = started.elapsed().as_millis() as u64;
    report
}

/// `files` with those under a directory a glob matched named once, by
/// that directory: what the event lists and what snapshots leave out, so
/// a copied `node_modules` is one path, not thousands.
fn collapse(files: Vec<String>, dirs: &BTreeSet<String>) -> Vec<String> {
    let under = |file: &str| {
        dirs.iter()
            .filter(|dir| file.starts_with(&format!("{dir}/")))
            .min_by_key(|dir| dir.len())
            .cloned()
    };
    let mut named = BTreeSet::new();
    for file in files {
        named.insert(under(&file).unwrap_or(file));
    }
    named.into_iter().collect()
}

/// Why a copy glob is refused, if it is: it must stay inside the
/// repository.
pub fn check_glob(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("is empty".into());
    }
    if pattern.starts_with('/') || pattern.starts_with('~') || pattern.starts_with('\\') {
        return Err("must be relative to the repository root".into());
    }
    if Path::new(pattern)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
        || pattern.split('/').any(|part| part == "..")
    {
        return Err("must not leave the repository ('..')".into());
    }
    if pattern
        .split('/')
        .next()
        .is_some_and(|first| first == ".git" || first == ".branchyard")
    {
        return Err("must not reach into .git or .branchyard".into());
    }
    glob::Pattern::new(pattern).map_err(|e| format!("is not a glob: {e}"))?;
    Ok(())
}

/// Add `path` (a match under `root`), or the regular files under it, to
/// `found` as paths relative to `root`, refusing what may not be copied.
fn collect(root: &Path, path: &Path, found: &mut BTreeSet<String>, refused: &mut Vec<String>) {
    let Ok(rel) = path.strip_prefix(root) else {
        refused.push(format!("{}: is outside the repository", path.display()));
        return;
    };
    let rel_text = rel.display().to_string();
    if rel_text.is_empty() {
        return;
    }
    let first = rel.components().next();
    if matches!(first, Some(Component::Normal(c)) if c == ".git" || c == ".branchyard") {
        return;
    }
    // The directory holding it must resolve inside the repository: a glob
    // can pass through a linked directory.
    let inside = path
        .parent()
        .and_then(|p| fs::canonicalize(p).ok())
        .is_some_and(|p| p.starts_with(root));
    if !inside {
        refused.push(format!(
            "{rel_text}: is reached through a link outside the repository"
        ));
        return;
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        refused.push(format!(
            "{rel_text}: is a symbolic link, which is never copied"
        ));
    } else if meta.is_file() {
        found.insert(rel_text);
    } else if meta.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            refused.push(format!("{rel_text}: could not be read"));
            return;
        };
        let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for entry in entries {
            collect(root, &entry, found, refused);
        }
    } else {
        refused.push(format!("{rel_text}: is not a regular file"));
    }
}

/// Which of `paths` git tracks at `root`.
fn tracked(root: &Path, paths: &BTreeSet<String>) -> BTreeSet<String> {
    let mut tracked = BTreeSet::new();
    let paths: Vec<&String> = paths.iter().collect();
    for chunk in paths.chunks(500) {
        let out = Command::new("git")
            .args(["ls-files", "-z", "--cached", "--"])
            .args(chunk.iter().map(|p| format!(":(literal){p}")))
            .current_dir(root)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        if let Ok(out) = out {
            for path in out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                tracked.insert(String::from_utf8_lossy(path).into_owned());
            }
        }
    }
    tracked
}

/// Whether the worktree's branch tracks no file under `dir`.
fn tracks_nothing(worktree: &Path, dir: &str) -> bool {
    Command::new("git")
        .args(["ls-files", "-z", "--cached", "--"])
        .arg(format!(":(literal){dir}"))
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|out| out.status.success() && out.stdout.is_empty())
}

fn copy_one(root: &Path, worktree: &Path, rel: &str) -> Result<(), String> {
    let source = root.join(rel);
    let target = worktree.join(rel);
    let parent = target
        .parent()
        .ok_or_else(|| "has no parent directory".to_owned())?;
    fs::create_dir_all(parent).map_err(|e| format!("could not create its directory: {e}"))?;
    // A linked directory in the worktree must not carry the file out of it.
    let inside = fs::canonicalize(parent).is_ok_and(|p| p.starts_with(worktree));
    if !inside {
        return Err("its directory in the worktree is a link outside it".into());
    }
    if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink() || m.is_dir()) {
        return Err("the worktree already has a link or directory there".into());
    }
    fs::copy(&source, &target)
        .map(|_| ())
        .map_err(|e| format!("could not be copied: {e}"))
}

/// Run `commands` in order with `sh -c` in `cwd`, stopping at the first
/// that fails, into `report`. Each leads its own process group, which is
/// killed at `timeout` or when `cancel` returns a reason.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_commands(
    report: &mut WorkspaceReport,
    commands: &[String],
    cwd: &Path,
    env: &[(String, String)],
    marker: Option<&str>,
    timeout: Duration,
    on_spawn: &dyn Fn(&ProcessRow) -> Result<(), Error>,
    cancel: &dyn Fn() -> Option<String>,
) -> Result<(), Error> {
    use std::os::unix::process::CommandExt;
    let started = Instant::now();
    let deadline = started + timeout;
    let mut output = Tail::default();
    for command in commands {
        report.commands.push(command.clone());
        let (mut reader, writer) = match std::io::pipe() {
            Ok(pipe) => pipe,
            Err(e) => {
                report.ok = false;
                report.error = Some(format!("could not start: {e}"));
                break;
            }
        };
        let spawned = {
            let mut sh = Command::new("sh");
            sh.arg("-c")
                .arg(command)
                .current_dir(cwd)
                .stdin(Stdio::null())
                .process_group(0);
            for (name, _) in std::env::vars_os() {
                if let Some(name) = name.to_str() {
                    if harness::is_branchyard_variable(name)
                        || harness::is_parent_session_variable(name)
                    {
                        sh.env_remove(name);
                    }
                }
            }
            for (name, value) in env {
                sh.env(name, value);
            }
            if let Some(marker) = marker {
                sh.env(proc::ENV_SPAWN, marker);
            }
            // `sh` holds the pipe's write ends until it is dropped at the
            // end of this block; after that only the child does.
            match writer.try_clone() {
                Ok(out) => {
                    sh.stdout(out).stderr(writer);
                    sh.spawn()
                }
                Err(e) => Err(e),
            }
        };
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                report.ok = false;
                report.error = Some(format!("could not start sh: {e}"));
                break;
            }
        };
        let pid = child.id();
        let start = proc::start_time(pid).unwrap_or_default();
        on_spawn(&ProcessRow {
            pid,
            pgid: pid,
            start: start.clone(),
            host: proc::host().to_owned(),
        })?;
        // The pipe's write ends live only in the child (and its children):
        // the reader ends when the last of them exits.
        let collector = std::thread::spawn(move || {
            let mut tail = Tail::default();
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => tail.push(&buffer[..n]),
                }
            }
            tail
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(e) => break Err(format!("could not wait for it: {e}")),
            }
            if Instant::now() >= deadline {
                proc::kill_group(pid, &start);
                let _ = child.wait();
                break Err(format!("timed out after {}s", timeout.as_secs()));
            }
            if let Some(why) = cancel() {
                proc::kill_group(pid, &start);
                let _ = child.wait();
                break Err(format!("stopped: {why}"));
            }
            std::thread::sleep(TICK);
        };
        // What the command left running in its group would hold the pipe
        // open; it goes with the command.
        proc::kill_group(pid, &start);
        if let Ok(tail) = collector.join() {
            output.push(&tail.bytes);
        }
        match status {
            Ok(status) if status.success() => {
                report.exit_code = status.code();
            }
            Ok(status) => {
                report.ok = false;
                report.exit_code = status.code();
                if status.code().is_none() {
                    report.error = Some("was killed by a signal".into());
                }
                break;
            }
            Err(why) => {
                report.ok = false;
                report.error = Some(why);
                break;
            }
        }
    }
    report.output = output.text();
    report.duration_ms = started.elapsed().as_millis() as u64;
    Ok(())
}

/// The last [`OUTPUT_TAIL`] bytes written.
#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
}

impl Tail {
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > 2 * OUTPUT_TAIL {
            let cut = self.bytes.len() - OUTPUT_TAIL;
            self.bytes.drain(..cut);
        }
    }

    fn text(&self) -> String {
        let start = self.bytes.len().saturating_sub(OUTPUT_TAIL);
        String::from_utf8_lossy(&self.bytes[start..]).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_that_leave_the_repository_are_refused() {
        for bad in [
            "",
            "/etc/passwd",
            "~/x",
            "../x",
            "a/../../x",
            ".git/config",
            ".branchyard/x",
        ] {
            assert!(check_glob(bad).is_err(), "{bad:?}");
        }
        for good in [".env", ".env.*", "config/**/*.local.json", "a..b"] {
            assert!(check_glob(good).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn ports_start_inside_the_range_and_wrap() {
        let (low, high) = PORT_RANGE;
        for name in ["a", "b", "some-branch"] {
            let port = port_start(Path::new("/r"), name);
            assert!((low..=high).contains(&port));
        }
        assert_eq!(next_port(high), low);
        assert_eq!(next_port(low), low + 1);
    }

    #[test]
    fn files_under_a_matched_directory_are_named_by_it() {
        let dirs: BTreeSet<String> = ["config".into(), "config/deep".into()].into();
        let files = vec![
            ".env".into(),
            "config/a.json".into(),
            "config/deep/b.json".into(),
            "configx/c.json".into(),
        ];
        assert_eq!(collapse(files, &dirs), [".env", "config", "configx/c.json"]);
    }

    #[test]
    fn a_tail_keeps_the_last_bytes() {
        let mut tail = Tail::default();
        for _ in 0..10 {
            tail.push(&[b'a'; OUTPUT_TAIL]);
        }
        tail.push(b"end");
        let text = tail.text();
        assert_eq!(text.len(), OUTPUT_TAIL);
        assert!(text.ends_with("end"));
    }

    #[test]
    fn commands_run_in_order_with_their_environment_and_stop_at_a_failure() {
        let dir = std::env::temp_dir().join(format!("by-ws-unit-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut report = WorkspaceReport::new(WorkspacePhase::Setup, Some(20001));
        let env = vec![("BRANCHYARD_PORT".to_owned(), "20001".to_owned())];
        run_commands(
            &mut report,
            &[
                "echo port=$BRANCHYARD_PORT".into(),
                "echo oops >&2; exit 3".into(),
                "echo never".into(),
            ],
            &dir,
            &env,
            None,
            Duration::from_secs(30),
            &|_| Ok(()),
            &|| None,
        )
        .unwrap();
        assert!(!report.ok);
        assert_eq!(report.exit_code, Some(3));
        assert_eq!(report.commands.len(), 2);
        assert!(report.output.contains("port=20001"), "{}", report.output);
        assert!(report.output.contains("oops"));
        assert!(!report.output.contains("never"));
        assert!(report.failure().contains("exited with status 3"));

        let mut slow = WorkspaceReport::new(WorkspacePhase::Setup, None);
        run_commands(
            &mut slow,
            &["sleep 30".into()],
            &dir,
            &[],
            None,
            Duration::from_millis(300),
            &|_| Ok(()),
            &|| None,
        )
        .unwrap();
        assert!(!slow.ok);
        assert!(slow.error.as_deref().unwrap().contains("timed out"));
        let _ = fs::remove_dir_all(&dir);
    }
}
