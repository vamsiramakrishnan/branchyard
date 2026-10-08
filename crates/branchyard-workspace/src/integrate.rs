//! Validated promotion of a candidate into a target branch.

use branchyard_support::best_effort;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use crate::check::{self, Check, CheckOutcome};
use crate::git::{self, identity_args, is_object_id, Git, GitError};
use crate::repo::{resolve_in, unique_suffix};
use crate::{Candidate, Commit, Repository};

/// A completed promotion: `target` moved from `previous` to `merged`, a merge
/// commit whose first parent is `previous` and second parent the candidate
/// head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrated {
    pub target: String,
    pub previous: Commit,
    pub merged: Commit,
    /// The last [`OUTPUT_TAIL_BYTES`](crate::OUTPUT_TAIL_BYTES) bytes of the check's output, if
    /// a check ran.
    pub check_output_tail: Option<String>,
    /// Worktrees with `target` checked out whose files could not be moved to
    /// `merged` after promotion (for example, a file appeared in the way).
    /// Their index and files still match `previous`; running
    /// `git read-tree -m -u <previous> <merged>` there completes the update.
    /// Empty in the normal case.
    pub stale_checkouts: Vec<PathBuf>,
}

/// One candidate of a [`Repository::integrate_many`], in the order merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedCandidate {
    /// Its git branch, `by/<name>`.
    pub branch: String,
    /// The candidate's head commit.
    pub head: Commit,
    /// What it was merged onto: the target as it was for the first
    /// candidate, then the merge before it. Merges stack, so each one's
    /// range is `onto..merge`, not `previous..merge`.
    pub onto: Commit,
    /// The merge commit made for it, or `None` when the target, or a
    /// candidate merged before it in the same call, already contained it.
    pub merge: Option<Commit>,
}

/// A completed [`Repository::integrate_many`]: `target` moved from
/// `previous` to `merged` in one compare-and-swap, through one merge commit
/// per candidate that was not already contained. When every candidate was
/// already contained, nothing moved and `merged` equals `previous`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegratedMany {
    /// The target branch, such as `by/parent`.
    pub target: String,
    /// The target before the call: the `expected` it was given.
    pub previous: Commit,
    /// The target after the call: the last merge, or `previous`.
    pub merged: Commit,
    /// Every candidate, in the order given.
    pub candidates: Vec<MergedCandidate>,
    /// The last [`OUTPUT_TAIL_BYTES`](crate::OUTPUT_TAIL_BYTES) bytes of
    /// each check's output, in the order the checks ran.
    pub check_output_tails: Vec<String>,
    /// As [`Integrated::stale_checkouts`].
    pub stale_checkouts: Vec<PathBuf>,
}

/// What a check said about one commit; see [`Repository::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub commit: Commit,
    pub passed: bool,
    /// The check ran past its timeout and was killed; `passed` is false.
    pub timed_out: bool,
    /// The last [`OUTPUT_TAIL_BYTES`](crate::OUTPUT_TAIL_BYTES) bytes of its
    /// combined output.
    pub output_tail: String,
}

/// Why a candidate was not promoted. In every case the target ref is
/// unchanged by this call.
#[derive(Debug)]
#[non_exhaustive]
pub enum IntegrationError {
    /// The target is not at the expected commit (`actual` is `None` if the
    /// branch no longer exists). Build and validate a new candidate against
    /// the new target; earlier evidence does not transfer.
    TargetMoved {
        expected: Commit,
        actual: Option<Commit>,
    },
    /// The merge has conflicts in these paths. Return the candidate for repair.
    Conflict {
        files: Vec<String>,
    },
    /// In a [`Repository::integrate_many`], merging `candidate` (a git
    /// branch) conflicted, in `files`, with the target as it was plus the
    /// candidates in `merged`, which were merged before it in the same
    /// call (empty when it conflicts with the target alone).
    ConflictWith {
        /// The candidate's git branch.
        candidate: String,
        /// The git branches merged before it.
        merged: Vec<String>,
        /// The conflicting paths.
        files: Vec<String>,
    },
    /// The check exited unsuccessfully.
    CheckFailed {
        status: ExitStatus,
        output_tail: String,
    },
    /// The check exceeded its timeout and was killed.
    CheckTimedOut {
        timeout: Duration,
        output_tail: String,
    },
    /// The check could not be started (empty argv, program not found, ...).
    CheckNotStarted(io::Error),
    /// A worktree with the target checked out has uncommitted changes to
    /// tracked files, or files where the merge would add new ones.
    DirtyTarget {
        worktree: PathBuf,
    },
    /// The candidate head is already contained in the target.
    AlreadyIntegrated,
    /// The candidate is not a commit in this repository or does not descend
    /// from its base.
    InvalidCandidate(String),
    Git(GitError),
}

