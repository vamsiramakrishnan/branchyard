//! A task's own repository: for a folder you granted, or for a task with
//! no files. See `docs/task-repos.md`.
//!
//! It lives in `$BRANCHYARD_HOME/tasks/<id>/`:
//!
//! - `git/` is the git directory. Its `main` is the accepted state; for a
//!   folder, `refs/branchyard/folder` is the commit the folder is known to
//!   be at. The folder is the work tree only when Branchyard reads it (its
//!   first snapshot, and the safety check on accept), through `GIT_DIR`,
//!   `GIT_WORK_TREE` and an index of its own in `git/`; nothing is ever
//!   written into the folder except an accepted attempt's changes.
//! - `work/` is the yard's root: a work tree of `git/` that holds only
//!   Branchyard's state (`.branchyard/`) and checks nothing out (its HEAD is
//!   detached at `main`). Attempts are its branches, each in its own
//!   worktree under `work/.branchyard/worktrees/`, so they never touch the
//!   folder.
//! - `accept/` holds the lock and the journal of an accept in progress.
//!
//! Accepting an attempt ([`accept_owned`]) makes `main` its head (or a
//! merge of it), then applies the change from the folder's commit to the
//! new `main` to the folder, path by path, and only if every path it
//! touches still holds what that commit has (or already holds the result):
//! a file changed outside the task since is never overwritten, and the
//! accept is refused with nothing written, naming each conflict. The plan
//! is journaled before anything is written, so an accept cut short is
//! finished by the next one.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use branchyard_workspace::Git;

use super::large::{self, At, ChunkStore, Large, Pointer, DEFAULT_THRESHOLD};
use super::{Task, TaskFiles, TaskView, LEAVE_OUT, TASK_DIR};
use crate::{Activity, BranchStatus, Error, RecordedEvent, Yard};
use branchyard_support::time::now_ms;

/// Where the folder is known to be.
pub(crate) const FOLDER_REF: &str = "refs/branchyard/folder";
const MAIN: &str = "refs/heads/main";
/// The folder's own index, in the git directory.
const INDEX: &str = "branchyard-folder-index";
/// What a folder task leaves out, before the folder's `.branchyardignore`.
const DEFAULT_IGNORES: &str = "\
# Operating systems
.DS_Store
._*
.Spotlight-V100/
.Trashes/
.fseventsd/
Thumbs.db
ehthumbs.db
desktop.ini
$RECYCLE.BIN/
# Editors
*.swp
*.swo
*~
.#*
# Caches and dependencies
node_modules/
__pycache__/
*.pyc
.cache/
.pytest_cache/
.mypy_cache/
.ruff_cache/
.venv/
.tox/
.gradle/
.parcel-cache/
.next/cache/
";

/// A repository made for a task, as its git config describes it.
#[derive(Clone, Debug)]
pub(crate) struct Owned {
    pub id: String,
    pub files: TaskFiles,
    pub large: Large,
    /// `$BRANCHYARD_HOME/tasks/<id>`.
    pub dir: PathBuf,
}

/// The task repository whose work tree or worktree `root` is, if it is
/// one: its git config names the task (`branchyard.task`).
pub(crate) fn owned(root: &Path) -> Option<Owned> {
    let out = Git::new(root)
        .args(["config", "-z", "--get-regexp", r"^branchyard\."])
        .run()
        .ok()?;
    let mut keys = std::collections::BTreeMap::new();
    for entry in out.split('\0').filter(|e| !e.is_empty()) {
        if let Some((key, value)) = entry.split_once('\n') {
            keys.insert(key.to_owned(), value.to_owned());
        }
    }
    let id = keys.get("branchyard.task")?.clone();
    let files = match keys.get("branchyard.files").map(String::as_str) {
        Some("folder") => TaskFiles::Folder {
            folder: PathBuf::from(keys.get("branchyard.folder")?),
        },
        Some("no_files") => TaskFiles::NoFiles,
        _ => return None,
    };
    let threshold = keys
        .get("branchyard.largefilethreshold")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_THRESHOLD);
    let store = match keys.get("branchyard.chunks") {
        Some(dir) => ChunkStore::new(dir),
        None => ChunkStore::at_home(),
    };
    Some(Owned {
        id,
        files,
        large: Large { threshold, store },
        dir: PathBuf::from(keys.get("branchyard.home")?),
    })
}

