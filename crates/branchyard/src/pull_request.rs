//! A branch's way to a pull request, recorded on its event log as
//! [`Activity::PullRequest`](crate::Activity::PullRequest): the issue it
//! started from, the check run on its candidate, each push, the pull
//! request opened or updated, what was last observed of it, and which
//! pieces of review and CI feedback were delivered back into the branch.
//!
//! The SDK only records these; `by pr` and `by run --issue` talk to GitHub
//! through the `gh` command (see `docs/pull-requests.md`). Folding the
//! events gives a branch's current pull-request state, so it survives
//! restarts and is the same wherever the log is read.

use serde::{Deserialize, Serialize};

use crate::git;
use crate::record::Recorder;
use crate::{Activity, Branch, Error};

/// One step on the way from a branch to a merged pull request. Serialized
/// as an object tagged by `kind`, such as
/// `{"kind": "opened", "number": 7, "url": "...", ...}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PullRequestActivity {
    /// The branch works on this issue; its pull request closes it.
    IssueLinked(IssueLink),
    /// The branch's check ran on its candidate, in a temporary worktree.
    Checked(CheckRun),
    /// The candidate was pushed to a git remote.
    Pushed(Pushed),
    /// A pull request was created for the branch.
    Opened(PullRequestRef),
    /// The branch's existing pull request was updated.
    Updated(PullRequestRef),
    /// What the forge said about the pull request, recorded when it
    /// changed.
    Observed(PullRequestObservation),
    /// Feedback (failed CI checks, review comments) was sent into the
    /// branch, each piece named by a key so it is delivered once.
    FeedbackDelivered {
        keys: Vec<String>,
        /// `steer` into a running turn, or `send` as a new turn.
        via: String,
        /// One line per piece, for the log.
        summary: Vec<String>,
    },
    /// Feedback recorded as delivered did not reach the branch after all;
    /// its keys are delivered again later.
    FeedbackUndelivered { keys: Vec<String>, reason: String },
    /// `by pr --watch` stopped following the pull request.
    WatchStopped { reason: String },
    /// Review threads whose comments were delivered as feedback, and whose
    /// files `commit` (just pushed) changed, were answered "Addressed in
    /// <commit>" and resolved; each thread is attempted once.
    // The doc comment is the schema description; `<commit>` is not HTML.
    #[allow(rustdoc::invalid_html_tags)]
    ThreadsResolved {
        commit: String,
        threads: Vec<ResolvedThread>,
    },
    /// The branch started from this pull request's head (`by run --pr`);
    /// its pull request is that one, so `by pr` pushes to its head and
    /// updates it.
    Started(PullRequestRef),
    /// The branch was started on top of another branch's candidate
    /// (`by run --base BRANCH`): a stack. Its pull request is opened
    /// against the parent's pull request head.
    Stacked(StackLink),
    /// The parent's pull request merged: the branch was rebased onto the
    /// branch it merged into, and its pull request retargeted.
    Restacked(Restack),
    /// The same rebase conflicted; the branch is unchanged.
    RestackBlocked {
        /// The parent whose pull request merged.
        parent: String,
        /// The branch it would have been rebased onto.
        onto: String,
        /// The paths that conflicted.
        files: Vec<String>,
    },
}

/// The branch this one is stacked on.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackLink {
    /// The parent branch's name.
    pub parent: String,
    /// The parent's candidate commit this branch started from.
    pub base: String,
}

/// A stacked branch rebased after its parent's pull request merged.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restack {
    /// The parent whose pull request merged.
    pub parent: String,
    /// The branch it merged into, which this one now sits on and targets.
    pub onto: String,
    /// The commit of `onto` it was rebased onto.
    pub onto_commit: String,
    /// Its candidate before the rebase.
    pub from: String,
    /// Its candidate after the rebase.
    pub to: String,
    /// The branch it is stacked on now: the parent's own parent when the
    /// parent was itself stacked, none when it now targets `onto` directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_parent: Option<String>,
}

/// What [`Branch::restack`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restacked {
    /// Rebased: the candidate before and after.
    Done {
        /// The candidate before the rebase.
        from: String,
        /// The candidate after it.
        to: String,
    },
    /// A commit did not apply; nothing changed. The conflicting paths.
    Conflict(Vec<String>),
    /// The branch already sits on `onto`.
    UpToDate,
}