impl fmt::Display for IntegrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TargetMoved { expected, actual } => match actual {
                Some(actual) => write!(f, "target moved from {expected} to {actual}"),
                None => write!(f, "target at {expected} no longer exists"),
            },
            Self::Conflict { files } => write!(f, "merge conflicts in {}", files.join(", ")),
            Self::ConflictWith {
                candidate,
                merged,
                files,
            } => match merged.is_empty() {
                true => write!(
                    f,
                    "{candidate} conflicts with the target in {}",
                    files.join(", ")
                ),
                false => write!(
                    f,
                    "{candidate} conflicts with the target after merging {} in {}",
                    merged.join(", "),
                    files.join(", ")
                ),
            },
            Self::CheckFailed { status, .. } => write!(f, "check failed: {status}"),
            Self::CheckTimedOut { timeout, .. } => write!(f, "check timed out after {timeout:?}"),
            Self::CheckNotStarted(e) => write!(f, "check could not start: {e}"),
            Self::DirtyTarget { worktree } => write!(
                f,
                "target is checked out with uncommitted changes in {}",
                worktree.display()
            ),
            Self::AlreadyIntegrated => write!(f, "candidate is already contained in the target"),
            Self::InvalidCandidate(m) => write!(f, "invalid candidate: {m}"),
            Self::Git(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for IntegrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CheckNotStarted(e) => Some(e),
            Self::Git(e) => Some(e),
            _ => None,
        }
    }
}

impl From<GitError> for IntegrationError {
    fn from(e: GitError) -> Self {
        Self::Git(e)
    }
}

impl Repository {
    /// Promotes exactly `candidate.head`, merged into `target` (a local branch
    /// name such as `main`), provided `target` is still at `expected`.
    ///
    /// 1. Refuse with [`IntegrationError::TargetMoved`] unless
    ///    `refs/heads/<target>` is `expected`. Validate that the candidate
    ///    head descends from its base, and refuse a head already in the target.
    ///    Refuse with [`IntegrationError::DirtyTarget`] if any worktree with
    ///    `target` checked out has uncommitted changes to tracked files.
    /// 2. Check out `expected` detached in a new temporary worktree under the
    ///    scratch directory (never the user's working tree) and run
    ///    `git merge --no-ff <head>` there, with hooks and rerere disabled and
    ///    a fallback identity if none is configured. On conflict, collect
    ///    the unmerged paths, abort, and return [`IntegrationError::Conflict`].
    /// 3. Run `check` in that worktree, on the exact merge commit.
    /// 4. Re-read the target and re-check checked-out worktrees, then promote
    ///    with `git update-ref refs/heads/<target> <merged> <expected>`, an
    ///    atomic compare-and-swap. If the ref moved meanwhile, return
    ///    `TargetMoved`; the concurrent commit is kept.
    /// 5. For each worktree with `target` checked out, move its index and files
    ///    from `expected` to `merged` with `git read-tree -m -u`, a two-tree
    ///    checkout that refuses to overwrite local changes. `merge --ff-only`
    ///    and `reset --keep` do not work here: after the CAS, HEAD already
    ///    names `merged`. A worktree that cannot be moved is reported in
    ///    [`Integrated::stale_checkouts`]; the promotion itself stands.
    /// 6. The temporary worktree is removed on success, on every error, and
    ///    on panic.
    ///
    /// Crash safety: nothing before step 4 writes any ref other than the
    /// detached temporary worktree's HEAD, so an interruption leaves the
    /// target untouched (at worst a stray `branchyard-integrate-*` directory
    /// and worktree registration; `git worktree prune` clears the latter once
    /// the directory is deleted). Step 4 is a single ref transaction. After
    /// an interruption, compare the target with `expected` and use
    /// [`Repository::is_ancestor`] on the candidate head to learn whether the
    /// promotion happened. A dirty-tree edit made between the final check and
    /// the CAS is not detected before promotion, but `read-tree -m -u` will
    /// not overwrite it.
    pub fn integrate(
        &self,
        candidate: &Candidate,
        target: &str,
        expected: &Commit,
        check: Option<&Check>,
    ) -> Result<Integrated, IntegrationError> {
        let target_ref = self.target_ref(target)?;
        if !is_object_id(expected.as_str()) {
            return Err(GitError::InvalidRevision(expected.0.clone()).into());
        }
        let actual = self.read_ref(&target_ref)?;
        if actual.as_ref() != Some(expected) {
            return Err(IntegrationError::TargetMoved {
                expected: expected.clone(),
                actual,
            });
        }
        self.validate_candidate(candidate, expected)?;
        for worktree in self.checkouts_of(&target_ref)? {
            if blocks_update(&worktree, expected, None)? {
                return Err(IntegrationError::DirtyTarget { worktree });
            }
        }

        let scratch = TempWorktree::create(self, expected)?;
        let merged = merge(&scratch.path, candidate, target, expected)?;

        let check_output_tail = match check {
            None => None,
            Some(check) => match check::run(check, &scratch.path) {
                Err(e) => return Err(IntegrationError::CheckNotStarted(e)),
                Ok((CheckOutcome::Exited(status), tail)) if status.success() => Some(tail),
                Ok((CheckOutcome::Exited(status), output_tail)) => {
                    return Err(IntegrationError::CheckFailed {
                        status,
                        output_tail,
                    })
                }
                Ok((CheckOutcome::TimedOut, output_tail)) => {
                    return Err(IntegrationError::CheckTimedOut {
                        timeout: check.timeout,
                        output_tail,
                    })
                }
            },
        };

        let reason = format!("branchyard: integrate {}", candidate.branch.branch());
        let stale_checkouts = self.promote(&target_ref, expected, &merged, &reason)?;
        drop(scratch);
        Ok(Integrated {
            target: target.to_owned(),
            previous: expected.clone(),
            merged,
            check_output_tail,
            stale_checkouts,
        })
    }

