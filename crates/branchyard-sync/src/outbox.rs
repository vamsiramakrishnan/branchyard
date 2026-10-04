//! The durable outbox: one SQLite file (`.branchyard/sync.db` in a
//! repository, the data directory on a server), mode 0600, in
//! write-ahead-log mode.
//!
//! | Table | Key | Holds |
//! |---|---|---|
//! | `meta` | `key` | this machine's device name |
//! | `outbox` | `(remote, task)` | a task waiting to be pushed: when first queued, tries, when next due, the last error |
//! | `tasks` | `(remote, task)` | what this machine knows of the task in that remote ([`TaskState`]) |
//! | `uploads` | `key` | resumable uploads' progress (upload IDs, sessions, parts) |
//! | `log` | `id` | recent sync outcomes, the last 500 kept |
//! | `counters` | `(remote, name)` | bytes, objects, swaps, conflicts and retries, summed over every process |
//!
//! Queuing a task twice keeps one row with the earlier time, so a burst of
//! turns becomes one push, and the lag reported is from the oldest change
//! not yet pushed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::engine::{sanitize_device, TaskState};
use crate::error::Result;
use crate::stats::Snapshot;
use crate::store::UploadJournal;

pub struct Outbox {
    conn: Mutex<Connection>,
    path: PathBuf,
}

/// A queued task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub task: String,
    pub enqueued_ms: u64,
    pub attempts: u32,
    pub next_ms: u64,
    pub last_error: Option<String>,
}

/// A recorded outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub at_ms: u64,
    pub task: String,
    pub outcome: String,
    pub detail: String,
}

/// `due` with no deadline, or one past what a column holds: everything is
/// due. A bound for a query, never a stored value.
const NO_DEADLINE: i64 = i64::MAX;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS outbox (
    remote TEXT NOT NULL, task TEXT NOT NULL, enqueued_ms INTEGER NOT NULL,
    touched_ms INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, next_ms INTEGER NOT NULL, last_error TEXT,
    PRIMARY KEY (remote, task));
CREATE TABLE IF NOT EXISTS tasks (
    remote TEXT NOT NULL, task TEXT NOT NULL, state TEXT NOT NULL,
    PRIMARY KEY (remote, task));
