//! The few git queries the engine makes itself, through
//! `branchyard_workspace`'s [`Git`], the one place that starts `git`;
//! everything that changes branches or worktrees goes through
//! `branchyard_workspace`'s repository API.

use branchyard_support::LockExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use branchyard_workspace::{Git, GitError};

use crate::Error;

/// Serializes this process's writes to the repository's shared state.
/// Worktree creation, branch deletion and `git config` take the same
/// `config` lock, and git fails rather than waits when it is held, so
/// parallel branches would otherwise fail at random.
static WRITES: Mutex<()> = Mutex::new(());

pub(crate) fn lock() -> MutexGuard<'static, ()> {
    WRITES.lock_recovering("WRITES")
}

/// Run git in `dir` and return stdout, or the exit's stderr as an error.
pub(crate) fn run(dir: &Path, args: &[&str]) -> Result<String, Error> {
    Git::new(dir).args(args).run().map_err(error)
}

/// Whether git exits successfully.
pub(crate) fn test(dir: &Path, args: &[&str]) -> Result<bool, Error> {
    Git::new(dir).args(args).succeeds().map_err(error)
}

fn output(dir: &Path, args: &[&str]) -> Result<(bool, String, String), Error> {
    let (out, _) = Git::new(dir).args(args).output().map_err(error)?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// The branch checked out at `root`, or `None` when HEAD is detached: the
/// default target of a merge.
pub fn current_branch(root: &Path) -> Result<Option<String>, Error> {
    branchyard_workspace::git::current_branch(root).map_err(error)
}

/// The repository's common git directory, shared by all its worktrees.
pub(crate) fn common_dir(root: &Path) -> Result<PathBuf, Error> {
    let dir = run(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(PathBuf::from(dir.trim_end_matches('\n')))
}

pub(crate) fn branch_exists(root: &Path, git_branch: &str) -> Result<bool, Error> {
    test(
        root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{git_branch}"),
        ],
    )
}

/// The commit `refs/heads/<branch>` points at, if the branch exists.
pub(crate) fn local_branch(root: &Path, branch: &str) -> Result<Option<String>, Error> {
    if branch.is_empty() || branch.starts_with('-') {
        return Ok(None);
    }
    let (ok, stdout, _) = output(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}^{{commit}}"),
        ],
    )?;
    Ok(ok.then(|| stdout.trim().to_owned()))
}

pub(crate) fn diff(root: &Path, base: &str, head: &str) -> Result<String, Error> {
    run(
        root,
        &["diff", "--no-color", "--no-ext-diff", base, head, "--"],
    )
}

pub(crate) fn error(error: GitError) -> Error {
    match error {
        GitError::NotAWorkTree(path) => Error::NotARepository(path),
        GitError::BranchExists(name) => Error::BranchExists(name.to_string()),
        other => Error::Git(other.to_string()),
    }
}

/// The full commit ID `rev` names in `dir`, if it names one.
pub(crate) fn commit(dir: &Path, rev: &str) -> Result<Option<String>, Error> {
    if rev.is_empty() || rev.starts_with('-') {
        return Ok(None);
    }
    let (ok, stdout, _) = output(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
    )?;
    Ok(ok.then(|| stdout.trim().to_owned()))
}

/// Point `name` at `commit`, creating it if needed.
pub(crate) fn update_ref(root: &Path, name: &str, commit: &str) -> Result<(), Error> {
    run(root, &["update-ref", "--no-deref", name, commit]).map(|_| ())
}

/// Delete `name` if it exists.
pub(crate) fn delete_ref(root: &Path, name: &str) -> Result<(), Error> {
    run(root, &["update-ref", "-d", "--no-deref", name]).map(|_| ())
}

/// Every ref under `prefix` (which ends with `/`), with the commit it names.
pub(crate) fn refs(root: &Path, prefix: &str) -> Result<Vec<(String, String)>, Error> {
    let out = run(
        root,
        &["for-each-ref", "--format=%(refname) %(objectname)", prefix],
    )?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let (name, id) = line.split_once(' ')?;
            Some((name.to_owned(), id.to_owned()))
        })
        .collect())
}

/// Files changed, lines added and lines removed between two commits.
pub(crate) fn numstat(root: &Path, from: &str, to: &str) -> Result<(u32, u32, u32), Error> {
    let out = run(
        root,
        &[
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            from,
            to,
            "--",
        ],
    )?;
    let (mut files, mut added, mut removed) = (0, 0, 0);
    for record in out.split('\0').filter(|r| !r.is_empty()) {
        let mut fields = record.splitn(3, '\t');
        let (Some(a), Some(d)) = (fields.next(), fields.next()) else {
            continue;
        };
        files += 1;
        added += a.parse::<u32>().unwrap_or(0);
        removed += d.parse::<u32>().unwrap_or(0);
    }
    Ok((files, added, removed))
}

/// The paths that differ between two commits, sorted.
pub(crate) fn changed_paths(root: &Path, from: &str, to: &str) -> Result<Vec<String>, Error> {
    let out = run(
        root,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            from,
            to,
            "--",
        ],
    )?;
    let mut paths: Vec<String> = out
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    paths.sort();
    Ok(paths)
}

/// Whether two commits have the same tree.
pub(crate) fn same_tree(root: &Path, a: &str, b: &str) -> Result<bool, Error> {
    let tree = |rev: &str| run(root, &["rev-parse", "--verify", &format!("{rev}^{{tree}}")]);
    Ok(tree(a)?.trim() == tree(b)?.trim())
}

/// `git status --porcelain` in `dir`, listing untracked files one by one
/// and leaving ignored ones out: empty when nothing is uncommitted.
pub(crate) fn status(dir: &Path) -> Result<String, Error> {
    run(
        dir,
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )
}
