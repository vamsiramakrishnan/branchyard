//! The few git queries the engine makes itself, through
//! `branchyard_workspace`'s [`Git`], the one place that starts `git`;
//! everything that changes branches or worktrees goes through
//! `branchyard_workspace`'s repository API.

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
    WRITES.lock().unwrap_or_else(|e| e.into_inner())
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