CREATE TABLE IF NOT EXISTS uploads (key TEXT PRIMARY KEY, state TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS log (
    id INTEGER PRIMARY KEY AUTOINCREMENT, remote TEXT NOT NULL, at_ms INTEGER NOT NULL,
    task TEXT NOT NULL, outcome TEXT NOT NULL, detail TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS counters (
    remote TEXT NOT NULL, name TEXT NOT NULL, value INTEGER NOT NULL,
    PRIMARY KEY (remote, name));
";

impl Outbox {
    pub fn open(path: &Path) -> Result<Outbox> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Outbox {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        })
    }

    /// `.branchyard/sync.db` under a repository root.
    pub fn path_for(root: &Path) -> PathBuf {
        root.join(".branchyard").join("sync.db")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(f(&conn)?)
    }

    /// This machine's device name: `configured` when given, else one made
    /// once from the host name and kept.
    pub fn device(&self, configured: Option<&str>) -> Result<String> {
        if let Some(name) = configured {
            return Ok(sanitize_device(name));
        }
        let existing: Option<String> = self.with(|c| {
            c.query_row("SELECT value FROM meta WHERE key = 'device'", [], |r| {
                r.get(0)
            })
            .optional()
        })?;
        if let Some(name) = existing {
            return Ok(name);
        }
        let name = format!(
            "{}-{}",
            crate::engine::default_device(),
            crate::util::hex(&crate::util::random_bytes(2)?)
        );
        self.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO meta (key, value) VALUES ('device', ?1)",
                params![name],
            )?;
            c.query_row("SELECT value FROM meta WHERE key = 'device'", [], |r| {
                r.get(0)
            })
        })
    }

    pub fn state(&self, remote: &str, task: &str) -> Result<TaskState> {
        let text: Option<String> = self.with(|c| {
            c.query_row(
                "SELECT state FROM tasks WHERE remote = ?1 AND task = ?2",
                params![remote, task],
                |r| r.get(0),
            )
            .optional()
        })?;
        Ok(match text {
            Some(t) => serde_json::from_str(&t)?,
            None => TaskState::default(),
        })
    }

    pub fn save_state(&self, remote: &str, task: &str, state: &TaskState) -> Result<()> {
        let text = serde_json::to_string(state)?;
        self.with(|c| {
            c.execute(
                "INSERT INTO tasks (remote, task, state) VALUES (?1, ?2, ?3)
                 ON CONFLICT (remote, task) DO UPDATE SET state = excluded.state",
                params![remote, task, text],
            )
        })?;
        Ok(())
    }

    /// Every task this machine has synced with `remote`.
    pub fn tasks(&self, remote: &str) -> Result<Vec<(String, TaskState)>> {
        let rows: Vec<(String, String)> = self.with(|c| {
            let mut s =
                c.prepare("SELECT task, state FROM tasks WHERE remote = ?1 ORDER BY task")?;
            let rows = s
                .query_map(params![remote], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        rows.into_iter()
            .map(|(t, s)| Ok((t, serde_json::from_str(&s)?)))
            .collect()
    }

    /// Queue `task` for a push, due now.
    pub fn enqueue(&self, remote: &str, task: &str, now_ms: u64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO outbox (remote, task, enqueued_ms, touched_ms, next_ms)
                 VALUES (?1, ?2, ?3, ?3, ?3)
                 ON CONFLICT (remote, task) DO UPDATE SET
                     next_ms = MIN(next_ms, excluded.next_ms),
                     touched_ms = MAX(touched_ms, excluded.touched_ms)",
                params![remote, task, now_ms as i64],
            )
        })?;
        Ok(())
    }

    fn rows(&self, remote: &str, due_by: Option<u64>) -> Result<Vec<Pending>> {
        self.with(|c| {
            let mut s = c.prepare(
                "SELECT task, enqueued_ms, attempts, next_ms, last_error FROM outbox
                 WHERE remote = ?1 AND next_ms <= ?2 ORDER BY enqueued_ms, task",
            )?;
            let rows = s
                .query_map(
                    params![
                        remote,
                        due_by.map_or(NO_DEADLINE, |d| i64::try_from(d).map_or(NO_DEADLINE, |v| v))
                    ],
                    |r| {
                        Ok(Pending {
                            task: r.get(0)?,
                            enqueued_ms: r.get::<_, i64>(1)? as u64,
                            attempts: r.get::<_, i64>(2)? as u32,
                            next_ms: r.get::<_, i64>(3)? as u64,
                            last_error: r.get(4)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Tasks due by `now_ms`, oldest first.
    pub fn due(&self, remote: &str, now_ms: u64) -> Result<Vec<Pending>> {
        self.rows(remote, Some(now_ms))
    }

    /// Every queued task.
    pub fn pending(&self, remote: &str) -> Result<Vec<Pending>> {
        self.rows(remote, None)
    }

    /// The task was pushed: drop its row, unless it was queued again after
    /// `started_ms` (then it stays, due now).
    pub fn done(&self, remote: &str, task: &str, started_ms: u64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM outbox WHERE remote = ?1 AND task = ?2 AND touched_ms <= ?3",
                params![remote, task, started_ms as i64],
            )
        })?;
        Ok(())
    }

    /// The push failed: try again at `next_ms`.
    pub fn failed(&self, remote: &str, task: &str, error: &str, next_ms: u64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE outbox SET attempts = attempts + 1, next_ms = ?3, last_error = ?4
                 WHERE remote = ?1 AND task = ?2",
                params![remote, task, next_ms as i64, error],
            )
        })?;
        Ok(())
    }

    pub fn record(
        &self,
        remote: &str,
        task: &str,
        outcome: &str,
        detail: &str,
        at_ms: u64,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO log (remote, at_ms, task, outcome, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![remote, at_ms as i64, task, outcome, detail],
            )?;
            c.execute(
                "DELETE FROM log WHERE id <= (SELECT MAX(id) FROM log) - 500",
                [],
            )
        })?;
        Ok(())
    }

    pub fn recent(&self, remote: &str, limit: usize) -> Result<Vec<LogEntry>> {
        self.with(|c| {
            let mut s = c.prepare(
                "SELECT at_ms, task, outcome, detail FROM log WHERE remote = ?1
                 ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = s
                .query_map(params![remote, limit as i64], |r| {
                    Ok(LogEntry {
                        at_ms: r.get::<_, i64>(0)? as u64,
                        task: r.get(1)?,
                        outcome: r.get(2)?,
                        detail: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Add a process's counters (since it last added) to the totals.
    pub fn add_counters(&self, remote: &str, delta: &Snapshot) -> Result<()> {
        self.with(|c| {
            for (name, value) in delta.pairs() {
                if value == 0 {
                    continue;
                }
                c.execute(
                    "INSERT INTO counters (remote, name, value) VALUES (?1, ?2, ?3)
                     ON CONFLICT (remote, name) DO UPDATE SET value = value + excluded.value",
                    params![remote, name, value as i64],
                )?;
            }
            Ok(())
        })
    }

    pub fn counters(&self, remote: &str) -> Result<Snapshot> {
        let rows: Vec<(String, i64)> = self.with(|c| {
            let mut s = c.prepare("SELECT name, value FROM counters WHERE remote = ?1")?;
            let rows = s
                .query_map(params![remote], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        let mut value = serde_json::Map::new();
        for (name, n) in rows {
            value.insert(name, serde_json::Value::from(n));
        }
        Ok(serde_json::from_value(serde_json::Value::Object(value)).unwrap_or_default())
    }
}

/// Resumable uploads' progress, in the outbox.
pub struct OutboxJournal(pub Arc<Outbox>);

impl UploadJournal for OutboxJournal {
    fn load(&self, key: &str) -> Option<String> {
        self.0
            .with(|c| {
                c.query_row(
                    "SELECT state FROM uploads WHERE key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()
            })
            .ok()
            .flatten()
    }

    fn save(&self, key: &str, state: &str) {
        let _ = self.0.with(|c| {
            c.execute(
                "INSERT INTO uploads (key, state) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET state = excluded.state",
                params![key, state],
            )
        });
    }

    fn clear(&self, key: &str) {
        let _ = self
            .0
            .with(|c| c.execute("DELETE FROM uploads WHERE key = ?1", params![key]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_outbox_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.db");
        {
            let outbox = Outbox::open(&path).unwrap();
            outbox.enqueue("r", "a", 10).unwrap();
            outbox.enqueue("r", "a", 20).unwrap();
            outbox.enqueue("r", "b", 15).unwrap();
            let journal = OutboxJournal(Arc::new(Outbox::open(&path).unwrap()));
            journal.save("k", "progress");
        }
        let outbox = Outbox::open(&path).unwrap();
        let due: Vec<_> = outbox.due("r", 100).unwrap();
        assert_eq!(due.len(), 2);
        assert_eq!((due[0].task.as_str(), due[0].enqueued_ms), ("a", 10));
        outbox.failed("r", "a", "boom", 500).unwrap();
        assert_eq!(outbox.due("r", 100).unwrap().len(), 1);
        let pending = outbox.pending("r").unwrap();
        assert_eq!(pending[0].last_error.as_deref(), Some("boom"));
        // Queued again after the push started: it stays.
        outbox.done("r", "b", 14).unwrap();
        assert_eq!(outbox.pending("r").unwrap().len(), 2);
        outbox.done("r", "b", 15).unwrap();
        assert_eq!(outbox.pending("r").unwrap().len(), 1);
        let journal = OutboxJournal(Arc::new(outbox));
        assert_eq!(journal.load("k").as_deref(), Some("progress"));
        let d1 = journal.0.device(None).unwrap();
        assert_eq!(journal.0.device(None).unwrap(), d1);
        assert_eq!(journal.0.device(Some("My Box")).unwrap(), "my-box");
        journal
            .0
            .add_counters(
                "r",
                &Snapshot {
                    swaps: 2,
                    ..Snapshot::default()
                },
            )
            .unwrap();
        journal
            .0
            .add_counters(
                "r",
                &Snapshot {
                    swaps: 1,
                    bytes_up: 5,
                    ..Snapshot::default()
                },
            )
            .unwrap();
        let c = journal.0.counters("r").unwrap();
        assert_eq!((c.swaps, c.bytes_up), (3, 5));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
