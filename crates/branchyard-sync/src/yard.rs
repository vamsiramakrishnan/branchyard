//! A repository's Branchyard branches as sync tasks, for `by sync` and a
//! server's replicator.
//!
//! A branch's task ID is `<repository key>.<branch>`, with `/` in the
//! branch's name as `~`: the repository key is the first 12 hex digits of
//! the repository's root commit, which every clone shares, so two
//! repositories syncing to one remote never mix their tasks. A machine
//! without the repository pulls a task by its full ID.

use std::path::PathBuf;

use branchyard::Yard;

use crate::error::{Error, Result};
use crate::git::Git;
use crate::manifest::check_task_id;
use crate::replicator::SourceProvider;
use crate::source::{task_id_for_branch, BranchSource, SyncSource};

/// The branches of one repository.
#[derive(Clone)]
pub struct YardTasks {
    yard: Yard,
    git_dir: PathBuf,
    key: Option<String>,
}

/// The first 12 hex digits of a repository's root commit (the
/// smallest, when there are several), or `None` before its first commit.
pub fn repository_key(git: &Git) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git.dir())
        .args(["rev-list", "--max-parents=0", "--all"])
        .output()
        .ok()?;
    let mut roots: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_owned())
        .filter(|l| l.len() >= 12)
        .collect();
    roots.sort();
    roots.first().map(|r| r[..12].to_owned())
}

/// Split a full task ID into its repository key and branch name.
pub fn split_task_id(task: &str) -> Option<(&str, String)> {
    let (key, branch) = task.split_once('.')?;
    (key.len() == 12 && key.bytes().all(|b| b.is_ascii_hexdigit()) && !branch.is_empty())
        .then(|| (key, branch.replace('~', "/")))
}

impl YardTasks {
    pub fn new(yard: Yard) -> Result<YardTasks> {
        let git_dir = BranchSource::git_dir_of(yard.root())?;
        let key = repository_key(&Git::new(&git_dir));
        Ok(YardTasks { yard, git_dir, key })
    }

    /// The task ID of a branch here.
    pub fn task_id(&self, branch: &str) -> Result<String> {
        let id = match &self.key {
            Some(key) => format!("{key}.{}", task_id_for_branch(branch)),
            None => task_id_for_branch(branch),
        };
        check_task_id(&id)?;
        Ok(id)
    }

    /// A task ID from what a person typed: a full ID, or a branch here.
    pub fn resolve(&self, task_or_branch: &str) -> Result<String> {
        if split_task_id(task_or_branch).is_some() {
            check_task_id(task_or_branch)?;
            return Ok(task_or_branch.to_owned());
        }
        self.task_id(task_or_branch)
    }

    fn source_for(&self, task: &str, name: &str, git_branch: &str) -> Result<BranchSource> {
        BranchSource::new(&self.git_dir, name, git_branch)?.with_task_id(task)
    }
}

impl SourceProvider for YardTasks {
    fn sources(&self) -> Result<Vec<Box<dyn SyncSource>>> {
        let branches = self
            .yard
            .branches()
            .map_err(|e| Error::local(format!("reading branches: {e}")))?;
        let mut out: Vec<Box<dyn SyncSource>> = Vec::new();
        for b in branches {
            let task = self.task_id(&b.name)?;
            out.push(Box::new(self.source_for(&task, &b.name, &b.git_branch)?));
        }
        Ok(out)
    }

    fn source(&self, task: &str) -> Result<Option<Box<dyn SyncSource>>> {
        let branch = match split_task_id(task) {
            Some((key, branch)) => {
                if self.key.as_deref().is_some_and(|k| k != key) {
                    return Err(Error::config(format!(
                        "{task} belongs to another repository (this one's key is {})",
                        self.key.as_deref().unwrap_or("")
                    )));
                }
                branch
            }
            None => task.replace('~', "/"),
        };
        let git_branch = self
            .yard
            .branches()
            .map_err(|e| Error::local(format!("reading branches: {e}")))?
            .into_iter()
            .find(|b| b.name == branch)
            .map_or_else(|| branch.clone(), |b| b.git_branch);
        Ok(Some(Box::new(self.source_for(
            task,
            &branch,
            &git_branch,
        )?)))
    }
}