/// What [`create`] makes.
#[derive(Clone, Debug, Default)]
pub struct NewTask {
    /// What is asked.
    pub prompt: String,
    /// The folder granted; `None` for a task with no files.
    pub folder: Option<PathBuf>,
    /// Who asks; by default git's identity, else the login name.
    pub by: Option<String>,
    /// The policy and grants it runs under, in one line.
    pub policy: String,
    /// Files of at least this many bytes are chunked (default
    /// [`DEFAULT_THRESHOLD`]).
    pub large_threshold: Option<u64>,
    /// The chunk store (default `<home>/chunks`).
    pub chunks: Option<PathBuf>,
    /// Branchyard's per-user directory (default [`super::home`]): the task
    /// is made in its `tasks/`.
    pub home: Option<PathBuf>,
}

/// What [`super::accept`] did.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Accepted {
    pub task: String,
    pub attempt: String,
    /// The branch the attempt went into: the target of the merge, or `main`
    /// for a task with a repository of its own.
    pub target: String,
    pub previous: String,
    pub commit: String,
    /// The folder the change was applied to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// Paths written in the folder.
    #[serde(default)]
    pub written: Vec<String>,
    /// Paths removed from the folder.
    #[serde(default)]
    pub removed: Vec<String>,
}

fn git_err(error: branchyard_workspace::GitError) -> Error {
    crate::git::error(error)
}

fn run(dir: &Path, args: &[&str]) -> Result<String, Error> {
    Ok(Git::new(dir)
        .args(args)
        .run()
        .map_err(git_err)?
        .trim()
        .to_owned())
}

fn tasks_dir(home: &Path) -> PathBuf {
    home.join("tasks")
}

/// Write the ignore rules of a task's repository: Branchyard's state, the
/// defaults, and the folder's `.branchyardignore`. The folder's own
/// `.gitignore` files apply as they are.
fn write_ignores(git_dir: &Path, folder: Option<&Path>) -> Result<(), Error> {
    let mut text = String::from(
        "# Written by Branchyard for a task's repository; see docs/task-repos.md.\n\
         .branchyard/\n",
    );
    text.push_str(DEFAULT_IGNORES);
    if let Some(own) = folder.and_then(|f| fs::read_to_string(f.join(".branchyardignore")).ok()) {
        text.push_str("# The folder's .branchyardignore\n");
        text.push_str(&own);
        if !own.ends_with('\n') {
            text.push('\n');
        }
    }
    super::write_atomic(&git_dir.join("info").join("exclude"), text.as_bytes())
}

/// Commit what the folder holds now (ignore rules applied, large files as
/// pointers), with `parent` as its parent; `parent` itself when nothing
/// changed.
fn snapshot_folder(
    git_dir: &Path,
    folder: &Path,
    large: &Large,
    parent: Option<&str>,
    message: &str,
) -> Result<String, Error> {
    write_ignores(git_dir, Some(folder))?;
    let at = At::folder(git_dir, folder, &git_dir.join(INDEX));
    large::stage(&at, large)?;
    at.git()
        .args(["add", "--all", "--", "."])
        .run()
        .map_err(git_err)?;
    let tree = at.git().arg("write-tree").run().map_err(git_err)?;
    let tree = tree.trim();
    if let Some(parent) = parent {
        if run(git_dir, &["rev-parse", &format!("{parent}^{{tree}}")])? == tree {
            return Ok(parent.to_owned());
        }
    }
    commit(
        git_dir,
        tree,
        &parent.into_iter().collect::<Vec<_>>(),
        message,
    )
}

fn commit(git_dir: &Path, tree: &str, parents: &[&str], message: &str) -> Result<String, Error> {
    let mut git = Git::new(git_dir).args(["commit-tree", tree]);
    for parent in parents {
        git = git.args(["-p", parent]);
    }
    Ok(git
        .args(["-m", message])
        .run()
        .map_err(git_err)?
        .trim()
        .to_owned())
}

