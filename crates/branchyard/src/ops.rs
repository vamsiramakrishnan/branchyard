//! Opening a yard, merging a candidate, and removing a branch.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use branchyard_workspace::{Candidate, Check, Commit, DiffStat, IntegrationError, Repository};

use crate::record::Recorder;
use crate::state::{Record, Store};
use crate::{git, names, Activity, BranchStatus, Error, Merged, Yard};

/// How long a branch's check may run during a merge.
pub(crate) const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

const EXCLUDE: &str = ".branchyard/";

pub(crate) fn open(path: &Path) -> Result<Yard, Error> {
    let repo = Repository::open(path).map_err(git::error)?;
    let root = repo.root().to_path_buf();
    Store::new(&root).create_dirs()?;
    exclude(&root)?;
    Ok(Yard {
        root,
        repo,
        hub: Default::default(),
    })
}

/// Add `.branchyard/` to the repository's `info/exclude` unless it is there.
/// The common git directory holds the file for every worktree, so this works
/// from a linked worktree too.
fn exclude(root: &Path) -> Result<(), Error> {
    let _lock = git::lock();
    let info = git::common_dir(root)?.join("info");
    fs::create_dir_all(&info)?;
    let path = info.join("exclude");
    let current = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let present = current.lines().any(|line| {
        matches!(
            line.trim(),
            ".branchyard/" | "/.branchyard/" | ".branchyard" | "/.branchyard"
        )
    });
    if present {
        return Ok(());
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let separator = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    write!(
        file,
        "{separator}# Branchyard state (branches, event logs, worktrees)\n{EXCLUDE}\n"
    )?;
    Ok(())
}

pub(crate) fn merge(yard: &Yard, name: &str, target: &str) -> Result<Merged, Error> {
    let store = yard.store();
    let mut record = store.read(name)?;
    let Some(candidate) = record.info.candidate.clone() else {
        return Err(Error::NoCandidate(name.to_owned()));
    };
    let branch = names::validate(name)?;
    let expected = git::local_branch(&yard.root, target)?
        .ok_or_else(|| Error::Git(format!("no local branch named {target}")))?;
    let proposal = Candidate {
        branch,
        base: Commit(record.info.base.clone()),
        head: Commit(candidate.commit.clone()),
        stat: DiffStat {
            files_changed: candidate.files_changed as usize,
            insertions: candidate.insertions as usize,
            deletions: candidate.deletions as usize,
        },
    };
    let check = record.check.clone().map(|argv| Check {
        argv,
        timeout: CHECK_TIMEOUT,
    });
    let integrated = {
        let _lock = git::lock();
        yard.repo
            .integrate(&proposal, target, &Commit(expected.clone()), check.as_ref())
    };
    let integrated = integrated.map_err(|e| integration_error(e, target))?;
    let mut recorder = Recorder::open(&store, name, None)?;
    for worktree in &integrated.stale_checkouts {
        recorder.record(Activity::Warning(format!(
            "{} still has {target}'s previous files; run `git read-tree -m -u {} {}` there",
            worktree.display(),
            integrated.previous,
            integrated.merged
        )))?;
    }
    record.info.status = BranchStatus::Merged {
        target: target.to_owned(),
        commit: integrated.merged.0.clone(),
    };
    store.write(&record)?;
    recorder.record(Activity::Status(record.info.status.clone()))?;
    Ok(Merged {
        branch: name.to_owned(),
        target: target.to_owned(),
        previous: integrated.previous.0,
        commit: integrated.merged.0,
    })
}

fn integration_error(error: IntegrationError, target: &str) -> Error {
    match error {
        IntegrationError::TargetMoved { expected, actual } => Error::TargetMoved {
            expected: expected.0,
            actual: actual.map(|c| c.0),
        },
        IntegrationError::Conflict { files } => Error::Conflict { files },
        IntegrationError::CheckFailed { output_tail, .. } => Error::CheckFailed { output_tail },
        IntegrationError::CheckTimedOut {
            timeout,
            output_tail,
        } => Error::CheckTimedOut {
            timeout,
            output_tail,
        },
        IntegrationError::CheckNotStarted(e) => Error::CheckNotStarted(e.to_string()),
        IntegrationError::DirtyTarget { worktree } => Error::DirtyTarget(worktree),
        IntegrationError::AlreadyIntegrated => Error::AlreadyMerged {
            target: target.to_owned(),
        },
        IntegrationError::InvalidCandidate(message) => Error::InvalidCandidate(message),
        IntegrationError::Git(e) => git::error(e),
        other => Error::Git(other.to_string()),
    }
}

pub(crate) fn remove(yard: &Yard, name: &str) -> Result<(), Error> {
    let store = yard.store();
    let record = store.read(name)?;
    let merged = matches!(record.info.status, BranchStatus::Merged { .. });
    let branch = names::validate(name)?;
    {
        let _lock = git::lock();
        match yard.repo.workspace(&branch).map_err(git::error)? {
            Some(workspace) => workspace.remove(!merged).map_err(git::error)?,
            None => {
                let git_branch = branch.branch();
                if !merged && git::branch_exists(&yard.root, &git_branch)? {
                    git::run(&yard.root, &["branch", "--quiet", "-D", &git_branch])?;
                }
            }
        }
    }
    store.delete(name)?;
    remove_home(&store, &record)?;
    Ok(())
}

/// Delete an isolated home no remaining branch uses. Forks share their
/// parent's so they can find its sessions.
fn remove_home(store: &Store, record: &Record) -> Result<(), Error> {
    let Some(home) = &record.home else {
        return Ok(());
    };
    let shared = store
        .list()?
        .iter()
        .any(|other| other.home.as_ref() == Some(home));
    if !shared && home.starts_with(store.dir()) {
        match fs::remove_dir_all(home) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
