//! Git-native workspaces for Branchyard: one branch per child agent, one
//! commit per candidate, and validated promotion into a target branch.
//!
//! Every child works on `by/<name>` in its own `git worktree`. A
//! [`Workspace::snapshot`] turns its state into a [`Candidate`], an exact
//! commit. [`Repository::integrate`] merges that commit in a private
//! temporary worktree, runs a trusted [`Check`] on the exact merge result,
//! and promotes it with a compare-and-swap of the target ref against the
//! commit the caller expected.
//!
//! Guarantees:
//! - The target ref changes only through that compare-and-swap, and only to
//!   a merge commit whose parents are the expected target and the candidate
//!   head. A moved target, conflict, failed or timed-out check, or dirty
//!   checkout leaves it unchanged.
//! - Merges and checks never run in the user's working tree. The temporary
//!   worktree is removed on every return path.
//! - An interruption before the compare-and-swap leaves the target
//!   untouched. The swap is a single ref update.
//! - git runs from argument vectors, never a shell, with hooks disabled for
//!   Branchyard's own commits, merges, and checkouts.
//!
//! Not guaranteed:
//! - Authorization, sandboxing, or trust in the check command; the caller
//!   decides what may run and where.
//! - Durable recording. Git and the caller's database do not share a
//!   transaction; the caller persists the promotion intent (expected and
//!   merged commits) and reconciles against the actual ref after a crash.
//! - Protection against processes that write the repository's files or refs
//!   without going through git's own locking.
//! - Portability beyond Unix-like hosts with git 2.36 or later.
mod branch;
mod check;
mod git;
mod integrate;
mod repo;

pub use branch::{BranchName, InvalidBranchName, BRANCH_PREFIX};
pub use check::{Check, OUTPUT_TAIL_BYTES};
pub use git::GitError;
pub use integrate::{CheckResult, Integrated, IntegrationError, Verified};
pub use repo::{Candidate, Commit, DiffStat, Repository, Workspace};
