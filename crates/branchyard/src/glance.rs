//! A quick, read-only look at a repository's branches and messages, for
//! callers that must never change anything or wait long: `by hook`, which a
//! Claude Code session runs before each prompt, and `by statusline`, which
//! it runs on every redraw (`docs/harness-integration.md`).
//!
//! [`Yard::open`](crate::Yard::open) is not that: it creates `.branchyard/`,
//! upgrades an older store and recovers branches whose engine stopped,
//! killing their processes. [`read`] opens `.branchyard/state.db` through a
//! read-only connection, waits at most [`READ_WAIT`] for a writer, and reads
//! three things. A repository whose state lives in PostgreSQL has no
//! `state.db`, so it reads as no yard.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags};

use crate::state::Record;
use crate::store_codec::decode;
use crate::{BranchInfo, BranchStatus, Error, Message};

/// How long a read waits for another process's write lock.
pub const READ_WAIT: Duration = Duration::from_millis(300);

/// At most this many unanswered questions are read, the newest.
const QUESTIONS_MAX: u32 = 200;

/// What [`read`] found.
#[derive(Clone, Debug, PartialEq)]
pub struct Glance {
    /// The repository root, where `.branchyard/` is.
    pub root: PathBuf,
    /// Every branch, oldest first.
    pub branches: Vec<BranchInfo>,
    /// Questions and escalations that no answer replies to yet and whose
    /// sender is still running a turn, oldest first: what a branch is
    /// asking now.
    pub asking: Vec<Message>,
    /// The messages addressed to the branch [`read`] was given that no
    /// turn has been given yet, oldest first; empty without one.
    pub unread: Vec<Message>,
}

impl Glance {
    /// How many branches are in each of the states a person acts on, in
    /// the order a status line shows them.
    pub fn counts(&self) -> Counts {
        let mut counts = Counts {
            asking: self.asking.len(),
            ..Counts::default()
        };
        for info in &self.branches {
            match info.status {
                BranchStatus::Running | BranchStatus::WaitingOnChildren => counts.running += 1,
                BranchStatus::Waiting => counts.waiting += 1,
                BranchStatus::Blocked { .. } | BranchStatus::AwaitingPlanApproval => {
                    counts.blocked += 1
                }
                BranchStatus::Ready => counts.ready += 1,
                BranchStatus::Failed { .. }
                | BranchStatus::BudgetExceeded { .. }
                | BranchStatus::Interrupted => counts.failed += 1,
                BranchStatus::NoChanges
                | BranchStatus::Merged { .. }
                | BranchStatus::Discarded { .. } => {}
            }
        }
        counts
    }

    /// The branch whose worktree holds `dir`, if any.
    pub fn branch_at(&self, dir: &Path) -> Option<&BranchInfo> {
        self.branches
            .iter()
            .filter(|info| !info.worktree.as_os_str().is_empty() && dir.starts_with(&info.worktree))
            .max_by_key(|info| info.worktree.as_os_str().len())
    }
}

/// Branch counts by state; see [`Glance::counts`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Running a turn, or parked until its children settle.
    pub running: usize,
    /// Created, waiting for prerequisites.
    pub waiting: usize,
    /// Unanswered questions and escalations from running branches.
    pub asking: usize,
    /// Blocked, or waiting for its plan to be approved.
    pub blocked: usize,
    /// Its last turn produced a candidate not yet integrated.
    pub ready: usize,
    /// Failed, interrupted or stopped at a limit.
    pub failed: usize,
}

/// The repository root above `start` (itself included) that has a
/// `.branchyard/state.db`, nearest first. A branch's worktree, inside
/// `.branchyard/worktrees/`, finds the repository that holds it.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| state_db(dir).is_file())
        .map(Path::to_path_buf)
}

/// The `.branchyard` directory of the repository at `root`.
pub fn state_dir(root: &Path) -> PathBuf {
    crate::state::dir(root)
}

fn state_db(root: &Path) -> PathBuf {
    state_dir(root).join("state.db")
}