/// Make a task with a repository of its own: for `new.folder`, or with no
/// files. Its first commit is the folder as it is now (or an empty tree),
/// on `main`. Returns the task and the yard its attempts run in.
pub fn create(new: &NewTask) -> Result<(Task, Yard), Error> {
    let folder = match &new.folder {
        Some(folder) => {
            let folder = fs::canonicalize(folder).map_err(|e| {
                Error::State(format!("cannot use the folder {}: {e}", folder.display()))
            })?;
            if !folder.is_dir() {
                return Err(Error::State(format!(
                    "{} is not a folder",
                    folder.display()
                )));
            }
            if folder.join(".git").exists() {
                return Err(Error::Unsupported(format!(
                    "{} is a git repository; run by there, and each attempt is a branch of it",
                    folder.display()
                )));
            }
            if folder.starts_with(new.home.clone().unwrap_or_else(super::home)) {
                return Err(Error::Unsupported(format!(
                    "{} is inside Branchyard's own directory",
                    folder.display()
                )));
            }
            Some(folder)
        }
        None => None,
    };
    let home = new.home.clone().unwrap_or_else(super::home);
    let id = super::new_id()?;
    let dir = tasks_dir(&home).join(&id);
    let git_dir = dir.join("git");
    let work = dir.join("work");
    fs::create_dir_all(&dir)?;
    let made = (|| {
        Git::new(&dir)
            .args([
                "init",
                "--quiet",
                "--initial-branch=main",
                "--separate-git-dir",
            ])
            .arg(&git_dir)
            .arg(&work)
            .run()
            .map_err(git_err)?;
        let threshold = new.large_threshold.unwrap_or(DEFAULT_THRESHOLD);
        let chunks = new.chunks.clone().unwrap_or_else(|| home.join("chunks"));
        let mut config = vec![
            ("branchyard.task", id.clone()),
            ("branchyard.home", dir.display().to_string()),
            ("branchyard.largeFileThreshold", threshold.to_string()),
            ("branchyard.chunks", chunks.display().to_string()),
            ("core.quotePath", "false".into()),
        ];
        match &folder {
            Some(folder) => {
                config.push(("branchyard.files", "folder".into()));
                config.push(("branchyard.folder", folder.display().to_string()));
            }
            None => config.push(("branchyard.files", "no_files".into())),
        }
        for (key, value) in [
            ("user.name", "Branchyard"),
            ("user.email", "branchyard@localhost"),
        ] {
            if !Git::new(&work)
                .args(["config", "--get", key])
                .test()
                .map_err(git_err)?
            {
                config.push((key, value.into()));
            }
        }
        for (key, value) in &config {
            Git::new(&work)
                .args(["config", key, value])
                .run()
                .map_err(git_err)?;
        }
        let large = Large {
            threshold,
            store: ChunkStore::new(chunks),
        };
        let title = super::title(&new.prompt);
        let first = match &folder {
            Some(folder) => snapshot_folder(
                &git_dir,
                folder,
                &large,
                None,
                &format!(
                    "{title}\n\nThe folder {} when the task began.\n",
                    folder.display()
                ),
            )?,
            None => {
                write_ignores(&git_dir, None)?;
                let empty = Git::new(&git_dir)
                    .args(["mktree"])
                    .stdin(Vec::new())
                    .run()
                    .map_err(git_err)?;
                commit(
                    &git_dir,
                    empty.trim(),
                    &[],
                    &format!("{title}\n\nA task with no files.\n"),
                )?
            }
        };
        run(&git_dir, &["update-ref", MAIN, &first])?;
        if folder.is_some() {
            run(&git_dir, &["update-ref", FOLDER_REF, &first])?;
        }
        run(&work, &["update-ref", "--no-deref", "HEAD", &first])?;
        let yard = Yard::open(&work)?;
        let files = match &folder {
            Some(folder) => TaskFiles::Folder {
                folder: folder.clone(),
            },
            None => TaskFiles::NoFiles,
        };
        let task = Task {
            id: id.clone(),
            title,
            asked: new.prompt.clone(),
            by: new
                .by
                .clone()
                .unwrap_or_else(|| super::who(&work, &crate::TaskOptions::default())),
            policy: new.policy.clone(),
            created_ms: now_ms(),
            files,
            origin: "task".into(),
        };
        super::save(&work, &task)?;
        Ok((task, yard))
    })();
    if made.is_err() {
        branchyard_support::cleanup_dir(&dir);
    }
    made
}

