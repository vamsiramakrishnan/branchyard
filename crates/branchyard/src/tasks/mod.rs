//! Tasks: what was asked, by whom and under what policy, owning its
//! attempts. See `docs/task-repos.md`.
//!
//! A task is recorded where its branches live, in the yard's state
//! (`.branchyard/tasks/<id>/`): its [`Task`], and one marker per attempt.
//! Every top-level branch belongs to one: `run` starts a task with one
//! attempt, `fan` and a routed run one with several, a map one whose items
//! are its attempts, and a fork joins its parent's.
//!
//! Each checkpoint of an attempt gets a record commit beside it,
//! `refs/branchyard/<attempt>/<incarnation>/record-<N>`: its tree is
//! checkpoint N's files plus `.task/` (`task.toml`, what was asked;
//! `conversation/<turn>.jsonl`, each turn's prompt, answer, approvals and
//! events; `effects.jsonl`, the effect ledger's entries, see
//! [`effects_snapshot`]), and its parents are the record it continues and
//! the checkpoint. The attempt's branch and candidate stay exactly as they
//! were, so merges, pull requests, diffs and `try` never see `.task/`. A
//! rewind or a fork moves the files and the conversation together: the
//! record of checkpoint N is the conversation as of N, and the next turn's
//! record continues it.
//!
//! A task may also have a repository of its own, under
//! `$BRANCHYARD_HOME/tasks/<id>/` (see [`folder`]): for a folder you granted
//! (`git/` is the git directory, the folder its work tree, and nothing is
//! written into the folder until an attempt is accepted), or for a task with
//! no files. Large files there are chunked ([`large`]).

pub mod large;

mod folder;
mod repo;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use branchyard_workspace::Git;

use crate::state::{now_ms, Record};
use crate::{
    Activity, BranchInfo, BranchStatus, Checkpoint, Error, Event, RecordedEvent, TaskOptions, Yard,
};

pub use folder::{accept_owned, create, home_tasks, open_home, remove_home, Accepted, NewTask};
pub use large::{ChunkStore, Pointer};
pub use repo::{reachable_chunks, TaskRepo};

/// The directory of a task's record, at the top of a record commit.
pub const TASK_DIR: &str = ".task";
/// The pathspec that leaves [`TASK_DIR`] out of a diff.
pub const LEAVE_OUT: &str = ":(top,exclude).task";

/// The `format` line of every `.task/task.toml` Branchyard writes.
pub const FORMAT: &str = "branchyard-task/1";
/// The variable that moves Branchyard's per-user directory (default
/// `~/.branchyard`): task repositories and the chunk store.
pub const ENV_HOME: &str = "BRANCHYARD_HOME";

/// `$BRANCHYARD_HOME`, or `~/.branchyard`.
pub fn home() -> PathBuf {
    match std::env::var_os(ENV_HOME).filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".branchyard"),
    }
}

/// What a task's files are.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskFiles {
    /// A repository you already have: each attempt is a branch of it.
    Repository,
    /// A folder you granted, with a git directory of its own outside it.
    Folder { folder: PathBuf },
    /// No files: the conversation and its results are the repository.
    NoFiles,
}

impl TaskFiles {
    pub fn label(&self) -> &'static str {
        match self {
            TaskFiles::Repository => "repository",
            TaskFiles::Folder { .. } => "folder",
            TaskFiles::NoFiles => "no files",
        }
    }
}

/// A task: what was asked, by whom, under what policy, and since when.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// A ULID: sortable by creation, unique without coordination.
    pub id: String,
    /// The first line of what was asked, shortened.
    pub title: String,
    /// What was asked, in full.
    pub asked: String,
    /// Who asked: the gateway actor, else git's `user.name <user.email>`,
    /// else the login name.
    pub by: String,
    /// The policy and grants the first attempt ran under, in one line.
    pub policy: String,
    pub created_ms: u64,
    pub files: TaskFiles,
    /// What started it: `run`, `fan`, `map`, `fork` or `task`.
    pub origin: String,
}