    /// Promotes every candidate in `candidates`, merged into `target` in
    /// order, provided `target` is still at `expected`: all of them, or
    /// none.
    ///
    /// The same steps and crash safety as [`Repository::integrate`], with
    /// one temporary worktree for the whole call: each candidate is merged
    /// with `git merge --no-ff` on top of the ones before it, every check
    /// in `checks` runs once, on the final merge, and the target moves with
    /// one compare-and-swap from `expected` to that merge. A candidate the
    /// target already contains, or one a candidate merged before it brought
    /// in, gets no merge commit ([`MergedCandidate::merge`] is `None`); it
    /// is not an error. When none needs a merge, nothing runs and the
    /// target does not move.
    ///
    /// A conflict is [`IntegrationError::ConflictWith`], naming the
    /// candidate that conflicted, the ones merged before it and the files.
    /// Any error leaves the target where it was.
    pub fn integrate_many(
        &self,
        candidates: &[Candidate],
        target: &str,
        expected: &Commit,
        checks: &[Check],
    ) -> Result<IntegratedMany, IntegrationError> {
        let target_ref = self.target_ref(target)?;
        if !is_object_id(expected.as_str()) {
            return Err(GitError::InvalidRevision(expected.0.clone()).into());
        }
        let actual = self.read_ref(&target_ref)?;
        if actual.as_ref() != Some(expected) {
            return Err(IntegrationError::TargetMoved {
                expected: expected.clone(),
                actual,
            });
        }
        let mut pending = Vec::new();
        let mut results = Vec::new();
        for candidate in candidates {
            match self.validate_candidate(candidate, expected) {
                Ok(()) => pending.push(candidate),
                Err(IntegrationError::AlreadyIntegrated) => {}
                Err(error) => return Err(error),
            }
        }
        if pending.is_empty() {
            return Ok(IntegratedMany {
                target: target.to_owned(),
                previous: expected.clone(),
                merged: expected.clone(),
                candidates: candidates
                    .iter()
                    .map(|c| MergedCandidate {
                        branch: c.branch.branch(),
                        head: c.head.clone(),
                        onto: expected.clone(),
                        merge: None,
                    })
                    .collect(),
                check_output_tails: Vec::new(),
                stale_checkouts: Vec::new(),
            });
        }
        for worktree in self.checkouts_of(&target_ref)? {
            if blocks_update(&worktree, expected, None)? {
                return Err(IntegrationError::DirtyTarget { worktree });
            }
        }

        let scratch = TempWorktree::create(self, expected)?;
        let mut head = expected.clone();
        let mut merged_names: Vec<String> = Vec::new();
        for candidate in candidates {
            let name = candidate.branch.branch();
            if !pending.iter().any(|p| std::ptr::eq(*p, candidate)) {
                results.push(MergedCandidate {
                    branch: name,
                    head: candidate.head.clone(),
                    onto: head.clone(),
                    merge: None,
                });
                continue;
            }
            let merge_commit = match merge(&scratch.path, candidate, target, &head) {
                Ok(commit) => Some(commit),
                // Brought in by a candidate merged before it.
                Err(IntegrationError::AlreadyIntegrated) => None,
                Err(IntegrationError::Conflict { files }) => {
                    return Err(IntegrationError::ConflictWith {
                        candidate: name,
                        merged: merged_names,
                        files,
                    })
                }
                Err(error) => return Err(error),
            };
            let onto = head.clone();
            if let Some(commit) = &merge_commit {
                head = commit.clone();
                merged_names.push(name.clone());
            }
            results.push(MergedCandidate {
                branch: name,
                head: candidate.head.clone(),
                onto,
                merge: merge_commit,
            });
        }
        let merged = head;
        if &merged == expected {
            drop(scratch);
            return Ok(IntegratedMany {
                target: target.to_owned(),
                previous: expected.clone(),
                merged,
                candidates: results,
                check_output_tails: Vec::new(),
                stale_checkouts: Vec::new(),
            });
        }

        let mut check_output_tails = Vec::new();
        for check in checks {
            match check::run(check, &scratch.path) {
                Err(e) => return Err(IntegrationError::CheckNotStarted(e)),
                Ok((CheckOutcome::Exited(status), tail)) if status.success() => {
                    check_output_tails.push(tail)
                }
                Ok((CheckOutcome::Exited(status), output_tail)) => {
                    return Err(IntegrationError::CheckFailed {
                        status,
                        output_tail,
                    })
                }
                Ok((CheckOutcome::TimedOut, output_tail)) => {
                    return Err(IntegrationError::CheckTimedOut {
                        timeout: check.timeout,
                        output_tail,
                    })
                }
            }
        }

        let names: Vec<String> = candidates.iter().map(|c| c.branch.branch()).collect();
        let reason = format!("branchyard: integrate {}", names.join(", "));
        let stale_checkouts = self.promote(&target_ref, expected, &merged, &reason)?;
        drop(scratch);
        Ok(IntegratedMany {
            target: target.to_owned(),
            previous: expected.clone(),
            merged,
            candidates: results,
            check_output_tails,
            stale_checkouts,
        })
    }

