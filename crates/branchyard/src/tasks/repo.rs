//! What sync needs of a task's repository: where its git directory is,
//! which refs are the task's, and which chunks its history names. See
//! "The API for sync" in `docs/task-repos.md`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::large::ChunkStore;
use super::{TaskFiles, TaskView};
use crate::{Error, Yard};

/// A task's repository, for replication.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskRepo {
    pub id: String,
    pub files: TaskFiles,
    /// The git directory: `$BRANCHYARD_HOME/tasks/<id>/git` for a task with
    /// a repository of its own, else the repository's common git directory
    /// (shared with everything else in it).
    pub git_dir: PathBuf,
    /// Whether the git directory is the task's alone. When it is not, only
    /// [`TaskRepo::refs`] are the task's.
    pub own: bool,
    /// Where its large files' chunks are.
    pub chunks: ChunkStore,
    /// The task's attempts (branch names), oldest first.
    pub attempts: Vec<String>,
}

impl TaskRepo {
    /// The repository of the task `key` names in `yard` (an ID, a unique
    /// prefix of one, or an attempt).
    pub fn of(yard: &Yard, key: &str) -> Result<TaskRepo, Error> {
        let view = super::view(yard, key)?;
        Ok(Self::from_view(yard, view))
    }

    fn from_view(yard: &Yard, view: TaskView) -> TaskRepo {
        let owned = super::folder::owned(yard.root());
        TaskRepo {
            id: view.task.id,
            files: view.task.files,
            git_dir: view.repository,
            own: owned.is_some(),
            chunks: owned
                .map(|o| o.large.store)
                .unwrap_or_else(ChunkStore::at_home),
            attempts: view.attempts.into_iter().map(|a| a.name).collect(),
        }
    }

    /// The repository of the task with a repository of its own that `key`
    /// names, under `<home>/tasks/` (`home` is normally [`super::home`]).
    pub fn open(home: &Path, key: &str) -> Result<TaskRepo, Error> {
        let yard = super::folder::open_home(home, key)?
            .ok_or_else(|| Error::State(format!("no task {key} has a repository of its own")))?;
        let id = super::folder::owned(yard.root())
            .map(|o| o.id)
            .ok_or_else(|| {
                Error::State(format!(
                    "{} is not a task's repository",
                    yard.root().display()
                ))
            })?;
        Self::of(&yard, &id)
    }

    /// Every task with a repository of its own under `<home>/tasks/`.
    pub fn list(home: &Path) -> Result<Vec<TaskRepo>, Error> {
        let mut repos = Vec::new();
        for view in super::folder::home_tasks(home)? {
            repos.push(Self::open(home, &view.task.id)?);
        }
        Ok(repos)
    }

    /// The task's refs with the commit each names, sorted: every ref of a
    /// repository of its own; in a shared repository, each attempt's
    /// branch and checkpoints.
    pub fn refs(&self) -> Result<Vec<(String, String)>, Error> {
        let mut refs = Vec::new();
        if self.own {
            refs = crate::git::refs(&self.git_dir, "refs/")?;
        } else {
            for name in &self.attempts {
                refs.extend(crate::git::refs(
                    &self.git_dir,
                    &format!("refs/heads/{}{name}", branchyard_workspace::BRANCH_PREFIX),
                )?);
                refs.extend(crate::git::refs(
                    &self.git_dir,
                    &format!("refs/branchyard/{name}/"),
                )?);
            }
            // Exact names only: `by/a` must not bring in `by/ab`.
            refs.retain(|(name, _)| {
                self.attempts.iter().any(|a| {
                    name == &format!("refs/heads/{}{a}", branchyard_workspace::BRANCH_PREFIX)
                        || name.starts_with(&format!("refs/branchyard/{a}/"))
                })
            });
        }
        refs.sort();
        refs.dedup();
        Ok(refs)
    }

    /// The chunk hashes named by pointers anywhere in the history of
    /// `rev` (a ref or commit of this repository).
    pub fn reachable_chunks(&self, rev: &str) -> Result<BTreeSet<String>, Error> {
        reachable_chunks(&self.git_dir, rev)
    }
}

/// The chunk hashes named by large-file pointers in every tree reachable
/// from `rev`, in the git directory (or work tree) `git_dir`: what must be
/// kept, and uploaded, for that history to be restored.
pub fn reachable_chunks(git_dir: &Path, rev: &str) -> Result<BTreeSet<String>, Error> {
    super::large::chunks_reachable(git_dir, rev)
}
