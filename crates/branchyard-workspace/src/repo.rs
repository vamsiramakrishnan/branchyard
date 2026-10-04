//! Repositories, branch workspaces, and candidates.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::git::{self, identity_args, is_object_id, Git, GitError};
use crate::BranchName;

/// Config key (under `branch.by/<name>.`) holding the commit a workspace
/// branch was created from. `git branch -D` removes it with the branch.
const BASE_KEY: &str = "branchyardBase";

static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// A process-unique suffix for scratch paths.
pub(crate) fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{}-{nanos:08x}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

/// A full commit object ID.
///
/// Values produced by this crate are always full IDs that resolved to a
/// commit. A hand-built value is checked again wherever it matters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Commit(pub String);

impl Commit {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Commit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Line counts between two trees. Binary files count as changed files with
/// no lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// An exact result to integrate: `head` is a commit on `branch` descending
/// from `base`. Identity is the commit ID, not the branch, which may move on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub branch: BranchName,
    pub base: Commit,
    pub head: Commit,
    pub stat: DiffStat,
}

/// A local, non-bare git repository, addressed through one of its working
/// trees.
#[derive(Debug, Clone)]
pub struct Repository {
    pub(crate) root: PathBuf,
    pub(crate) scratch: PathBuf,
}

impl Repository {
    /// Opens the working tree containing `path` and records its top level.
    pub fn open(path: &Path) -> Result<Repository, GitError> {
        let not_work_tree = || GitError::NotAWorkTree(path.to_path_buf());
        let out = match Git::new(path)
            .args(["rev-parse", "--is-inside-work-tree", "--show-toplevel"])
            .run()
        {
            Ok(out) => out,
            Err(GitError::Failed { .. }) => return Err(not_work_tree()),
            Err(e) => return Err(e),
        };
        let mut lines = out.lines();
        if lines.next() != Some("true") {
            return Err(not_work_tree());
        }
        let root = lines
            .next()
            .filter(|l| !l.is_empty())
            .ok_or_else(not_work_tree)?;
        Ok(Repository {
            root: PathBuf::from(root),
            scratch: std::env::temp_dir(),
        })
    }

    /// The top level of the working tree this repository was opened from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where temporary integration worktrees are created (default: the
    /// system temporary directory). A crash can leave one behind; it is a
    /// detached worktree that references no branch.
    pub fn set_scratch_dir(&mut self, dir: impl Into<PathBuf>) {
        self.scratch = dir.into();
    }

    /// Resolves `rev` to the full ID of the commit it names.
    pub fn resolve(&self, rev: &str) -> Result<Commit, GitError> {
        resolve_in(&self.root, rev)
    }

    /// The branch checked out in this working tree (`main`, not
    /// `refs/heads/main`), or `None` when HEAD is detached.
    pub fn current_branch(&self) -> Result<Option<String>, GitError> {
        current_branch_in(&self.root)
    }

    /// Whether `ancestor` is reachable from `descendant` (a commit is its own
    /// ancestor). Recovery uses this to learn whether a candidate reached a
    /// target after an interrupted integration.
    pub fn is_ancestor(&self, ancestor: &Commit, descendant: &Commit) -> Result<bool, GitError> {
        let a = self.resolve(ancestor.as_str())?;
        let d = self.resolve(descendant.as_str())?;
        Git::new(&self.root)
            .args(["merge-base", "--is-ancestor", a.as_str(), d.as_str()])
            .test()
    }

    /// Creates branch `by/<name>` at `base` checked out in a new worktree at
    /// `dir`, and records `base` for later diffs and candidates.
    ///
    /// Fails with [`GitError::BranchExists`] rather than reusing a branch.
    /// A relative `dir` is taken relative to the current process directory.
    pub fn create_branch(
        &self,
        name: &BranchName,
        base: &Commit,
        dir: &Path,
    ) -> Result<Workspace, GitError> {
        let base = self.resolve(base.as_str())?;
        let dir = canonical_new_dir(dir)?;
        if Git::new(&self.root)
            .args(["show-ref", "--verify", "--quiet", &name.ref_name()])
            .test()?
        {
            return Err(GitError::BranchExists(name.clone()));
        }
        // The base is recorded before the branch exists so no crash can leave
        // a by/* branch without one; a stale record is overwritten on reuse.
        let key = base_key(name);
        Git::new(&self.root)
            .args(["config", &key, base.as_str()])
            .run()?;
        let added = Git::new(&self.root)
            .no_hooks()
            .args(["worktree", "add", "--quiet", "-b", &name.branch()])
            .arg(&dir)
            .arg(base.as_str())
            .run();
        if let Err(e) = added {
            let _ = Git::new(&self.root).args(["config", "--unset", &key]).run();
            return Err(e);
        }
        Ok(Workspace {
            name: name.clone(),
            path: dir,
            base,
            root: self.root.clone(),
            exclude: Vec::new(),
        })
    }