/// The directories of every task with a repository of its own.
fn home_dirs(home: &Path) -> Result<Vec<PathBuf>, Error> {
    let entries = match fs::read_dir(tasks_dir(home)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("work").is_dir())
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// Every task with a repository of its own in `home` (normally
/// [`super::home`]), oldest first.
pub fn home_tasks(home: &Path) -> Result<Vec<TaskView>, Error> {
    let mut views = Vec::new();
    for dir in home_dirs(home)? {
        let yard = Yard::open(dir.join("work"))?;
        views.extend(super::list(&yard)?);
    }
    Ok(views)
}

/// The yard of the task with a repository of its own that `key` names:
/// its ID, a unique prefix of it (at least four characters), or one of its
/// attempts. `None` when no such task is in `home`.
pub fn open_home(home: &Path, key: &str) -> Result<Option<Yard>, Error> {
    let upper = key.to_ascii_uppercase();
    let dirs = home_dirs(home)?;
    let by_id: Vec<&PathBuf> = dirs
        .iter()
        .filter(|d| {
            let name = d.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name == upper || (upper.len() >= 4 && name.starts_with(&upper))
        })
        .collect();
    match by_id.as_slice() {
        [one] => return Yard::open(one.join("work")).map(Some),
        [] => {}
        many => {
            return Err(Error::State(format!(
                "{key} names {} tasks; give more of the ID",
                many.len()
            )))
        }
    }
    for dir in &dirs {
        if super::task_of(&dir.join("work"), key)?.is_some() {
            return Yard::open(dir.join("work")).map(Some);
        }
    }
    Ok(None)
}

/// Remove the task with a repository of its own that `key` names: its git
/// directory, its attempts' worktrees and its state. Refused while an
/// attempt runs. The folder is never touched, and chunks stay in the store
/// (other tasks may share them).
pub fn remove_home(home: &Path, key: &str) -> Result<TaskView, Error> {
    let yard = open_home(home, key)?.ok_or_else(|| Error::State(format!("no task {key}")))?;
    let owned = owned(yard.root()).ok_or_else(|| {
        Error::State(format!(
            "{} is not a task's repository",
            yard.root().display()
        ))
    })?;
    let view = super::view(&yard, &owned.id)?;
    if let Some(running) = view
        .attempts
        .iter()
        .find(|a| matches!(a.status, Some(BranchStatus::Running)))
    {
        return Err(Error::Running(running.name.clone()));
    }
    let _lock = crate::DirLock::acquire(&owned.dir.join("accept"), "accepting this task")?;
    drop(yard);
    fs::remove_dir_all(&owned.dir)?;
    Ok(view)
}

/// An accept in progress, journaled before the folder is written.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
    attempt: String,
    /// `main` before.
    main: String,
    /// `main` after.
    new: String,
    /// The commit the folder was at; `None` for a task with no files.
    folder_from: Option<String>,
}

fn journal_path(owned: &Owned) -> PathBuf {
    owned.dir.join("accept").join("journal.json")
}

fn is_ancestor(dir: &Path, a: &str, b: &str) -> Result<bool, Error> {
    Git::new(dir)
        .args(["merge-base", "--is-ancestor", a, b])
        .test()
        .map_err(git_err)
}

/// Accept `attempt` of `task`, whose repository is its own: `main` becomes
/// the attempt's head (fast-forward), or a merge of it when another attempt
/// was accepted since; then, for a folder, the change from the folder's
/// commit to the new `main` is applied to the folder (see the module
/// documentation). The attempt is recorded `merged` into `main`.
pub fn accept_owned(yard: &Yard, task: &Task, attempt: &str) -> Result<Accepted, Error> {
    let owned = owned(yard.root()).ok_or_else(|| {
        Error::State(format!(
            "{} is not a task's repository",
            yard.root().display()
        ))
    })?;
    let _lock = crate::DirLock::acquire(&owned.dir.join("accept"), "accepting this task")?;
    recover_accept(yard, &owned)?;
    let root = yard.root().to_path_buf();
    let (record, lease) = crate::ops::hold(yard, attempt)?;
    if let BranchStatus::Merged { target, .. } = &record.info.status {
        return Err(Error::AlreadyMerged {
            target: target.clone(),
        });
    }
    // The attempt's record at the checkpoint it is at: its files, and its
    // conversation in `.task/`; its branch's head if it has none.
    let head = match super::record_at(yard, &record, record.checkpoint)? {
        Some(commit) => commit,
        None => crate::git::commit(&root, &format!("refs/heads/{}", record.info.git_branch))?
            .ok_or_else(|| Error::Git(format!("{} does not exist", record.info.git_branch)))?,
    };
    let main = crate::git::commit(&root, MAIN)?
        .ok_or_else(|| Error::Git("the task's repository has no main".into()))?;
    if is_ancestor(&root, &head, &main)? {
        return Err(Error::AlreadyMerged {
            target: "main".into(),
        });
    }
    let new = match is_ancestor(&root, &main, &head)? {
        true => head.clone(),
        false => merge(&root, &main, &head, attempt)?,
    };
    let folder_from = match &owned.files {
        TaskFiles::Folder { .. } => Some(
            crate::git::commit(&root, FOLDER_REF)?
                .ok_or_else(|| Error::Git(format!("the task's repository has no {FOLDER_REF}")))?,
        ),
        _ => None,
    };
    let journal = Journal {
        attempt: attempt.to_owned(),
        main,
        new,
        folder_from,
    };
    // Refused here, nothing has been written.
    let plan = plan_for(&root, &owned, &journal)?;
    super::write_atomic(&journal_path(&owned), &super::to_json(&journal))?;
    crate::checkpoint::fault("accept-after-intent");
    let (written, removed) = apply(&root, &owned, &plan)?;
    crate::checkpoint::fault("accept-after-apply");
    finish(yard, &owned, &journal, Some((record, lease)))?;
    Ok(Accepted {
        task: task.id.clone(),
        attempt: attempt.to_owned(),
        target: "main".into(),
        previous: journal.main,
        commit: journal.new,
        folder: match &owned.files {
            TaskFiles::Folder { folder } => Some(folder.clone()),
            _ => None,
        },
        written,
        removed,
    })
}