/// One attempt of a task, as [`list`] and [`view`] show it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttemptView {
    pub name: String,
    /// `None` once the branch has been removed.
    pub status: Option<BranchStatus>,
    pub harness: Option<String>,
    pub turns: u32,
    /// The checkpoint the branch is at.
    pub checkpoint: Option<u32>,
    pub candidate: Option<String>,
    pub worktree: Option<PathBuf>,
    /// The attempt this one was forked from, and at which checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// The record commit of the checkpoint it is at: its files and
    /// `.task/` as of then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<String>,
    /// The conversation files that record holds, in turn order.
    pub conversation: Vec<String>,
}

/// A task with its attempts and where its repository is.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskView {
    #[serde(flatten)]
    pub task: Task,
    pub attempts: Vec<AttemptView>,
    /// The git directory holding the task's history.
    pub repository: PathBuf,
    /// For a task with a repository of its own: what `main` holds, the
    /// accepted state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted: Option<String>,
    /// For a folder: the commit the folder is known to be at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_at: Option<String>,
}

/// A marker for one attempt, in the task's registry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct AttemptMarker {
    name: String,
    added_ms: u64,
    /// For a fork: the record it starts from (its parent's at the fork).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inherit: Option<String>,
}

/// A task a new branch is about to join: an existing one, or one to be
/// saved with its first attempt.
#[derive(Clone, Debug)]
pub(crate) struct Joining {
    pub task: Task,
    /// For a fork: the record commit its conversation continues.
    pub inherit: Option<String>,
}

fn registry(root: &Path) -> PathBuf {
    crate::state::dir(root).join("tasks")
}

fn index_dir(root: &Path) -> PathBuf {
    crate::state::dir(root).join("task-attempts")
}