/// Read `root`'s branches, what they are asking, and, given `inbox_of`,
/// that branch's unread messages, without creating, upgrading or
/// recovering anything. `Ok(None)` when `root` has no `state.db`.
///
/// # Errors
///
/// [`Error::State`] when the database cannot be read within [`READ_WAIT`]
/// or holds a record this build cannot decode.
pub fn read(root: &Path, inbox_of: Option<&str>) -> Result<Option<Glance>, Error> {
    let path = state_db(root);
    if !path.is_file() {
        return Ok(None);
    }
    let failed = |what: &str, error: rusqlite::Error| {
        Error::State(format!("{} ({what}): {error}", path.display()))
    };
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| failed("open", e))?;
    conn.busy_timeout(READ_WAIT)
        .map_err(|e| failed("busy timeout", e))?;
    let branches = branches(&conn).map_err(|e| failed("branches", e))??;
    let asking = questions(&conn)
        .map_err(|e| failed("questions", e))?
        .into_iter()
        .filter(|m| {
            branches
                .iter()
                .any(|b| b.name == m.from && b.status == BranchStatus::Running)
        })
        .collect();
    let unread = match inbox_of {
        Some(branch) => unread(&conn, branch).map_err(|e| failed("inbox", e))?,
        None => Vec::new(),
    };
    Ok(Some(Glance {
        root: root.to_path_buf(),
        branches,
        asking,
        unread,
    }))
}

/// Every branch, oldest first; decoding errors are the inner result.
fn branches(conn: &Connection) -> rusqlite::Result<Result<Vec<BranchInfo>, Error>> {
    let mut statement = conn.prepare(
        "SELECT name, record FROM branches WHERE record IS NOT NULL ORDER BY created_ms, name",
    )?;
    let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut infos = Vec::new();
    for row in rows {
        let (name, text) = row?;
        match decode::<Record>(&format!("record {name}"), &text) {
            Ok(record) => infos.push(record.info),
            Err(error) => return Ok(Err(error)),
        }
    }
    Ok(Ok(infos))
}

const MESSAGE_COLUMNS: &str =
    "id, from_branch, to_branch, kind, text, in_reply_to, at_ms, delivered_ms";

fn questions(conn: &Connection) -> rusqlite::Result<Vec<Message>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {MESSAGE_COLUMNS} FROM messages q \
         WHERE kind IN ('question', 'escalation') \
         AND NOT EXISTS (SELECT 1 FROM messages a WHERE a.in_reply_to = q.id) \
         ORDER BY id DESC LIMIT ?1"
    ))?;
    let mut newest: Vec<Message> = statement
        .query_map(params![QUESTIONS_MAX], crate::sqlite::message_from)?
        .collect::<rusqlite::Result<_>>()?;
    newest.reverse();
    Ok(newest)
}

fn unread(conn: &Connection, branch: &str) -> rusqlite::Result<Vec<Message>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {MESSAGE_COLUMNS} FROM messages \
         WHERE to_branch = ?1 AND delivered_ms IS NULL ORDER BY id"
    ))?;
    let rows = statement.query_map(params![branch], crate::sqlite::message_from)?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Yard;

    fn repository() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("by-glance-")
            .tempdir()
            .unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    #[test]
    fn no_state_database_is_no_yard_and_nothing_is_created() {
        let dir = repository();
        assert_eq!(read(dir.path(), None).unwrap(), None);
        assert_eq!(find_root(dir.path()), None);
        assert!(!dir.path().join(".branchyard").exists());
    }

    #[test]
    fn an_empty_yard_reads_from_below_its_root() {
        let dir = repository();
        drop(Yard::open(dir.path()).unwrap());
        let below = dir.path().join(".branchyard/worktrees/x/src");
        std::fs::create_dir_all(&below).unwrap();
        let root = find_root(&below).unwrap();
        assert_eq!(root, dir.path());
        let glance = read(&root, Some("x")).unwrap().unwrap();
        assert!(glance.branches.is_empty() && glance.asking.is_empty());
        assert!(glance.unread.is_empty());
        assert_eq!(glance.counts(), Counts::default());
        assert_eq!(glance.branch_at(&below), None);
    }

    #[test]
    fn a_file_that_is_not_a_database_is_an_error() {
        let dir = repository();
        std::fs::create_dir_all(dir.path().join(".branchyard")).unwrap();
        std::fs::write(
            dir.path().join(".branchyard/state.db"),
            "this is not a database, and is longer than a header",
        )
        .unwrap();
        assert!(read(dir.path(), None).is_err());
    }
}