    /// Creates branch `by/<name>` at `base` in an existing detached worktree
    /// at `from` (a warm pool's ready slot), moved to `dir` first, and
    /// records `base` as [`Repository::create_branch`] does. `from` must be
    /// a worktree of this repository with no changes to tracked files; when
    /// its `HEAD` is not `base`, the checkout moves it there (a fast
    /// forward of what the slot holds), keeping untracked files that do not
    /// collide. An existing branch is refused before `from` is touched;
    /// a later failure removes the worktree wherever it is, and leaves
    /// neither the branch nor its base record behind.
    pub fn adopt_worktree(
        &self,
        name: &BranchName,
        base: &Commit,
        from: &Path,
        dir: &Path,
    ) -> Result<Workspace, GitError> {
        let base = self.resolve(base.as_str())?;
        let dir = canonical_new_dir(dir)?;
        if Git::new(&self.root)
            .args(["show-ref", "--verify", "--quiet", &name.ref_name()])
            .test()?
        {
            return Err(GitError::BranchExists(name.clone()));
        }
        let key = base_key(name);
        Git::new(&self.root)
            .args(["config", &key, base.as_str()])
            .run()?;
        let undo = |at: &Path| {
            let _ = Git::new(&self.root)
                .args(["worktree", "remove", "--force"])
                .arg(at)
                .run();
            branchyard_support::cleanup_dir(at);
            let _ = Git::new(&self.root).args(["config", "--unset", &key]).run();
        };
        let moved = Git::new(&self.root)
            .no_hooks()
            .args(["worktree", "move"])
            .arg(from)
            .arg(&dir)
            .run();
        if let Err(e) = moved {
            undo(from);
            return Err(e);
        }
        let switched = Git::new(&dir)
            .no_hooks()
            .args(["switch", "--quiet", "--no-track", "-c", &name.branch()])
            .arg(base.as_str())
            .run();
        if let Err(e) = switched {
            undo(&dir);
            let _ = Git::new(&self.root)
                .args(["branch", "-D", &name.branch()])
                .run();
            return Err(e);
        }
        Ok(Workspace {
            name: name.clone(),
            path: dir,
            base,
            root: self.root.clone(),
            exclude: Vec::new(),
        })
    }

    /// Pushes exactly `commit` to `remote` (a remote name or URL) as
    /// `refs/heads/<remote_branch>`, without a shell, without the
    /// repository's hooks and without a terminal prompt, so credentials come
    /// from the user's configured helpers or SSH agent. `force` replaces a
    /// remote branch that is not an ancestor of `commit`; without it such a
    /// push is refused by the remote.
    pub fn push(
        &self,
        remote: &str,
        commit: &Commit,
        remote_branch: &str,
        force: bool,
    ) -> Result<(), GitError> {
        if remote.is_empty() || remote.starts_with('-') {
            return Err(GitError::InvalidRef(remote.to_owned()));
        }
        let target = format!("refs/heads/{remote_branch}");
        if remote_branch.is_empty()
            || remote_branch.starts_with('-')
            || !Git::new(&self.root)
                .args(["check-ref-format", &target])
                .test()?
        {
            return Err(GitError::InvalidRef(remote_branch.to_owned()));
        }
        let commit = self.resolve(commit.as_str())?;
        let plus = if force { "+" } else { "" };
        Git::new(&self.root)
            .no_hooks()
            .args(["push", "--quiet", "--porcelain", "--", remote])
            .arg(format!("{plus}{commit}:{target}"))
            .run()?;
        Ok(())
    }

    /// The worktree that has `by/<name>` checked out, if any.
    pub fn workspace(&self, name: &BranchName) -> Result<Option<Workspace>, GitError> {
        Ok(self.workspaces()?.into_iter().find(|w| &w.name == name))
    }

    /// Every worktree with a `by/*` branch checked out. Worktrees whose
    /// directory has disappeared are included so they can be removed. Fails
    /// with [`GitError::MissingBase`] if a `by/*` worktree was not created by
    /// [`Repository::create_branch`].
    pub fn workspaces(&self) -> Result<Vec<Workspace>, GitError> {
        let mut found = Vec::new();
        for entry in git::worktrees(&self.root)? {
            let Some(name) = entry.branch.as_deref().and_then(BranchName::from_branch) else {
                continue;
            };
            let base = Git::new(&self.root)
                .args(["config", "--get", &base_key(&name)])
                .output()?
                .0;
            let base = String::from_utf8_lossy(&base.stdout).trim().to_owned();
            if !is_object_id(&base) {
                return Err(GitError::MissingBase(name));
            }
            found.push(Workspace {
                name,
                path: entry.path,
                base: Commit(base),
                root: self.root.clone(),
                exclude: Vec::new(),
            });
        }
        Ok(found)
    }
}