/// One review thread `by pr --watch` answered and resolved, or tried to.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedThread {
    /// The thread's GraphQL node ID.
    pub id: String,
    pub path: String,
    /// The "Addressed in" reply was posted.
    pub replied: bool,
    /// GitHub reports the thread resolved.
    pub resolved: bool,
    /// Why not, when either failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// An issue a branch works on.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueLink {
    /// The issue's number: GitHub's and GitLab's, or the number in a Linear
    /// or Jira key (`123` of `ENG-123`).
    pub number: u64,
    pub url: String,
    pub title: String,
    /// The tracker, when not GitHub: `linear`, `jira` or `gitlab`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker: Option<String>,
    /// The tracker's own reference when it is not `#number`: `ENG-123`,
    /// `PROJ-7`, `group/project#12`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// A check run on one commit; see [`Branch::verify_candidate`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    pub commit: String,
    pub argv: Vec<String>,
    pub passed: bool,
    #[serde(default)]
    pub timed_out: bool,
    /// The end of its combined output, at most 4096 bytes.
    #[serde(default)]
    pub output_tail: String,
}

/// A candidate pushed to a git remote.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pushed {
    /// The git remote's name (or URL).
    pub remote: String,
    /// The branch on the remote, without `refs/heads/`.
    pub remote_branch: String,
    pub commit: String,
    #[serde(default)]
    pub forced: bool,
}

/// A pull request as `by pr` last opened or updated it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestRef {
    pub number: u64,
    pub url: String,
    /// Its head: the branch on the remote.
    pub head: String,
    /// Its base, when `by pr` set one.
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub draft: bool,
}

/// What was observed of a pull request at one moment.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestObservation {
    pub number: u64,
    pub url: String,
    /// `open`, `merged` or `closed`.
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    /// The commit the pull request's head is at.
    #[serde(default)]
    pub head_commit: Option<String>,
    /// GitHub's review decision: `approved`, `changes_requested`,
    /// `review_required`, or none.
    #[serde(default)]
    pub review_decision: Option<String>,
    /// `mergeable`, `conflicting` or `unknown`.
    #[serde(default)]
    pub mergeable: Option<String>,
    /// GitHub's merge state: `clean`, `blocked`, `behind`, `dirty`,
    /// `unstable`, `has_hooks`, `draft` or `unknown`.
    #[serde(default)]
    pub merge_state: Option<String>,
    pub ci: CiSummary,
    /// Review threads not yet resolved.
    #[serde(default)]
    pub unresolved_threads: u32,
}

/// The pull request's CI checks, counted by outcome.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiSummary {
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
    pub skipped: u32,
    /// The names of the failed checks.
    #[serde(default)]
    pub failing: Vec<String>,
}

impl Branch {
    /// Append `activity` to this branch's event log, outside any turn.
    pub fn record_pull_request(&self, activity: PullRequestActivity) -> Result<(), Error> {
        let store = self.yard().store();
        Recorder::open(&store, &self.info().name, None)?
            .record(Activity::PullRequest(Box::new(activity)))
    }