    /// Steps 4 and 5 of [`Repository::integrate`]: the compare-and-swap of
    /// `target_ref` from `expected` to `merged`, then the checkouts moved.
    /// Returns the checkouts that could not be moved.
    fn promote(
        &self,
        target_ref: &str,
        expected: &Commit,
        merged: &Commit,
        reason: &str,
    ) -> Result<Vec<PathBuf>, IntegrationError> {
        let actual = self.read_ref(target_ref)?;
        if actual.as_ref() != Some(expected) {
            return Err(IntegrationError::TargetMoved {
                expected: expected.clone(),
                actual,
            });
        }
        let checkouts = self.checkouts_of(target_ref)?;
        for worktree in &checkouts {
            if blocks_update(worktree, expected, Some(merged))? {
                return Err(IntegrationError::DirtyTarget {
                    worktree: worktree.clone(),
                });
            }
        }
        let (out, args) = Git::new(&self.root)
            .args(["update-ref", "-m", reason, target_ref])
            .args([merged.as_str(), expected.as_str()])
            .output()?;
        if !out.status.success() {
            let actual = self.read_ref(target_ref)?;
            if actual.as_ref() != Some(expected) {
                return Err(IntegrationError::TargetMoved {
                    expected: expected.clone(),
                    actual,
                });
            }
            return Err(git::failed(args, &out).into());
        }
        let mut stale_checkouts = Vec::new();
        for worktree in checkouts {
            best_effort(
                "refresh the git index of a checkout",
                Git::new(&worktree)
                    .args(["update-index", "-q", "--refresh"])
                    .output(),
            );
            let moved = Git::new(&worktree)
                .no_hooks()
                .args(["read-tree", "-m", "-u", expected.as_str(), merged.as_str()])
                .run();
            if moved.is_err() {
                stale_checkouts.push(worktree);
            }
        }
        Ok(stale_checkouts)
    }