/// `dir` made absolute with its parent canonicalized, so the returned path is
/// the one git records and later lists. `dir` itself must not exist yet.
fn canonical_new_dir(dir: &Path) -> Result<PathBuf, GitError> {
    let dir = std::path::absolute(dir)?;
    match (dir.parent(), dir.file_name()) {
        (Some(parent), Some(name)) => {
            fs::create_dir_all(parent)?;
            Ok(fs::canonicalize(parent)?.join(name))
        }
        _ => Ok(dir),
    }
}

fn base_key(name: &BranchName) -> String {
    format!("branch.{}.{BASE_KEY}", name.branch())
}

pub(crate) fn resolve_in(dir: &Path, rev: &str) -> Result<Commit, GitError> {
    if rev.is_empty() || rev.starts_with('-') {
        return Err(GitError::InvalidRevision(rev.to_owned()));
    }
    let (out, _) = Git::new(dir)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(format!("{rev}^{{commit}}"))
        .output()?;
    let id = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() && is_object_id(&id) {
        Ok(Commit(id))
    } else {
        Err(GitError::InvalidRevision(rev.to_owned()))
    }
}

pub(crate) fn current_branch_in(dir: &Path) -> Result<Option<String>, GitError> {
    let (out, args) = Git::new(dir)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output()?;
    match out.status.code() {
        Some(0) => {
            let full = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            Ok(Some(
                full.strip_prefix("refs/heads/")
                    .map(str::to_owned)
                    .unwrap_or(full),
            ))
        }
        Some(1) => Ok(None),
        _ => Err(git::failed(args, &out)),
    }
}

fn tree_of(dir: &Path, commit: &Commit) -> Result<String, GitError> {
    Ok(Git::new(dir)
        .args(["rev-parse", "--verify"])
        .arg(format!("{commit}^{{tree}}"))
        .run()?
        .trim()
        .to_owned())
}

pub(crate) fn numstat(dir: &Path, from: &str, to: &str) -> Result<DiffStat, GitError> {
    let out = Git::new(dir)
        .args([
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            from,
            to,
            "--",
        ])
        .run()?;
    let mut stat = DiffStat::default();
    for record in out.split('\0').filter(|r| !r.is_empty()) {
        let mut fields = record.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(_path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Err(GitError::Parse(format!("numstat record {record:?}")));
        };
        stat.files_changed += 1;
        // Binary files report "-".
        stat.insertions += added.parse::<usize>().unwrap_or(0);
        stat.deletions += deleted.parse::<usize>().unwrap_or(0);
    }
    Ok(stat)
}

/// A worktree with branch `by/<name>` checked out, created from `base`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub name: BranchName,
    pub path: PathBuf,
    pub base: Commit,
    root: PathBuf,
    /// Paths, relative to the worktree, that [`Workspace::snapshot`] and
    /// [`Workspace::diff`] leave out even when git would add them; see
    /// [`Workspace::excluding`].
    exclude: Vec<String>,
}

impl Workspace {
    /// This workspace with `paths` (relative to the worktree, taken
    /// literally, not as patterns) left out of every snapshot and diff,
    /// even when no ignore rule covers them: files placed in the worktree
    /// for the agent's use, such as a copied `.env`, never reach a
    /// candidate.
    pub fn excluding(mut self, paths: impl IntoIterator<Item = String>) -> Workspace {
        self.exclude.extend(paths);
        self
    }