    /// Rebase this branch onto `onto` (a revision), as a stack does after
    /// its parent's pull request merged: the commits after its base are
    /// replayed on `onto`, its base and candidate move with them, and a
    /// snapshot is recorded. Refused while a turn runs, and when its worktree
    /// has uncommitted work. A conflict aborts the rebase and changes
    /// nothing. Records no pull-request event; the caller does.
    pub fn restack(&self, onto: &str) -> Result<Restacked, Error> {
        let name = &self.info().name;
        let yard = self.yard();
        let (mut record, lease) = crate::ops::hold(yard, name)?;
        match &record.info.status {
            crate::BranchStatus::Running => return Err(Error::Running(name.clone())),
            crate::BranchStatus::Waiting | crate::BranchStatus::Blocked { .. } => {
                return Err(Error::Denied(format!("{name} has not started")))
            }
            crate::BranchStatus::Merged { target, .. } => {
                return Err(Error::Denied(format!("{name} was merged into {target}")))
            }
            _ => {}
        }
        let worktree = record.info.worktree.clone();
        if !worktree.is_dir() {
            return Err(Error::State(format!(
                "{name}'s worktree {} is missing",
                worktree.display()
            )));
        }
        let _lock = git::lock();
        let dirty = git::status(&worktree)?;
        if !dirty.trim().is_empty() {
            return Err(Error::Denied(format!(
                "{name}'s worktree has uncommitted changes, which a rebase would trip over; \
                 send a turn so they are checkpointed, or commit them:\n{}",
                dirty.trim_end()
            )));
        }
        let onto_commit = yard
            .repo
            .resolve(onto)
            .map_err(|_| Error::Git(format!("{onto:?} does not name a commit")))?;
        let from = record
            .info
            .candidate
            .as_ref()
            .map(|c| c.commit.clone())
            .ok_or_else(|| Error::NoCandidate(name.clone()))?;
        if record.info.base == onto_commit.0 {
            return Ok(Restacked::UpToDate);
        }
        let branch = crate::names::validate(name)?;
        let workspace = yard
            .repo
            .workspace(&branch)
            .map_err(git::error)?
            .ok_or_else(|| {
                Error::State(format!(
                    "no worktree has {} checked out",
                    record.info.git_branch
                ))
            })?;
        let to = match workspace.rebase_onto(&onto_commit).map_err(git::error)? {
            branchyard_workspace::Rebased::Done(head) => head.0,
            branchyard_workspace::Rebased::Conflict(files) => {
                return Ok(Restacked::Conflict(files))
            }
        };
        let candidate = match git::same_tree(&yard.root, &onto_commit.0, &to)? {
            true => None,
            false => {
                let (files_changed, insertions, deletions) =
                    git::numstat(&yard.root, &onto_commit.0, &to)?;
                Some(crate::CandidateInfo {
                    commit: to.clone(),
                    files_changed,
                    insertions,
                    deletions,
                })
            }
        };
        record.info.base = onto_commit.0.clone();
        record.info.status = match candidate {
            Some(_) => crate::BranchStatus::Ready,
            None => crate::BranchStatus::NoChanges,
        };
        record.info.candidate = candidate.clone();
        let event = candidate.map(|c| crate::RecordedEvent {
            at_ms: branchyard_support::time::now_ms(),
            activity: Activity::Snapshot(c),
        });
        lease.finish(Some(&record), event.as_ref())?;
        Ok(Restacked::Done { from, to })
    }

    /// The branch's check, as given with `--check` when it was created.
    pub fn check(&self) -> Result<Option<Vec<String>>, Error> {
        Ok(self.yard().store().read(&self.info().name)?.check)
    }

    /// Run the branch's check on its current candidate, checked out alone
    /// in a temporary worktree (not merged into anything), with the timeout
    /// `merge` uses. `None` when the branch has no check. Records nothing;
    /// see [`Branch::record_pull_request`].
    pub fn verify_candidate(&self) -> Result<Option<CheckRun>, Error> {
        let record = self.yard().store().read(&self.info().name)?;
        let candidate = record
            .info
            .candidate
            .ok_or_else(|| Error::NoCandidate(self.info().name.clone()))?;
        let Some(argv) = record.check else {
            return Ok(None);
        };
        let check = branchyard_workspace::Check {
            argv: argv.clone(),
            timeout: crate::ops::CHECK_TIMEOUT,
        };
        let verified = {
            let _lock = git::lock();
            self.yard()
                .repo
                .verify(&branchyard_workspace::Commit(candidate.commit), &check)
        };
        let verified = verified.map_err(|error| match error {
            branchyard_workspace::IntegrationError::CheckNotStarted(e) => Error::CheckNotStarted {
                reason: e.to_string(),
                checks: Vec::new(),
            },
            branchyard_workspace::IntegrationError::Git(e) => git::error(e),
            other => Error::Git(other.to_string()),
        })?;
        Ok(Some(CheckRun {
            commit: verified.commit.0,
            argv,
            passed: verified.passed,
            timed_out: verified.timed_out,
            output_tail: verified.output_tail,
        }))
    }

    /// Push the branch's current candidate commit to `remote` as
    /// `refs/heads/<remote_branch>`; see
    /// [`branchyard_workspace::Repository::push`]. Records nothing.
    pub fn push_candidate(
        &self,
        remote: &str,
        remote_branch: &str,
        force: bool,
    ) -> Result<Pushed, Error> {
        let record = self.yard().store().read(&self.info().name)?;
        let candidate = record
            .info
            .candidate
            .ok_or_else(|| Error::NoCandidate(self.info().name.clone()))?;
        let commit = branchyard_workspace::Commit(candidate.commit.clone());
        self.yard()
            .repo
            .push(remote, &commit, remote_branch, force)
            .map_err(git::error)?;
        Ok(Pushed {
            remote: remote.to_owned(),
            remote_branch: remote_branch.to_owned(),
            commit: candidate.commit,
            forced: force,
        })
    }
}

/// A branch name made from free text, as `by run` names a branch after its
/// prompt: lowercase `[a-z0-9-]`, at most 40 characters, `task` if nothing
/// is left.
pub fn slug(text: &str) -> String {
    crate::names::slug(text)
}