/// A merge commit of `head` into `main`, made without a work tree. The
/// task's record (`.task/`) is the accepted attempt's; a conflict anywhere
/// else refuses it.
fn merge(root: &Path, main: &str, head: &str, attempt: &str) -> Result<String, Error> {
    let (out, args) = Git::new(root)
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            main,
            head,
        ])
        .output()
        .map_err(git_err)?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = text.lines();
    let tree = lines.next().unwrap_or("").to_owned();
    let conflicts: Vec<String> = lines.filter(|l| !l.is_empty()).map(str::to_owned).collect();
    let ours = |path: &String| path == TASK_DIR || path.starts_with(&format!("{TASK_DIR}/"));
    match out.status.code() {
        Some(0 | 1) if conflicts.iter().all(ours) => {
            // The record as the attempt has it, whatever `main` had.
            let mut top: Vec<String> = Git::new(root)
                .args(["ls-tree", "-z", &tree])
                .run()
                .map_err(git_err)?
                .split('\0')
                .filter(|e| !e.is_empty() && !e.ends_with(&format!("\t{TASK_DIR}")))
                .map(str::to_owned)
                .collect();
            let record = Git::new(root)
                .args(["ls-tree", "-z", head, "--", TASK_DIR])
                .run()
                .map_err(git_err)?;
            top.extend(
                record
                    .split('\0')
                    .filter(|e| !e.is_empty())
                    .map(str::to_owned),
            );
            let mut input = Vec::new();
            for entry in top {
                input.extend_from_slice(entry.as_bytes());
                input.push(0);
            }
            let tree = Git::new(root)
                .args(["mktree", "-z"])
                .stdin(input)
                .run()
                .map_err(git_err)?;
            commit(
                root,
                tree.trim(),
                &[main, head],
                &format!("Accept {attempt}\n"),
            )
        }
        Some(1) => Err(Error::Conflict {
            files: conflicts.into_iter().filter(|p| !ours(p)).collect(),
        }),
        _ => Err(git_err(branchyard_workspace::git::failed(args, &out))),
    }
}

/// Finish a journaled accept: `main`, the folder's ref and the attempt's
/// record. Repeating it is harmless.
fn finish(
    yard: &Yard,
    owned: &Owned,
    journal: &Journal,
    held: Option<(crate::state::Record, crate::state::Lease)>,
) -> Result<(), Error> {
    let root = yard.root();
    let main = crate::git::commit(root, MAIN)?.unwrap_or_default();
    if main == journal.main {
        run(root, &["update-ref", MAIN, &journal.new, &journal.main])?;
    } else if main != journal.new {
        return Err(Error::TargetMoved {
            expected: journal.main.clone(),
            actual: Some(main),
        });
    }
    if journal.folder_from.is_some() {
        run(root, &["update-ref", FOLDER_REF, &journal.new])?;
    }
    run(root, &["update-ref", "--no-deref", "HEAD", &journal.new])?;
    let held = match held {
        Some(held) => Some(held),
        None => match crate::ops::hold(yard, &journal.attempt) {
            Ok(held) => Some(held),
            Err(Error::UnknownBranch(_)) => None,
            Err(error) => return Err(error),
        },
    };
    if let Some((mut record, lease)) = held {
        record.info.status = BranchStatus::Merged {
            target: "main".into(),
            commit: journal.new.clone(),
        };
        let event = RecordedEvent {
            at_ms: now_ms(),
            activity: Activity::Status(record.info.status.clone()),
        };
        lease.finish(Some(&record), Some(&event))?;
    }
    branchyard_support::cleanup_file(journal_path(owned));
    Ok(())
}