    /// The first commit on `target`'s first-parent line, oldest first,
    /// that contains `commit`: the merge (or commit) that brought it in.
    /// `None` when `target` does not contain it.
    pub fn brought_in_by(&self, commit: &Commit, target: &str) -> Result<Option<Commit>, GitError> {
        let target_ref = self.target_ref(target)?;
        if !self.is_ancestor(commit, &Commit(target_ref.clone()))? {
            return Ok(None);
        }
        let range = format!("{}..{target_ref}", commit.as_str());
        let listed = Git::new(&self.root)
            .args(["rev-list", "--first-parent", "--reverse"])
            .arg(&range)
            .run()?;
        // An empty range: the commit is the target's head, or on its line.
        let mut first = None;
        for line in listed.lines().filter(|l| !l.is_empty()) {
            let candidate = Commit(line.trim().to_owned());
            if self.is_ancestor(commit, &candidate)? {
                first = Some(candidate);
                break;
            }
        }
        Ok(Some(first.unwrap_or_else(|| commit.clone())))
    }

    /// Runs `check` on exactly `commit`, checked out detached in a new
    /// temporary worktree under the scratch directory (removed on every
    /// return path), without merging it anywhere. A check that exits
    /// unsuccessfully or times out is a [`Verified`] with `passed` false,
    /// not an error; one that cannot start is
    /// [`IntegrationError::CheckNotStarted`].
    pub fn verify(&self, commit: &Commit, check: &Check) -> Result<Verified, IntegrationError> {
        let commit = self.resolve(commit.as_str())?;
        let scratch = TempWorktree::create(self, &commit)?;
        let verified = match check::run(check, &scratch.path) {
            Err(e) => return Err(IntegrationError::CheckNotStarted(e)),
            Ok((CheckOutcome::Exited(status), output_tail)) => Verified {
                commit,
                passed: status.success(),
                timed_out: false,
                output_tail,
            },
            Ok((CheckOutcome::TimedOut, output_tail)) => Verified {
                commit,
                passed: false,
                timed_out: true,
                output_tail,
            },
        };
        drop(scratch);
        Ok(verified)
    }

    fn target_ref(&self, target: &str) -> Result<String, GitError> {
        let target_ref = format!("refs/heads/{target}");
        if target.is_empty()
            || target.starts_with('-')
            || !Git::new(&self.root)
                .args(["check-ref-format", &target_ref])
                .test()?
        {
            return Err(GitError::InvalidRef(target.to_owned()));
        }
        Ok(target_ref)
    }

    fn read_ref(&self, full_ref: &str) -> Result<Option<Commit>, GitError> {
        let (out, args) = Git::new(&self.root)
            .args(["rev-parse", "--verify", "--quiet", full_ref])
            .output()?;
        match out.status.code() {
            Some(0) => Ok(Some(Commit(
                String::from_utf8_lossy(&out.stdout).trim().to_owned(),
            ))),
            Some(1) => Ok(None),
            _ => Err(git::failed(args, &out)),
        }
    }

    fn validate_candidate(
        &self,
        candidate: &Candidate,
        expected: &Commit,
    ) -> Result<(), IntegrationError> {
        for (what, commit) in [("head", &candidate.head), ("base", &candidate.base)] {
            let exact = is_object_id(commit.as_str())
                && resolve_in(&self.root, commit.as_str()).ok().as_ref() == Some(commit);
            if !exact {
                return Err(IntegrationError::InvalidCandidate(format!(
                    "{what} {commit} is not a full commit ID in this repository"
                )));
            }
        }
        if !self.is_ancestor(&candidate.base, &candidate.head)? {
            return Err(IntegrationError::InvalidCandidate(format!(
                "head {} does not descend from base {}",
                candidate.head, candidate.base
            )));
        }
        if self.is_ancestor(&candidate.head, expected)? {
            return Err(IntegrationError::AlreadyIntegrated);
        }
        Ok(())
    }

    /// Worktrees (with a directory present) that have `full_ref` checked out.
    fn checkouts_of(&self, full_ref: &str) -> Result<Vec<PathBuf>, GitError> {
        Ok(git::worktrees(&self.root)?
            .into_iter()
            .filter(|w| !w.bare && !w.prunable && w.branch.as_deref() == Some(full_ref))
            .map(|w| w.path)
            .collect())
    }
}

