//! Tasks with a repository of their own (folder tasks and tasks with no
//! files, under `$BRANCHYARD_HOME/tasks/`) as sync sources, the mapping
//! "Task repositories" in docs/sync.md describes; and a provider that puts
//! them beside a repository's branches.
//!
//! A task's ID is its ULID, which never looks like a branch's
//! `<repository key>.<branch>` ID, so the two kinds never collide.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use branchyard::tasks::TaskRepo;

use crate::error::{Error, Result};
use crate::replicator::SourceProvider;
use crate::source::{ChunkId, SyncSource};

/// One task's own repository.
pub struct TaskSource {
    repo: TaskRepo,
}

impl TaskSource {
    pub fn new(repo: TaskRepo) -> TaskSource {
        TaskSource { repo }
    }
}

fn local(e: branchyard::Error) -> Error {
    Error::local(e.to_string())
}

impl SyncSource for TaskSource {
    fn task_id(&self) -> &str {
        &self.repo.id
    }

    fn git_dir(&self) -> &Path {
        &self.repo.git_dir
    }

    fn refs(&self) -> Result<BTreeMap<String, String>> {
        Ok(self.repo.refs().map_err(local)?.into_iter().collect())
    }

    /// The repository is the task's alone, so its refs keep their names.
    fn local_ref(&self, task_ref: &str) -> Option<String> {
        Some(task_ref.to_owned())
    }

    fn chunk_dir(&self) -> Option<&Path> {
        Some(self.repo.chunks.dir())
    }

    fn reachable_chunks(&self, commit: &str) -> Result<Vec<ChunkId>> {
        self.repo
            .reachable_chunks(commit)
            .map_err(local)?
            .iter()
            .map(|hex| ChunkId::parse(hex))
            .collect()
    }
}

/// Every task with a repository of its own under a Branchyard home.
#[derive(Clone)]
pub struct HomeTasks {
    home: PathBuf,
}

impl HomeTasks {
    /// The tasks under `home` (normally [`branchyard::tasks::home`]).
    pub fn new(home: impl Into<PathBuf>) -> HomeTasks {
        HomeTasks { home: home.into() }
    }

    /// The ID of the task `key` names here (an ID or a unique prefix of
    /// one), or `None` when no task here has a repository of its own by it.
    pub fn resolve(&self, key: &str) -> Option<String> {
        TaskRepo::open(&self.home, key).ok().map(|repo| repo.id)
    }
}

impl SourceProvider for HomeTasks {
    fn sources(&self) -> Result<Vec<Box<dyn SyncSource>>> {
        if !self.home.join("tasks").is_dir() {
            return Ok(Vec::new());
        }
        Ok(TaskRepo::list(&self.home)
            .map_err(local)?
            .into_iter()
            .map(|repo| Box::new(TaskSource::new(repo)) as Box<dyn SyncSource>)
            .collect())
    }

    /// Only a task that exists here: pulling a task's own repository onto a
    /// machine that never had it is not built yet (docs/sync.md).
    fn source(&self, task: &str) -> Result<Option<Box<dyn SyncSource>>> {
        Ok(TaskRepo::open(&self.home, task)
            .ok()
            .map(|repo| Box::new(TaskSource::new(repo)) as Box<dyn SyncSource>))
    }
}

/// Several providers as one: every source of each, and a task from the
/// first that has it.
pub struct Providers(pub Vec<Arc<dyn SourceProvider>>);

impl SourceProvider for Providers {
    fn sources(&self) -> Result<Vec<Box<dyn SyncSource>>> {
        let mut all = Vec::new();
        for provider in &self.0 {
            all.extend(provider.sources()?);
        }
        Ok(all)
    }

    fn source(&self, task: &str) -> Result<Option<Box<dyn SyncSource>>> {
        for provider in &self.0 {
            if let Some(source) = provider.source(task)? {
                return Ok(Some(source));
            }
        }
        Ok(None)
    }
}