/// Finish an accept a stopped process left journaled.
fn recover_accept(yard: &Yard, owned: &Owned) -> Result<(), Error> {
    let Some(journal) = super::read_json::<Journal>(&journal_path(owned))? else {
        return Ok(());
    };
    let plan = plan_for(yard.root(), owned, &journal)?;
    apply(yard.root(), owned, &plan)?;
    finish(yard, owned, &journal, None)
}

/// What a path holds: absent, a file with its mode and blob, or a link.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    mode: String,
    blob: String,
}

/// One change to make in the folder.
#[derive(Clone, Debug)]
struct Change {
    path: String,
    old: Option<Entry>,
    new: Option<Entry>,
}

/// The changes from `journal.folder_from` to `journal.new` (leaving
/// `.task/` and submodules out), checked against the folder: each path
/// must hold what the old commit has, or already what the new one has.
/// Refused, naming every conflict, otherwise.
fn plan_for(root: &Path, owned: &Owned, journal: &Journal) -> Result<Vec<Change>, Error> {
    let (TaskFiles::Folder { folder }, Some(from)) = (&owned.files, &journal.folder_from) else {
        return Ok(Vec::new());
    };
    let raw = Git::new(root)
        .args([
            "diff",
            "--raw",
            "-z",
            "--no-renames",
            "--no-abbrev",
            from,
            &journal.new,
            "--",
        ])
        .arg(LEAVE_OUT)
        .run_bytes()
        .map_err(git_err)?;
    let fields: Vec<String> = raw
        .split(|b| *b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    let zero = |s: &str| s.bytes().all(|b| b == b'0');
    let mut changes = Vec::new();
    let mut i = 0;
    while i + 1 < fields.len() {
        let meta = fields[i].trim_start_matches(':').to_owned();
        let path = fields[i + 1].clone();
        i += 2;
        let parts: Vec<&str> = meta.split(' ').collect();
        let [old_mode, new_mode, old_blob, new_blob, ..] = parts.as_slice() else {
            continue;
        };
        let entry = |mode: &str, blob: &str| {
            (!zero(mode)).then(|| Entry {
                mode: mode.to_owned(),
                blob: blob.to_owned(),
            })
        };
        let (old, new) = (entry(old_mode, old_blob), entry(new_mode, new_blob));
        // Submodules are not files to write.
        if [&old, &new]
            .iter()
            .any(|e| e.as_ref().is_some_and(|e| e.mode == "160000"))
        {
            continue;
        }
        changes.push(Change { path, old, new });
    }
    let removed: std::collections::BTreeSet<String> = changes
        .iter()
        .filter(|c| c.new.is_none())
        .map(|c| c.path.clone())
        .collect();
    let mut conflicts = Vec::new();
    let mut plan = Vec::new();
    for change in changes {
        let disk = folder.join(&change.path);
        if holds(root, &disk, change.new.as_ref())? {
            continue;
        }
        if !holds(root, &disk, change.old.as_ref())? {
            conflicts.push(format!(
                "{}: {}",
                change.path,
                match (&change.old, disk_kind(&disk)) {
                    (None, _) => "created in the folder since the task began",
                    (Some(_), "absent") => "removed from the folder since the task began",
                    _ => "changed in the folder since the task began",
                }
            ));
            continue;
        }
        if change.new.is_some() {
            let mut parent = Path::new(&change.path).parent();
            while let Some(dir) = parent.filter(|d| !d.as_os_str().is_empty()) {
                let at = folder.join(dir);
                let relative = dir.to_string_lossy();
                if fs::symlink_metadata(&at).is_ok_and(|m| !m.is_dir())
                    && !removed.contains(relative.as_ref() as &str)
                {
                    conflicts.push(format!(
                        "{}: {relative} is a file in the folder, where this needs a folder",
                        change.path
                    ));
                }
                parent = dir.parent();
            }
        }
        plan.push(change);
    }
    if !conflicts.is_empty() {
        return Err(Error::Denied(format!(
            "accepting would overwrite changes made in {} outside the task; nothing was \
             written. Keep or move them, then accept again:\n  {}",
            folder.display(),
            conflicts.join("\n  ")
        )));
    }
    Ok(plan)
}

fn disk_kind(path: &Path) -> &'static str {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => "directory",
        Ok(meta) if meta.file_type().is_symlink() => "link",
        Ok(_) => "file",
        Err(_) => "absent",
    }
}