/// A branch name as one file name: `/` and `%` escaped.
fn encode(name: &str) -> String {
    name.replace('%', "%25").replace('/', "%2F")
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = large::temp_beside(path);
    let written = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_data()?;
        fs::rename(&temp, path)
    })();
    if written.is_err() {
        branchyard_support::cleanup_file(&temp);
    }
    Ok(written?)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::State(format!("{} is unreadable: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn to_json<T: Serialize>(value: &T) -> Vec<u8> {
    let mut text = serde_json::to_vec_pretty(value).unwrap_or_default();
    text.push(b'\n');
    text
}

/// Save `task` in the yard at `root`, unless it is there already.
pub(crate) fn save(root: &Path, task: &Task) -> Result<(), Error> {
    let path = registry(root).join(&task.id).join("task.json");
    if path.is_file() {
        return Ok(());
    }
    write_atomic(&path, &to_json(task))
}

/// The task `id` recorded in the yard at `root`.
pub(crate) fn load(root: &Path, id: &str) -> Result<Option<Task>, Error> {
    if id.is_empty() || id.contains(['/', '.']) {
        return Ok(None);
    }
    read_json(&registry(root).join(id).join("task.json"))
}

/// Record `name` as an attempt of `joining`'s task, saving the task first
/// if it is new.
pub(crate) fn attach(root: &Path, joining: &Joining, name: &str) -> Result<(), Error> {
    save(root, &joining.task)?;
    let marker = AttemptMarker {
        name: name.to_owned(),
        added_ms: now_ms(),
        inherit: joining.inherit.clone(),
    };
    let dir = registry(root).join(&joining.task.id).join("attempts");
    write_atomic(
        &dir.join(format!("{}.json", encode(name))),
        &to_json(&marker),
    )?;
    write_atomic(
        &index_dir(root).join(encode(name)),
        joining.task.id.as_bytes(),
    )
}

/// The task branch `name` is an attempt of, if any.
pub fn task_of(root: &Path, name: &str) -> Result<Option<Task>, Error> {
    let id = match fs::read_to_string(index_dir(root).join(encode(name))) {
        Ok(id) => id,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    load(root, id.trim())
}

/// Forget that `name` is an attempt, as its branch is removed. Its marker
/// in the task stays, so the task still lists it, as removed.
pub(crate) fn forget(root: &Path, name: &str) {
    branchyard_support::cleanup_file(index_dir(root).join(encode(name)));
}

fn marker(root: &Path, id: &str, name: &str) -> Result<Option<AttemptMarker>, Error> {
    read_json(
        &registry(root)
            .join(id)
            .join("attempts")
            .join(format!("{}.json", encode(name))),
    )
}

fn attempt_names(root: &Path, id: &str) -> Result<Vec<String>, Error> {
    let dir = registry(root).join(id).join("attempts");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut markers = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Some(marker) = read_json::<AttemptMarker>(&path)? {
            markers.push(marker);
        }
    }
    markers.sort_by(|a, b| (a.added_ms, &a.name).cmp(&(b.added_ms, &b.name)));
    Ok(markers.into_iter().map(|m| m.name).collect())
}

/// Every task recorded in the yard at `root`, oldest first.
fn tasks_in(root: &Path) -> Result<Vec<Task>, Error> {
    let entries = match fs::read_dir(registry(root)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut tasks = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some(task) = load(root, &name)? {
            tasks.push(task);
        }
    }
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(tasks)
}

/// A new ULID: 48 bits of milliseconds and 80 random bits, in Crockford's
/// base 32.
pub fn new_id() -> String {
    use ring::rand::SecureRandom;
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut random = [0u8; 10];
    let _ = ring::rand::SystemRandom::new().fill(&mut random);
    let mut bytes = [0u8; 16];
    bytes[6..].copy_from_slice(&random);
    let value = ((now_ms() as u128 & ((1 << 48) - 1)) << 80) | u128::from_be_bytes(bytes);
    (0..26)
        .map(|i| ALPHABET[((value >> (125 - 5 * i)) & 31) as usize] as char)
        .collect()
}

/// `2026-10-03T12:00:00Z` for milliseconds since the epoch.
pub fn utc(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The first line of `prompt`, at most 72 characters.
fn title(prompt: &str) -> String {
    let line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    match line.char_indices().nth(72) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

/// Who asked, for a task's record.
fn who(root: &Path, options: &TaskOptions) -> String {
    if let Some(actor) = &options.actor {
        return actor.subject.clone();
    }
    let config = |key: &str| {
        Git::new(root)
            .args(["config", "--get", key])
            .run()
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    match (config("user.name"), config("user.email")) {
        (Some(name), Some(email)) => format!("{name} <{email}>"),
        (Some(name), None) => name,
        (None, Some(email)) => email,
        (None, None) => std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".into()),
    }
}

/// The task a new top-level branch joins: `options.join_task`, the task a
/// repository of its own was made for, or a new one started by `origin`.
pub(crate) fn joining(
    yard: &Yard,
    prompt: &str,
    options: &TaskOptions,
    origin: &str,
) -> Result<Joining, Error> {
    let owned = folder::owned(&yard.root);
    let id = options
        .join_task
        .clone()
        .or_else(|| owned.as_ref().map(|o| o.id.clone()));
    if let Some(id) = id {
        let mut task = load(&yard.root, &id)?
            .ok_or_else(|| Error::State(format!("no task {id} is recorded here")))?;
        // A task made before its first attempt takes that attempt's policy.
        if task.policy.is_empty() {
            task.policy = options.policy.summary();
            write_atomic(
                &registry(&yard.root).join(&task.id).join("task.json"),
                &to_json(&task),
            )?;
        }
        return Ok(Joining {
            task,
            inherit: None,
        });
    }
    Ok(Joining {
        task: Task {
            id: new_id(),
            title: title(prompt),
            asked: prompt.to_owned(),
            by: who(&yard.root, options),
            policy: options.policy.summary(),
            created_ms: now_ms(),
            files: TaskFiles::Repository,
            origin: origin.to_owned(),
        },
        inherit: None,
    })
}

/// The task a fork of `parent` (at its checkpoint `at`, else where it is)
/// joins: the caller's, else its parent's, else a new one. Its
/// conversation continues the parent's record there.
pub(crate) fn joining_fork(
    yard: &Yard,
    parent: &str,
    at: Option<u32>,
    prompt: &str,
    options: &TaskOptions,
) -> Result<Joining, Error> {
    let mut joining = match (&options.join_task, task_of(&yard.root, parent)?) {
        (None, Some(task)) => Joining {
            task,
            inherit: None,
        },
        _ => joining(yard, prompt, options, "fork")?,
    };
    if task_of(&yard.root, parent)?.is_some_and(|t| t.id == joining.task.id) {
        let record = yard.store().read(parent)?;
        joining.inherit = record_at(yard, &record, at.or(record.checkpoint))?;
    }
    Ok(joining)
}

/// The effect ledger's entries (`docs/effects.md`) for attempt `branch` as
/// of the commit about to be made, as JSON lines, written to
/// `.task/effects.jsonl`. `None` when the ledger cannot be read, which
/// leaves an empty file; the ledger stays the source of truth.
pub fn effects_snapshot(yard: &Yard, branch: &str) -> Option<Vec<u8>> {
    let entries = yard.effects(Some(branch)).ok()?;
    let mut out = Vec::new();
    for entry in &entries {
        serde_json::to_writer(&mut out, entry).ok()?;
        out.push(b'\n');
    }
    Some(out)
}

/// The conversation files in `commit`'s `.task/conversation/`, in turn
/// order.
fn conversation_in(dir: &Path, commit: &str) -> Vec<String> {
    let out = Git::new(dir)
        .args(["ls-tree", "--name-only", commit, "--"])
        .arg(format!("{TASK_DIR}/conversation/"))
        .run()
        .unwrap_or_default();
    let mut names: Vec<String> = out
        .lines()
        .filter_map(|l| l.rsplit('/').next())
        .filter(|n| n.ends_with(".jsonl"))
        .map(str::to_owned)
        .collect();
    names.sort_by_key(|n| {
        n.trim_end_matches(".jsonl")
            .parse::<u64>()
            .unwrap_or(u64::MAX)
    });
    names
}

/// The task's record as `.task/task.toml`, for attempt `info`.
fn task_toml(task: &Task, info: &BranchInfo) -> String {
    let mut doc = toml_edit::DocumentMut::new();
    doc["format"] = toml_edit::value(FORMAT);
    doc["id"] = toml_edit::value(&task.id);
    doc["title"] = toml_edit::value(&task.title);
    doc["asked"] = toml_edit::value(&task.asked);
    doc["by"] = toml_edit::value(&task.by);
    doc["policy"] = toml_edit::value(&task.policy);
    doc["created"] = toml_edit::value(utc(task.created_ms));
    doc["files"] = toml_edit::value(match &task.files {
        TaskFiles::Repository => "repository",
        TaskFiles::Folder { .. } => "folder",
        TaskFiles::NoFiles => "no_files",
    });
    if let TaskFiles::Folder { folder } = &task.files {
        doc["folder"] = toml_edit::value(folder.display().to_string());
    }
    doc["origin"] = toml_edit::value(&task.origin);
    doc["attempt"] = toml_edit::value(&info.name);
    doc["harness"] = toml_edit::value(&info.harness);
    doc.to_string()
}

/// One turn's conversation: a summary line (the prompt, the answer, the
/// approvals), then every event of the turn as recorded.
fn conversation_bytes(turn: u32, name: &str, events: &[RecordedEvent]) -> Vec<u8> {
    let start = events
        .iter()
        .rposition(|e| matches!(e.activity, Activity::Prompt(_)))
        .unwrap_or(0);
    let events = &events[start..];
    let mut prompt = String::new();
    let mut answer = String::new();
    let mut approvals = Vec::new();
    for event in events {
        match &event.activity {
            Activity::Prompt(text) => prompt = text.clone(),
            Activity::Harness(Event::MessageDelta { text, .. }) => answer.push_str(text),
            Activity::Decision {
                tool,
                allowed,
                source,
                ..
            } => approvals.push(json!({ "tool": tool, "allowed": allowed, "source": source })),
            _ => {}
        }
    }
    let mut out = serde_json::to_vec(&json!({
        "turn": turn,
        "attempt": name,
        "prompt": prompt,
        "answer": answer,
        "approvals": approvals,
    }))
    .unwrap_or_default();
    out.push(b'\n');
    for event in events {
        if let Ok(line) = serde_json::to_vec(event) {
            out.extend_from_slice(&line);
            out.push(b'\n');
        }
    }
    out
}

/// Just before a turn's snapshot (never when it is replayed): in a task's
/// own repository, large files become pointers ([`large::stage`]).
pub(crate) fn before_snapshot(yard: &Yard, info: &BranchInfo) -> Result<(), Error> {
    if let Some(owned) = folder::owned(&yard.root) {
        large::stage(&large::At::new(&info.worktree), &owned.large)?;
    }
    Ok(())
}

/// `refs/branchyard/<name>/<incarnation>/record-<turn>`, beside checkpoint
/// `git_ref` (`.../turn-<turn>`).
pub(crate) fn record_ref(git_ref: &str) -> String {
    match git_ref.rsplit_once("/turn-") {
        Some((dir, turn)) => format!("{dir}/record-{turn}"),
        None => format!("{git_ref}-record"),
    }
}

/// The record commit of checkpoint `turn` of the branch `record` is: for
/// 0 (or none), the record a fork started from.
pub(crate) fn record_at(
    yard: &Yard,
    record: &Record,
    turn: Option<u32>,
) -> Result<Option<String>, Error> {
    let name = &record.info.name;
    match turn.filter(|n| *n > 0) {
        Some(turn) => {
            let events = crate::record::read(&yard.store(), name)?;
            let Some(checkpoint) = crate::checkpoint::recorded(&events)
                .into_iter()
                .find(|c| c.turn == turn)
            else {
                return Ok(None);
            };
            crate::git::commit(&yard.root, &record_ref(&checkpoint.git_ref))
        }
        None => Ok(match task_of(&yard.root, name)? {
            Some(task) => marker(&yard.root, &task.id, name)?.and_then(|m| m.inherit),
            None => None,
        }),
    }
}

/// `-c` options giving git an identity where it has none, as Branchyard's
/// own snapshots do.
fn identity(dir: &Path) -> Vec<&'static str> {
    let mut args = Vec::new();
    for (key, value) in [
        ("user.name", "user.name=Branchyard"),
        ("user.email", "user.email=branchyard@localhost"),
    ] {
        if !Git::new(dir)
            .args(["config", "--get", key])
            .test()
            .unwrap_or(false)
        {
            args.extend(["-c", value]);
        }
    }
    args
}

fn git_out(dir: &Path, args: &[&str]) -> Result<String, Error> {
    Git::new(dir).args(args).run().map_err(crate::git::error)
}

fn mktree(dir: &Path, entries: &[String]) -> Result<String, Error> {
    let mut input = Vec::new();
    for entry in entries {
        input.extend_from_slice(entry.as_bytes());
        input.push(0);
    }
    Ok(Git::new(dir)
        .args(["mktree", "-z"])
        .stdin(input)
        .run()
        .map_err(crate::git::error)?
        .trim()
        .to_owned())
}

/// The entries of the tree `rev` (`commit` or `commit:path`), as `ls-tree
/// -z` gives them; none when it does not exist.
fn entries(dir: &Path, rev: &str) -> Vec<String> {
    Git::new(dir)
        .args(["ls-tree", "-z", rev])
        .run()
        .map(|out| {
            out.split('\0')
                .filter(|e| !e.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn entry_name(entry: &str) -> &str {
    entry.split_once('\t').map_or("", |(_, name)| name)
}

/// The highest turn number among `conversation/<n>.jsonl` names.
fn last_turn(names: &[String]) -> u32 {
    names
        .iter()
        .filter_map(|n| n.trim_end_matches(".jsonl").parse::<u32>().ok())
        .max()
        .unwrap_or(0)
}

/// After checkpoint `checkpoint` of the branch `record` is (an attempt of
/// a task) was recorded: its record commit, continuing the record of the
/// checkpoint it came after (or the one a fork started from), with this
/// turn's conversation added. Returns the commit; `None` for a branch that
/// is no task's attempt.
pub(crate) fn record_checkpoint(
    yard: &Yard,
    record: &Record,
    checkpoint: &Checkpoint,
) -> Result<Option<String>, Error> {
    let root = &yard.root;
    let info = &record.info;
    let Some(task) = task_of(root, &info.name)? else {
        return Ok(None);
    };
    let previous = record_at(yard, record, checkpoint.after)?;
    let inherited = match task_of(root, &info.name)? {
        Some(_) => marker(root, &task.id, &info.name)?.and_then(|m| m.inherit),
        None => None,
    };
    // A fork's turns are numbered after those of the record it started
    // from.
    let offset = inherited
        .as_deref()
        .map(|commit| last_turn(&conversation_in(root, commit)))
        .unwrap_or(0);
    let turn = offset + checkpoint.turn;
    let events = crate::record::read(&yard.store(), &info.name)?;
    let blob = |bytes: Vec<u8>| -> Result<String, Error> {
        Ok(Git::new(root)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(bytes)
            .run()
            .map_err(crate::git::error)?
            .trim()
            .to_owned())
    };
    let mut conversation: Vec<String> = previous
        .as_deref()
        .map(|p| entries(root, &format!("{p}:{TASK_DIR}/conversation")))
        .unwrap_or_default();
    let name = format!("{turn}.jsonl");
    conversation.retain(|e| entry_name(e) != name);
    conversation.push(format!(
        "100644 blob {}\t{name}",
        blob(conversation_bytes(turn, &info.name, &events))?
    ));
    let effects = effects_snapshot(yard, &info.name).unwrap_or_default();
    let dir = mktree(
        root,
        &[
            format!(
                "100644 blob {}\ttask.toml",
                blob(task_toml(&task, info).into_bytes())?
            ),
            format!("100644 blob {}\teffects.jsonl", blob(effects)?),
            format!("040000 tree {}\tconversation", mktree(root, &conversation)?),
        ],
    )?;
    let mut top: Vec<String> = entries(root, &checkpoint.commit)
        .into_iter()
        .filter(|e| entry_name(e) != TASK_DIR)
        .collect();
    top.push(format!("040000 tree {dir}\t{TASK_DIR}"));
    let tree = mktree(root, &top)?;
    let mut args = vec!["commit-tree".to_owned(), tree];
    if let Some(previous) = previous.or(inherited) {
        args.extend(["-p".to_owned(), previous]);
    }
    args.extend(["-p".to_owned(), checkpoint.commit.clone()]);
    args.extend([
        "-m".to_owned(),
        format!(
            "{}: turn {} of {}\n\nThe task's record at checkpoint {}.\n",
            task.id, turn, info.name, checkpoint.turn
        ),
    ]);
    let commit = Git::new(root)
        .args(identity(root))
        .args(&args)
        .run()
        .map_err(crate::git::error)?
        .trim()
        .to_owned();
    git_out(
        root,
        &["update-ref", &record_ref(&checkpoint.git_ref), &commit],
    )?;
    Ok(Some(commit))
}

/// After a worktree was created or reset in a task's own repository: its
/// large files, from their pointers.
pub(crate) fn after_checkout(yard: &Yard, worktree: &Path) -> Result<(), Error> {
    if let Some(owned) = folder::owned(&yard.root) {
        large::restore(&large::At::new(worktree), &owned.large)?;
    }
    Ok(())
}

/// Before a reset of `worktree` in a task's own repository: the large
/// files changed since their last checkpoint (which a reset would lose,
/// and `git status` cannot see), after which the reset may replace them.
pub(crate) fn before_reset(yard: &Yard, worktree: &Path) -> Result<Vec<String>, Error> {
    match folder::owned(&yard.root) {
        Some(_) => large::dirty(&large::At::new(worktree)),
        None => Ok(Vec::new()),
    }
}

/// Clear what [`large::stage`] marked, so a reset may write those paths.
pub(crate) fn release(yard: &Yard, worktree: &Path) -> Result<(), Error> {
    match folder::owned(&yard.root) {
        Some(_) => large::release(&large::At::new(worktree)),
        None => Ok(()),
    }
}

/// The view of `task` in `yard`.
fn view_of(yard: &Yard, task: Task) -> Result<TaskView, Error> {
    let store = yard.store();
    let mut attempts = Vec::new();
    for name in attempt_names(&yard.root, &task.id)? {
        let attempt = match store.read(&name) {
            Ok(record) if task_of(&yard.root, &name)?.is_some_and(|t| t.id == task.id) => {
                let at = record_at(yard, &record, record.checkpoint)?;
                AttemptView {
                    conversation: at
                        .as_deref()
                        .map(|c| conversation_in(&yard.root, c))
                        .unwrap_or_default(),
                    record: at,
                    name,
                    status: Some(record.info.status.clone()),
                    harness: Some(record.info.harness.clone()),
                    turns: record.info.turns,
                    checkpoint: record.checkpoint,
                    candidate: record.info.candidate.as_ref().map(|c| c.commit.clone()),
                    worktree: Some(record.info.worktree.clone()),
                    forked_from: record
                        .info
                        .parent
                        .clone()
                        .filter(|_| record.info.depth == 0),
                }
            }
            _ => AttemptView {
                name,
                status: None,
                harness: None,
                turns: 0,
                checkpoint: None,
                candidate: None,
                worktree: None,
                forked_from: None,
                record: None,
                conversation: Vec::new(),
            },
        };
        attempts.push(attempt);
    }
    let owned = folder::owned(&yard.root);
    let accepted = match &owned {
        Some(_) => crate::git::commit(&yard.root, "refs/heads/main")?,
        None => None,
    };
    let folder_at = match &task.files {
        TaskFiles::Folder { .. } => crate::git::commit(&yard.root, folder::FOLDER_REF)?,
        _ => None,
    };
    Ok(TaskView {
        task,
        attempts,
        repository: crate::git::common_dir(&yard.root)?,
        accepted,
        folder_at,
    })
}

/// Every task recorded in `yard`, oldest first.
pub fn list(yard: &Yard) -> Result<Vec<TaskView>, Error> {
    tasks_in(&yard.root)?
        .into_iter()
        .map(|task| view_of(yard, task))
        .collect()
}

/// The task `key` names in `yard`: its ID, a unique prefix of it (at least
/// four characters, any case), or one of its attempts.
pub fn find(yard: &Yard, key: &str) -> Result<Task, Error> {
    if let Some(task) = load(&yard.root, key)? {
        return Ok(task);
    }
    if let Some(task) = task_of(&yard.root, key)? {
        return Ok(task);
    }
    let upper = key.to_ascii_uppercase();
    let matches: Vec<Task> = match upper.len() >= 4 {
        true => tasks_in(&yard.root)?
            .into_iter()
            .filter(|t| t.id.starts_with(&upper))
            .collect(),
        false => Vec::new(),
    };
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap_or_else(|| unreachable!())),
        0 => Err(Error::State(format!(
            "no task {key}: give its ID (or a prefix of at least 4 characters) or an attempt's \
             name; `by task ls` lists them"
        ))),
        n => Err(Error::State(format!(
            "{key} names {n} tasks; give more of the ID"
        ))),
    }
}

/// The view of the task `key` names.
pub fn view(yard: &Yard, key: &str) -> Result<TaskView, Error> {
    let task = find(yard, key)?;
    view_of(yard, task)
}

/// The attempt of `view` an operation is about: `attempt` if given (it
/// must be one), else the only one whose branch still exists.
pub fn pick_attempt<'a>(
    view: &'a TaskView,
    attempt: Option<&str>,
) -> Result<&'a AttemptView, Error> {
    let live: Vec<&AttemptView> = view
        .attempts
        .iter()
        .filter(|a| a.status.is_some())
        .collect();
    match attempt {
        Some(name) => view
            .attempts
            .iter()
            .find(|a| a.name == name && a.status.is_some())
            .ok_or_else(|| {
                Error::State(format!(
                    "{name} is not a current attempt of task {}; its attempts are {}",
                    view.task.id,
                    names_of(&live)
                ))
            }),
        None => match live.as_slice() {
            [one] => Ok(one),
            [] => Err(Error::State(format!(
                "task {} has no attempts left",
                view.task.id
            ))),
            _ => Err(Error::State(format!(
                "task {} has {} attempts ({}); name one with --attempt",
                view.task.id,
                live.len(),
                names_of(&live)
            ))),
        },
    }
}

fn names_of(attempts: &[&AttemptView]) -> String {
    match attempts.is_empty() {
        true => "none".into(),
        false => attempts
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// Remove the task `key` names from `yard`: each attempt's branch, as
/// `by rm` removes it (refused while one runs), then its record. A task
/// with a repository of its own is removed with [`remove_home`]. The
/// folder of a folder task is never touched.
pub fn remove(yard: &Yard, key: &str) -> Result<TaskView, Error> {
    let view = view(yard, key)?;
    if folder::owned(&yard.root).is_some() {
        return Err(Error::Unsupported(format!(
            "task {} has a repository of its own; remove it with tasks::remove_home",
            view.task.id
        )));
    }
    if let Some(running) = view
        .attempts
        .iter()
        .find(|a| matches!(a.status, Some(BranchStatus::Running)))
    {
        return Err(Error::Running(running.name.clone()));
    }
    for attempt in view.attempts.iter().filter(|a| a.status.is_some()) {
        yard.remove(&attempt.name)?;
    }
    for attempt in &view.attempts {
        forget(&yard.root, &attempt.name);
    }
    let dir = registry(&yard.root).join(&view.task.id);
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    Ok(view)
}

/// Accept attempt `attempt` (or the only one) of the task `key` names:
/// in a repository you already have, merge its candidate into `into` (the
/// current branch by default) as `by merge` does (the record is never on it); in
/// a task's own repository, see [`accept_owned`].
pub fn accept(
    yard: &Yard,
    key: &str,
    attempt: Option<&str>,
    into: Option<&str>,
) -> Result<Accepted, Error> {
    let view = view(yard, key)?;
    let attempt = pick_attempt(&view, attempt)?.name.clone();
    if folder::owned(&yard.root).is_some() {
        return accept_owned(yard, &view.task, &attempt);
    }
    let target = match into {
        Some(target) => target.to_owned(),
        None => yard.current_branch()?.ok_or_else(|| {
            Error::State("the checkout's HEAD is detached; name the target with --into".into())
        })?,
    };
    let merged = yard.merge(&attempt, &target)?;
    Ok(Accepted {
        task: view.task.id,
        attempt,
        target: merged.target,
        previous: merged.previous,
        commit: merged.commit,
        folder: None,
        written: Vec::new(),
        removed: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_ulids_in_creation_order() {
        let a = new_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_id();
        assert_eq!(a.len(), 26);
        assert!(a
            .bytes()
            .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase()));
        assert!(a < b, "{a} {b}");
    }

    #[test]
    fn times_are_utc() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(1_791_028_800_000), "2026-10-03T12:00:00Z");
    }

    #[test]
    fn titles_are_one_short_line() {
        assert_eq!(title("\n  Fix the parser\nand more"), "Fix the parser");
        assert_eq!(title(&"x".repeat(80)).chars().count(), 73);
    }

    #[test]
    fn names_encode_to_one_file_name() {
        assert_eq!(encode("a/b%c"), "a%2Fb%25c");
    }
}