/// Merges the candidate head into the detached `expected` in `dir`; returns
/// the merge commit.
fn merge(
    dir: &Path,
    candidate: &Candidate,
    target: &str,
    expected: &Commit,
) -> Result<Commit, IntegrationError> {
    let message = format!(
        "Merge {} ({}) into {target}",
        candidate.branch.branch(),
        candidate.head
    );
    let (out, args) = Git::new(dir)
        .no_hooks()
        .args(["-c", "rerere.enabled=false"])
        .args(identity_args(dir)?)
        .args(["merge", "--no-ff", "--no-edit", "--quiet", "-m", &message])
        .arg(candidate.head.as_str())
        .output()?;
    if !out.status.success() {
        let unmerged = Git::new(dir)
            .args(["diff", "--name-only", "-z", "--diff-filter=U"])
            .run();
        best_effort(
            "abort the merge",
            Git::new(dir).args(["merge", "--abort"]).output(),
        );
        let files: Vec<String> = unmerged
            .unwrap_or_default()
            .split('\0')
            .filter(|f| !f.is_empty())
            .map(str::to_owned)
            .collect();
        if files.is_empty() {
            return Err(git::failed(args, &out).into());
        }
        return Err(IntegrationError::Conflict { files });
    }
    let merged = resolve_in(dir, "HEAD")?;
    if &merged == expected {
        return Err(IntegrationError::AlreadyIntegrated);
    }
    Ok(merged)
}

/// Whether updating the checkout at `worktree` from `from` to `to` could lose
/// work: modified or staged tracked files, or (with `to`) existing files at
/// paths the merge adds.
fn blocks_update(worktree: &Path, from: &Commit, to: Option<&Commit>) -> Result<bool, GitError> {
    let status = Git::new(worktree)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=no",
            "--ignore-submodules=none",
        ])
        .run()?;
    if !status.is_empty() {
        return Ok(true);
    }
    let Some(to) = to else {
        return Ok(false);
    };
    let added = Git::new(worktree)
        .args([
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=A",
        ])
        .args([from.as_str(), to.as_str(), "--"])
        .run()?;
    Ok(added
        .split('\0')
        .filter(|p| !p.is_empty())
        .any(|p| fs::symlink_metadata(worktree.join(p)).is_ok()))
}

/// A detached worktree that is removed when dropped.
/// What [`Repository::check_commit`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    Passed { output_tail: String },
    Failed { output_tail: String },
    TimedOut { output_tail: String },
}

impl Repository {
    /// Run `check` on `commit` exactly, in a private temporary worktree
    /// that is removed on every return path; nothing else is changed.
    /// Fails if the worktree cannot be created or the check cannot start.
    pub fn check_commit(&self, commit: &Commit, check: &Check) -> Result<CheckResult, GitError> {
        let scratch = TempWorktree::create(self, commit)?;
        match check::run(check, &scratch.path)? {
            (CheckOutcome::Exited(status), output_tail) if status.success() => {
                Ok(CheckResult::Passed { output_tail })
            }
            (CheckOutcome::Exited(_), output_tail) => Ok(CheckResult::Failed { output_tail }),
            (CheckOutcome::TimedOut, output_tail) => Ok(CheckResult::TimedOut { output_tail }),
        }
    }
}

struct TempWorktree {
    root: PathBuf,
    path: PathBuf,
}

impl TempWorktree {
    fn create(repo: &Repository, at: &Commit) -> Result<Self, GitError> {
        fs::create_dir_all(&repo.scratch)?;
        let path = repo
            .scratch
            .join(format!("branchyard-integrate-{}", unique_suffix()));
        // Constructed first so a partially created worktree is cleaned up.
        let worktree = TempWorktree {
            root: repo.root.clone(),
            path,
        };
        Git::new(&repo.root)
            .no_hooks()
            .args(["worktree", "add", "--quiet", "--detach"])
            .arg(&worktree.path)
            .arg(at.as_str())
            .run()?;
        Ok(worktree)
    }
}

impl Drop for TempWorktree {
    fn drop(&mut self) {
        let removed = Git::new(&self.root)
            .args(["worktree", "remove", "--force", "--force"])
            .arg(&self.path)
            .run();
        if removed.is_err() || self.path.exists() {
            branchyard_support::cleanup_dir(&self.path);
            best_effort(
                "git worktree prune",
                Git::new(&self.root).args(["worktree", "prune"]).run(),
            );
        }
    }
}