fn blob(root: &Path, id: &str) -> Result<Vec<u8>, Error> {
    Git::new(root)
        .args(["cat-file", "blob", id])
        .run_bytes()
        .map_err(git_err)
}

/// Whether `disk` holds `entry` (absent for `None`): the same bytes, by the
/// blob's hash or, for a pointer, the large file's hash; the same link
/// target. Permission bits are not compared.
fn holds(root: &Path, disk: &Path, entry: Option<&Entry>) -> Result<bool, Error> {
    let meta = fs::symlink_metadata(disk);
    let Some(entry) = entry else {
        return Ok(meta.is_err());
    };
    let Ok(meta) = meta else {
        return Ok(false);
    };
    if entry.mode == "120000" {
        return Ok(meta.file_type().is_symlink()
            && fs::read_link(disk)?.as_os_str().as_encoded_bytes() == blob(root, &entry.blob)?);
    }
    if !meta.is_file() {
        return Ok(false);
    }
    if let Some(pointer) = Pointer::parse(&blob(root, &entry.blob)?) {
        return Ok(meta.len() == pointer.size && large::hash_file(disk)? == pointer.blake3);
    }
    let id = Git::new(root)
        .args(["hash-object", "--no-filters", "--"])
        .arg(disk)
        .run()
        .map_err(git_err)?;
    Ok(id.trim() == entry.blob)
}

/// Make the planned changes in the folder: removals first, then each file
/// written to a temporary name beside it and renamed over it.
fn apply(root: &Path, owned: &Owned, plan: &[Change]) -> Result<(Vec<String>, Vec<String>), Error> {
    let TaskFiles::Folder { folder } = &owned.files else {
        return Ok((Vec::new(), Vec::new()));
    };
    let mut written = Vec::new();
    let mut removed = Vec::new();
    for change in plan.iter().filter(|c| c.new.is_none()) {
        let disk = folder.join(&change.path);
        if fs::symlink_metadata(&disk).is_ok() {
            fs::remove_file(&disk)?;
        }
        // Folders the removal emptied go too; a folder with anything left
        // stays.
        let mut parent = disk.parent();
        while let Some(dir) = parent.filter(|d| d.starts_with(folder) && *d != folder.as_path()) {
            if fs::remove_dir(dir).is_err() {
                break;
            }
            parent = dir.parent();
        }
        removed.push(change.path.clone());
    }
    for change in plan {
        let Some(new) = &change.new else {
            continue;
        };
        let disk = folder.join(&change.path);
        if let Some(parent) = disk.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = blob(root, &new.blob)?;
        if new.mode == "120000" {
            let temp = large::temp_beside(&disk);
            link(&bytes, &temp)?;
            fs::rename(&temp, &disk)?;
        } else if let Some(pointer) = Pointer::parse(&bytes) {
            owned
                .large
                .store
                .restore_file(&pointer, &disk, new.mode == "100755")?;
        } else {
            let temp = large::temp_beside(&disk);
            let wrote = fs::write(&temp, &bytes)
                .map_err(Error::from)
                .and_then(|()| large::set_executable(&temp, new.mode == "100755"))
                .and_then(|()| {
                    if fs::symlink_metadata(&disk).is_ok_and(|m| m.is_dir()) {
                        fs::remove_dir_all(&disk)?;
                    }
                    fs::rename(&temp, &disk).map_err(Error::from)
                });
            if wrote.is_err() {
                branchyard_support::cleanup_file(&temp);
            }
            wrote?;
        }
        written.push(change.path.clone());
    }
    Ok((written, removed))
}

#[cfg(unix)]
fn link(target: &[u8], at: &Path) -> Result<(), Error> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), at)?;
    Ok(())
}

#[cfg(not(unix))]
fn link(_: &[u8], _: &Path) -> Result<(), Error> {
    Err(Error::Unsupported("symbolic links need a Unix host".into()))
}