    /// `git add --all` over the whole worktree minus [`Workspace::excluding`].
    /// A path an ignore rule already covers is not named: git refuses a
    /// pathspec, even an exclusion, that names an ignored file.
    fn add_all(&self) -> Result<Git, GitError> {
        let mut exclude = self.exclude.clone();
        if !exclude.is_empty() {
            // One path per line, as given: a path with a newline is
            // simply never recognised here, and stays named.
            let (out, _) = Git::new(&self.path)
                .args(["check-ignore", "--"])
                .args(&exclude)
                .output()?;
            let ignored: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_owned)
                .collect();
            exclude.retain(|p| !ignored.contains(p));
        }
        Ok(Git::new(&self.path)
            .args(["add", "--all", "--", "."])
            .args(exclude.iter().map(|p| format!(":(exclude,literal){p}"))))
    }

    /// Stages every change in the worktree, respecting `.gitignore`, and
    /// commits it on `by/<name>` if anything is staged.
    ///
    /// Returns the branch head as a candidate, or `None` when its tree equals
    /// the base tree. Commits the harness made itself are included. Fails if
    /// HEAD left `by/<name>` or no longer descends from `base`. Hooks are not
    /// run.
    pub fn snapshot(&self, message: &str) -> Result<Option<Candidate>, GitError> {
        let branch = current_branch_in(&self.path)?;
        if branch.as_deref() != Some(self.name.branch().as_str()) {
            return Err(GitError::NotOnBranch {
                expected: self.name.branch(),
                actual: branch,
            });
        }
        self.add_all()?.run()?;
        let unchanged = Git::new(&self.path)
            .args(["diff", "--cached", "--quiet", "--no-ext-diff"])
            .test()?;
        if !unchanged {
            let identity = identity_args(&self.path)?;
            Git::new(&self.path)
                .no_hooks()
                .args(identity)
                .args(["commit", "--quiet", "--no-verify", "-m", message])
                .run()?;
        }
        let head = resolve_in(&self.path, "HEAD")?;
        if !Git::new(&self.path)
            .args([
                "merge-base",
                "--is-ancestor",
                self.base.as_str(),
                head.as_str(),
            ])
            .test()?
        {
            return Err(GitError::NotDescendant {
                base: self.base.0.clone(),
                head: head.0,
            });
        }
        if tree_of(&self.path, &head)? == tree_of(&self.path, &self.base)? {
            return Ok(None);
        }
        let stat = numstat(&self.path, self.base.as_str(), head.as_str())?;
        Ok(Some(Candidate {
            branch: self.name.clone(),
            base: self.base.clone(),
            head,
            stat,
        }))
    }

    /// Unified diff from `base` to the current working tree, including
    /// untracked files not ignored by `.gitignore`. Does not touch the
    /// worktree's index.
    pub fn diff(&self) -> Result<String, GitError> {
        let tree = self.working_tree()?;
        Git::new(&self.path)
            .args([
                "diff",
                "--no-color",
                "--no-ext-diff",
                self.base.as_str(),
                tree.as_str(),
                "--",
            ])
            .run()
    }

    /// Line counts for [`Workspace::diff`].
    pub fn diffstat(&self) -> Result<DiffStat, GitError> {
        let tree = self.working_tree()?;
        numstat(&self.path, self.base.as_str(), &tree)
    }

    /// Removes the worktree, discarding uncommitted changes, and optionally
    /// deletes `by/<name>` with its recorded base. Commits stay reachable
    /// only through other refs once the branch is deleted.
    pub fn remove(self, delete_branch: bool) -> Result<(), GitError> {
        if self.path.exists() {
            Git::new(&self.root)
                .args(["worktree", "remove", "--force"])
                .arg(&self.path)
                .run()?;
        } else {
            Git::new(&self.root).args(["worktree", "prune"]).run()?;
        }
        if delete_branch {
            Git::new(&self.root)
                .args(["branch", "--quiet", "-D", &self.name.branch()])
                .run()?;
        }
        Ok(())
    }

    /// Writes the working tree (tracked, untracked, not ignored) as a tree
    /// object through a private copy of the index.
    fn working_tree(&self) -> Result<String, GitError> {
        let index = Git::new(&self.path)
            .args(["rev-parse", "--path-format=absolute", "--git-path", "index"])
            .run()?;
        let index = PathBuf::from(index.trim_end_matches('\n'));
        let scratch = index.with_file_name(format!("branchyard-index-{}", unique_suffix()));
        let _cleanup = RemoveOnDrop(&scratch);
        match fs::copy(&index, &scratch) {
            // The copy keeps the index's modification time. Git compares
            // file times in whole seconds and trusts a stat-identical entry
            // only when the index is newer than the file ("racy git"); a
            // copy stamped now would make a same-size edit written in the
            // checkout's second look clean, and the diff would miss it.
            Ok(_) => fs::File::options()
                .write(true)
                .open(&scratch)?
                .set_modified(fs::metadata(&index)?.modified()?)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.add_all()?.env("GIT_INDEX_FILE", &scratch).run()?;
        Ok(Git::new(&self.path)
            .env("GIT_INDEX_FILE", &scratch)
            .arg("write-tree")
            .run()?
            .trim()
            .to_owned())
    }
}

struct RemoveOnDrop<'a>(&'a Path);

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        branchyard_support::cleanup_file(self.0);
        let mut lock = self.0.as_os_str().to_owned();
        lock.push(".lock");
        branchyard_support::cleanup_file(PathBuf::from(lock));
    }
}
