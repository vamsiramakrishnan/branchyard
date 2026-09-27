//! Opening a yard, merging a candidate, and removing a branch.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use branchyard_workspace::{Candidate, Check, Commit, DiffStat, IntegrationError, Repository};
use serde_json::json;

use crate::record::Recorder;
use crate::state::{now_ms, Begun, Lease, Record, Store, Taken};
use crate::{
    git, names, recover, Activity, BranchStatus, Error, Merged, RecordedEvent, RemoveOptions, Yard,
};

/// How long a branch's check may run during a merge.
pub(crate) const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

const EXCLUDE: &str = ".branchyard/";

pub(crate) fn open(path: &Path) -> Result<Yard, Error> {
    open_with(path, Store::open)
}

/// [`open`] with the store `store` opens at the repository root.
pub(crate) fn open_with(
    path: &Path,
    store: impl FnOnce(&Path) -> Result<Store, Error>,
) -> Result<Yard, Error> {
    let repo = Repository::open(path).map_err(git::error)?;
    let root = repo.root().to_path_buf();
    exclude(&root)?;
    let store = store(&root)?;
    let yard = Yard {
        root,
        repo,
        store,
        hub: Default::default(),
    };
    recover::all(&yard)?;
    Ok(yard)
}

/// Take `name`'s lease for a step outside any turn, such as a merge or a
/// removal, keeping its record. Refused while a turn runs on it.
fn hold(yard: &Yard, name: &str) -> Result<(Record, Lease), Error> {
    recover::stale(yard, name)?;
    let store = yard.store();
    let record = store.read(name)?;
    match store.acquire(&record)? {
        Taken::Granted(lease) => Ok((record, lease)),
        Taken::Stale => Err(Error::Running(name.to_owned())),
    }
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
    let (mut record, lease) = hold(yard, name)?;
    let fence = lease.fence().clone();
    let Some(candidate) = record.info.candidate.clone() else {
        return Err(Error::NoCandidate(name.to_owned()));
    };
    if matches!(&record.info.status, BranchStatus::Merged { target: t, .. } if t == target) {
        return Err(Error::AlreadyMerged {
            target: target.to_owned(),
        });
    }
    let branch = names::validate(name)?;
    let expected = git::local_branch(&yard.root, target)?
        .ok_or_else(|| Error::Git(format!("no local branch named {target}")))?;
    // Journaled per candidate and target, outside any turn. A merge whose
    // engine stopped after moving the target is recognised on the next
    // attempt instead of being refused or repeated.
    let step = format!("merge {target} {}", candidate.commit);
    let intent = json!({ "target": target, "candidate": candidate.commit, "expected": expected });
    let recorded = match store.backend().begin_step(&fence, 0, &step, &intent)? {
        Begun::Done(outcome) => serde_json::from_value::<Merged>(outcome).ok(),
        Begun::Pending(intent) => {
            let landed = landed(yard, name, target, &candidate.commit, &intent)?;
            if let Some(merged) = &landed {
                let outcome = serde_json::to_value(merged).unwrap_or_default();
                store.backend().finish_step(&fence, 0, &step, &outcome)?;
            }
            landed
        }
        Begun::Fresh => None,
    };
    let merged = match recorded {
        Some(merged) => merged,
        None => {
            let merged = integrate(yard, &record, &fence, target, &expected, branch);
            match merged {
                Ok(merged) => {
                    store.backend().finish_step(
                        &fence,
                        0,
                        &step,
                        &serde_json::to_value(&merged).unwrap_or_default(),
                    )?;
                    merged
                }
                Err(error) => {
                    // Refused before the target moved: nothing happened.
                    store.backend().abandon_step(&fence, 0, &step)?;
                    return Err(error);
                }
            }
        }
    };
    record.info.status = BranchStatus::Merged {
        target: target.to_owned(),
        commit: merged.commit.clone(),
    };
    let event = RecordedEvent {
        at_ms: now_ms(),
        activity: Activity::Status(record.info.status.clone()),
    };
    lease.finish(Some(&record), Some(&event))?;
    Ok(merged)
}

/// For a merge whose engine stopped after recording its intent: the
/// merge, if the candidate is already in the target.
fn landed(
    yard: &Yard,
    name: &str,
    target: &str,
    candidate: &str,
    intent: &serde_json::Value,
) -> Result<Option<Merged>, Error> {
    let Some(head) = git::local_branch(&yard.root, target)? else {
        return Ok(None);
    };
    let contained = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", candidate, &head])
        .current_dir(&yard.root)
        .status()
        .map_err(|e| Error::Git(format!("could not run git: {e}")))?
        .success();
    Ok(contained.then(|| Merged {
        branch: name.to_owned(),
        target: target.to_owned(),
        previous: intent
            .get("expected")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
        commit: head,
    }))
}

/// Integrate the record's candidate into `target` at `expected`.
fn integrate(
    yard: &Yard,
    record: &Record,
    fence: &crate::state::Fence,
    target: &str,
    expected: &str,
    branch: branchyard_workspace::BranchName,
) -> Result<Merged, Error> {
    let store = yard.store();
    let name = record.info.name.clone();
    let candidate = record
        .info
        .candidate
        .clone()
        .ok_or_else(|| Error::NoCandidate(name.clone()))?;
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
        yard.repo.integrate(
            &proposal,
            target,
            &Commit(expected.to_owned()),
            check.as_ref(),
        )
    };
    let integrated = integrated.map_err(|e| integration_error(e, target))?;
    let mut recorder = Recorder::fenced(&store, fence, None);
    for worktree in &integrated.stale_checkouts {
        recorder.record(Activity::Warning(format!(
            "{} still has {target}'s previous files; run `git read-tree -m -u {} {}` there",
            worktree.display(),
            integrated.previous,
            integrated.merged
        )))?;
    }
    Ok(Merged {
        branch: name,
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

pub(crate) fn remove(yard: &Yard, name: &str, options: &RemoveOptions) -> Result<(), Error> {
    let store = yard.store();
    let (record, lease) = hold(yard, name)?;
    // Journaled so a removal cut short says so; repeating it finishes it.
    store
        .backend()
        .begin_step(lease.fence(), 0, "remove", &json!({}))?;
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
    crate::storage::gc_after_removal(&store)?;
    if !options.keep_credentials {
        remove_credentials(&store, &record)?;
    }
    remove_home(&store, &record)?;
    Ok(())
}

/// Remove the credentials provisioning wrote in the branch's private home,
/// before the home itself goes or while a fork keeps it. A branch still
/// running in a shared home keeps them until its turn ends; its next turn
/// provisions them again anyway.
fn remove_credentials(store: &Store, record: &Record) -> Result<(), Error> {
    let Some(home) = &record.home else {
        return Ok(());
    };
    if !home.is_dir() {
        return Ok(());
    }
    let running = store.list()?.iter().any(|other| {
        other.home.as_ref() == Some(home) && other.info.status == BranchStatus::Running
    });
    if running {
        return Ok(());
    }
    branchyard_provision::apply::remove_credentials(home)
        .map(|_| ())
        .map_err(|e| Error::State(format!("could not remove the credentials in its home: {e}")))
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
