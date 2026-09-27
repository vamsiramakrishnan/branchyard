//! The local-mode [`Backend`]: SQLite in WAL mode at `.branchyard/state.db`.
//!
//! Writes run in `BEGIN IMMEDIATE` transactions, so a fence check and the
//! write it guards commit together and concurrent writers in other
//! processes wait (up to [`BUSY`]) instead of failing. Records, leases,
//! steps, processes and cancels commit with `synchronous=FULL`; event
//! appends with `synchronous=NORMAL`, which survives a process crash but
//! may lose the last appends to an operating-system crash. A later `FULL`
//! commit makes every earlier append durable too.
//!
//! On first open, branch records (`branches/*.json`) and event logs
//! (`events/*.jsonl`) written by earlier versions are imported in one
//! transaction, then moved to `legacy/`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;

use crate::state::{
    now_ms, Acquired, Backend, Begun, FeedRow, Fence, LeaseRow, Owner, ProcessRow, Record,
    ReservationRow, StepRow,
};
use crate::{Error, Message, RecordedEvent};

/// How long a write waits for another process's transaction.
const BUSY: Duration = Duration::from_secs(30);
const SCHEMA: i64 = 1;

const TABLES: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS branches (
    incarnation INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    created_ms INTEGER NOT NULL,
    record TEXT
);
CREATE TABLE IF NOT EXISTS leases (
    branch TEXT PRIMARY KEY,
    incarnation INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    turn INTEGER NOT NULL,
    owner TEXT,
    host TEXT NOT NULL,
    pid INTEGER NOT NULL,
    pid_start TEXT NOT NULL,
    acquired_ms INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL,
    deadline_ms INTEGER
);
CREATE TABLE IF NOT EXISTS steps (
    incarnation INTEGER NOT NULL,
    turn INTEGER NOT NULL,
    step TEXT NOT NULL,
    branch TEXT NOT NULL,
    generation INTEGER NOT NULL,
    intent TEXT NOT NULL,
    outcome TEXT,
    started_ms INTEGER NOT NULL,
    finished_ms INTEGER,
    PRIMARY KEY (incarnation, turn, step)
);
CREATE TABLE IF NOT EXISTS processes (
    incarnation INTEGER NOT NULL,
    turn INTEGER NOT NULL,
    pid INTEGER NOT NULL,
    branch TEXT NOT NULL,
    pgid INTEGER NOT NULL,
    start TEXT NOT NULL,
    host TEXT NOT NULL,
    generation INTEGER NOT NULL,
    recorded_ms INTEGER NOT NULL,
    PRIMARY KEY (incarnation, turn, pid)
);
CREATE TABLE IF NOT EXISTS cancels (
    incarnation INTEGER NOT NULL,
    turn INTEGER NOT NULL,
    branch TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    at_ms INTEGER NOT NULL,
    subtree INTEGER NOT NULL,
    PRIMARY KEY (incarnation, turn)
);
CREATE TABLE IF NOT EXISTS reservations (
    name TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    host TEXT NOT NULL,
    pid INTEGER NOT NULL,
    pid_start TEXT NOT NULL,
    reserved_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    branch TEXT NOT NULL,
    incarnation INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    at_ms INTEGER NOT NULL,
    activity TEXT NOT NULL,
    UNIQUE (incarnation, seq)
);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_branch TEXT NOT NULL,
    to_branch TEXT NOT NULL,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    in_reply_to INTEGER,
    at_ms INTEGER NOT NULL,
    delivered_ms INTEGER
);
CREATE INDEX IF NOT EXISTS messages_to ON messages (to_branch, id);
CREATE INDEX IF NOT EXISTS messages_reply ON messages (in_reply_to);
";

#[derive(Debug)]
pub(crate) struct Sqlite {
    path: PathBuf,
    conn: Mutex<Conn>,
}

#[derive(Debug)]
struct Conn {
    conn: Connection,
    /// Whether the connection is at `synchronous=FULL`.
    full: bool,
}

fn db(context: &str, error: rusqlite::Error) -> Error {
    Error::State(format!("{context}: {error}"))
}

fn int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn uint(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn encode<T: serde::Serialize>(what: &str, value: &T) -> Result<String, Error> {
    serde_json::to_string(value).map_err(|e| Error::State(format!("encode {what}: {e}")))
}

fn decode<T: serde::de::DeserializeOwned>(what: &str, text: &str) -> Result<T, Error> {
    serde_json::from_str(text).map_err(|e| Error::State(format!("{what}: {e}")))
}

fn fenced(fence: &Fence, why: &str) -> Error {
    Error::Fenced(format!(
        "{}'s lease generation {} {why}; another engine owns the branch now",
        fence.branch, fence.generation
    ))
}

/// Fail unless the branch's lease is held at the fence's generation.
fn check(tx: &Transaction<'_>, fence: &Fence) -> Result<(), Error> {
    let row: Option<(i64, i64, Option<String>)> = tx
        .query_row(
            "SELECT incarnation, generation, owner FROM leases WHERE branch = ?1",
            params![fence.branch],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| db("lease", e))?;
    match row {
        Some((incarnation, generation, Some(_)))
            if incarnation == fence.incarnation && uint(generation) == fence.generation =>
        {
            Ok(())
        }
        Some((_, _, None)) => Err(fenced(fence, "was released")),
        Some(_) => Err(fenced(fence, "was superseded")),
        None => Err(fenced(fence, "no longer exists")),
    }
}

fn incarnation(conn: &Connection, name: &str) -> Result<Option<i64>, Error> {
    conn.query_row(
        "SELECT incarnation FROM branches WHERE name = ?1",
        params![name],
        |r| r.get(0),
    )
    .optional()
    .map_err(|e| db("branch", e))
}

fn stored_record(conn: &Connection, name: &str) -> Result<Option<Record>, Error> {
    let text: Option<Option<String>> = conn
        .query_row(
            "SELECT record FROM branches WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| db("read", e))?;
    match text.flatten() {
        Some(text) => decode(&format!("record {name}"), &text).map(Some),
        None => Ok(None),
    }
}

/// Write `record`, keeping the stored children; inserts the branch when
/// it has no row.
fn put(tx: &Transaction<'_>, record: &Record) -> Result<(), Error> {
    let name = &record.info.name;
    let mut record = record.clone();
    if let Some(current) = stored_record(tx, name)? {
        record.info.children = current.info.children;
    }
    let text = encode(name, &record)?;
    let updated = tx
        .execute(
            "UPDATE branches SET record = ?2, created_ms = ?3 WHERE name = ?1",
            params![name, text, int(record.created_ms)],
        )
        .map_err(|e| db("write", e))?;
    if updated == 0 {
        tx.execute(
            "INSERT INTO branches (name, created_ms, record) VALUES (?1, ?2, ?3)",
            params![name, int(record.created_ms), text],
        )
        .map_err(|e| db("write", e))?;
    }
    Ok(())
}

/// Append `event` to `name`'s log; returns its sequence number.
fn insert_event(tx: &Transaction<'_>, name: &str, event: &RecordedEvent) -> Result<u64, Error> {
    let activity = encode("event", &event.activity)?;
    let incarnation =
        incarnation(tx, name)?.ok_or_else(|| Error::UnknownBranch(name.to_owned()))?;
    let seq: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE incarnation = ?1",
            params![incarnation],
            |r| r.get(0),
        )
        .map_err(|e| db("append", e))?;
    tx.execute(
        "INSERT INTO events (branch, incarnation, seq, at_ms, activity) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![name, incarnation, seq, int(event.at_ms), activity],
    )
    .map_err(|e| db("append", e))?;
    Ok(uint(seq))
}

fn lease_row(conn: &Connection, name: &str) -> Result<Option<LeaseRow>, Error> {
    conn.query_row(
        "SELECT branch, incarnation, generation, turn, owner, host, pid, pid_start, \
         expires_ms, deadline_ms FROM leases WHERE branch = ?1",
        params![name],
        lease_from,
    )
    .optional()
    .map_err(|e| db("lease", e))
}

fn lease_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<LeaseRow> {
    Ok(LeaseRow {
        branch: r.get(0)?,
        incarnation: r.get(1)?,
        generation: uint(r.get(2)?),
        turn: uint(r.get(3)?),
        owner: r.get(4)?,
        host: r.get(5)?,
        pid: u32::try_from(r.get::<_, i64>(6)?).unwrap_or(0),
        start: r.get(7)?,
        expires_ms: uint(r.get(8)?),
        deadline_ms: r.get::<_, Option<i64>>(9)?.map(uint),
    })
}

fn event_from(what: &str, at_ms: i64, activity: &str) -> Result<RecordedEvent, Error> {
    Ok(RecordedEvent {
        at_ms: uint(at_ms),
        activity: decode(what, activity)?,
    })
}

impl Sqlite {
    /// Open or create `state.db` in `dir`, and import what earlier versions
    /// left there.
    pub fn open(dir: &Path) -> Result<Sqlite, Error> {
        let path = dir.join("state.db");
        let conn = Connection::open(&path).map_err(|e| db(&path.display().to_string(), e))?;
        conn.busy_timeout(BUSY).map_err(|e| db("busy timeout", e))?;
        // Switching a new database to WAL can fail at once with SQLITE_BUSY
        // while another process switches it, without waiting in the busy
        // handler; retry within the same bound.
        let deadline = std::time::Instant::now() + BUSY;
        let mode: String = loop {
            match conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)) {
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::DatabaseBusy
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                other => break other.map_err(|e| db("journal mode", e))?,
            }
        };
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(Error::State(format!(
                "{} could not use write-ahead logging (journal mode {mode})",
                path.display()
            )));
        }
        conn.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")
            .map_err(|e| db("pragmas", e))?;
        let store = Sqlite {
            path,
            conn: Mutex::new(Conn { conn, full: true }),
        };
        store.tx(true, |tx| {
            tx.execute_batch(TABLES).map_err(|e| db("schema", e))?;
            let version: Option<String> = tx
                .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(|e| db("schema", e))?;
            match version.map(|v| v.parse::<i64>()) {
                None => {
                    tx.execute(
                        "INSERT INTO meta (key, value) VALUES ('schema', ?1)",
                        params![SCHEMA.to_string()],
                    )
                    .map_err(|e| db("schema", e))?;
                    Ok(())
                }
                Some(Ok(SCHEMA)) => Ok(()),
                Some(other) => Err(Error::State(format!(
                    "{} has schema {other:?}; this version understands {SCHEMA}",
                    store.path.display()
                ))),
            }
        })?;
        store.import_legacy(dir)?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> MutexGuard<'_, Conn> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` in an immediate transaction; `full` chooses the commit's
    /// durability.
    fn tx<T>(
        &self,
        full: bool,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut guard = self.lock();
        if guard.full != full {
            let mode = if full { "FULL" } else { "NORMAL" };
            guard
                .conn
                .execute_batch(&format!("PRAGMA synchronous = {mode}"))
                .map_err(|e| db("synchronous", e))?;
            guard.full = full;
        }
        let tx = guard
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| db("begin", e))?;
        let value = f(&tx)?;
        tx.commit().map_err(|e| db("commit", e))?;
        Ok(value)
    }

    fn query<T>(&self, f: impl FnOnce(&Connection) -> Result<T, Error>) -> Result<T, Error> {
        let guard = self.lock();
        f(&guard.conn)
    }

    /// Import `branches/*.json` and `events/*.jsonl` once, then move them
    /// to `legacy/`. Events are numbered in the feed by recording time
    /// across branches, and in each branch by line.
    fn import_legacy(&self, dir: &Path) -> Result<(), Error> {
        let branches = dir.join("branches");
        let events = dir.join("events");
        if !branches.is_dir() && !events.is_dir() {
            return Ok(());
        }
        let imported = self.tx(true, |tx| {
            let done: Option<String> = tx
                .query_row(
                    "SELECT value FROM meta WHERE key = 'legacy_imported'",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| db("import", e))?;
            if done.is_some() {
                return Ok(false);
            }
            let mut names = Vec::new();
            for (name, path) in files(&branches, ".json")? {
                let text = fs::read(&path).map_err(|e| legacy_io(&path, e))?;
                let created_ms = match text.is_empty() {
                    true => now_ms(),
                    false => {
                        let record: Record = serde_json::from_slice(&text)
                            .map_err(|e| Error::State(format!("import {}: {e}", path.display())))?;
                        record.created_ms
                    }
                };
                let record =
                    (!text.is_empty()).then(|| String::from_utf8_lossy(&text).into_owned());
                tx.execute(
                    "INSERT OR IGNORE INTO branches (name, created_ms, record) VALUES (?1, ?2, ?3)",
                    params![name, int(created_ms), record],
                )
                .map_err(|e| db("import", e))?;
                names.push(name);
            }
            let mut all = Vec::new();
            for (name, path) in files(&events, ".jsonl")? {
                let Some(incarnation) = incarnation(tx, &name)? else {
                    continue;
                };
                let text = fs::read_to_string(&path).map_err(|e| legacy_io(&path, e))?;
                // A final line without its newline was torn by a crash.
                let complete = match text.rfind('\n') {
                    Some(end) => &text[..end],
                    None => "",
                };
                let mut seq = 0;
                for (n, line) in complete.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let event: RecordedEvent = serde_json::from_str(line).map_err(|e| {
                        Error::State(format!("import {} line {}: {e}", path.display(), n + 1))
                    })?;
                    seq += 1;
                    all.push((event.at_ms, name.clone(), seq, incarnation, event));
                }
            }
            all.sort_by(|a, b| (a.0, &a.1, a.2).cmp(&(b.0, &b.1, b.2)));
            for (at_ms, name, seq, incarnation, event) in all {
                tx.execute(
                    "INSERT INTO events (branch, incarnation, seq, at_ms, activity) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        name,
                        incarnation,
                        seq,
                        int(at_ms),
                        encode("event", &event.activity)?
                    ],
                )
                .map_err(|e| db("import", e))?;
            }
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('legacy_imported', ?1)",
                params![now_ms().to_string()],
            )
            .map_err(|e| db("import", e))?;
            Ok(true)
        })?;
        if imported {
            let legacy = dir.join("legacy");
            fs::create_dir_all(&legacy).map_err(|e| legacy_io(&legacy, e))?;
            for from in [branches, events] {
                if !from.is_dir() {
                    continue;
                }
                let name = from.file_name().unwrap_or_default().to_owned();
                let mut to = legacy.join(&name);
                if to.exists() {
                    to = legacy.join(format!("{}.{}", name.to_string_lossy(), now_ms()));
                }
                fs::rename(&from, &to).map_err(|e| legacy_io(&from, e))?;
            }
        }
        Ok(())
    }
}

fn legacy_io(path: &Path, error: io::Error) -> Error {
    Error::State(format!("import {}: {error}", path.display()))
}

/// `(name, path)` of the files in `dir` ending in `suffix`, by name.
fn files(dir: &Path, suffix: &str) -> Result<Vec<(String, PathBuf)>, Error> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(legacy_io(dir, e)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| legacy_io(dir, e))?;
        let file = entry.file_name();
        let Some(name) = file.to_str().and_then(|f| f.strip_suffix(suffix)) else {
            continue;
        };
        if !name.starts_with('.') && !name.is_empty() {
            found.push((name.to_owned(), entry.path()));
        }
    }
    found.sort();
    Ok(found)
}

impl Backend for Sqlite {
    fn reserve(&self, name: &str, owner: &Owner) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let now = int(now_ms());
            let inserted = tx
                .execute(
                    "INSERT OR IGNORE INTO branches (name, created_ms, record) VALUES (?1, ?2, NULL)",
                    params![name, now],
                )
                .map_err(|e| db("reserve", e))?;
            if inserted == 1 {
                tx.execute(
                    "INSERT OR REPLACE INTO reservations (name, owner, host, pid, pid_start, \
                     reserved_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![name, owner.id, owner.host, owner.pid, owner.start, now],
                )
                .map_err(|e| db("reserve", e))?;
            }
            Ok(inserted == 1)
        })
    }

    fn release(&self, name: &str) -> Result<(), Error> {
        self.tx(true, |tx| {
            let released = tx
                .execute(
                    "DELETE FROM branches WHERE name = ?1 AND record IS NULL",
                    params![name],
                )
                .map_err(|e| db("release", e))?;
            if released == 1 {
                tx.execute("DELETE FROM reservations WHERE name = ?1", params![name])
                    .map_err(|e| db("release", e))?;
            }
            Ok(())
        })
    }

    fn reservations(&self) -> Result<Vec<ReservationRow>, Error> {
        self.query(|conn| {
            let mut statement = conn
                .prepare(
                    "SELECT r.name, r.owner, r.host, r.pid, r.pid_start, r.reserved_ms \
                     FROM reservations r JOIN branches b ON b.name = r.name \
                     WHERE b.record IS NULL ORDER BY r.name",
                )
                .map_err(|e| db("reservations", e))?;
            let rows = statement
                .query_map([], |r| {
                    Ok(ReservationRow {
                        name: r.get(0)?,
                        owner: r.get(1)?,
                        host: r.get(2)?,
                        pid: u32::try_from(r.get::<_, i64>(3)?).unwrap_or(0),
                        start: r.get(4)?,
                        reserved_ms: uint(r.get(5)?),
                    })
                })
                .map_err(|e| db("reservations", e))?;
            rows.collect::<Result<_, _>>()
                .map_err(|e| db("reservations", e))
        })
    }

    fn reclaim(&self, row: &ReservationRow) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let removed = tx
                .execute(
                    "DELETE FROM reservations WHERE name = ?1 AND owner = ?2 AND reserved_ms = ?3",
                    params![row.name, row.owner, int(row.reserved_ms)],
                )
                .map_err(|e| db("reclaim", e))?;
            if removed == 0 {
                return Ok(false);
            }
            let freed = tx
                .execute(
                    "DELETE FROM branches WHERE name = ?1 AND record IS NULL",
                    params![row.name],
                )
                .map_err(|e| db("reclaim", e))?;
            Ok(freed == 1)
        })
    }

    fn taken(&self, name: &str) -> Result<bool, Error> {
        self.query(|conn| Ok(incarnation(conn, name)?.is_some()))
    }

    fn read(&self, name: &str) -> Result<Option<Record>, Error> {
        self.query(|conn| stored_record(conn, name))
    }

    fn list(&self) -> Result<Vec<Record>, Error> {
        self.query(|conn| {
            let mut statement = conn
                .prepare(
                    "SELECT name, record FROM branches WHERE record IS NOT NULL \
                     ORDER BY created_ms, name",
                )
                .map_err(|e| db("list", e))?;
            let rows = statement
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map_err(|e| db("list", e))?;
            let mut records = Vec::new();
            for row in rows {
                let (name, text) = row.map_err(|e| db("list", e))?;
                records.push(decode(&format!("record {name}"), &text)?);
            }
            Ok(records)
        })
    }

    fn write(&self, record: &Record, fence: Option<&Fence>) -> Result<(), Error> {
        self.tx(true, |tx| {
            if let Some(fence) = fence {
                check(tx, fence)?;
            }
            put(tx, record)
        })
    }

    fn add_child(&self, parent: &str, child: &str) -> Result<(), Error> {
        self.tx(true, |tx| {
            let mut record =
                stored_record(tx, parent)?.ok_or_else(|| Error::UnknownBranch(parent.into()))?;
            if !record.info.children.iter().any(|c| c == child) {
                record.info.children.push(child.to_owned());
            }
            tx.execute(
                "UPDATE branches SET record = ?2 WHERE name = ?1",
                params![parent, encode(parent, &record)?],
            )
            .map_err(|e| db("add child", e))?;
            Ok(())
        })
    }

    fn delete(&self, name: &str) -> Result<(), Error> {
        self.tx(true, |tx| {
            let Some(incarnation) = incarnation(tx, name)? else {
                return Ok(());
            };
            tx.execute("DELETE FROM reservations WHERE name = ?1", params![name])
                .map_err(|e| db("delete", e))?;
            for sql in [
                "DELETE FROM steps WHERE incarnation = ?1",
                "DELETE FROM processes WHERE incarnation = ?1",
                "DELETE FROM cancels WHERE incarnation = ?1",
                "DELETE FROM leases WHERE incarnation = ?1",
                "DELETE FROM branches WHERE incarnation = ?1",
            ] {
                tx.execute(sql, params![incarnation])
                    .map_err(|e| db("delete", e))?;
            }
            Ok(())
        })
    }

    fn acquire(&self, record: &Record, owner: &Owner, ttl: Duration) -> Result<Acquired, Error> {
        let name = &record.info.name;
        self.tx(true, |tx| {
            let incarnation =
                incarnation(tx, name)?.ok_or_else(|| Error::UnknownBranch(name.clone()))?;
            // Creating a reserved name: only its reserving engine may, so a
            // reservation reclaimed from a live but slow engine and taken by
            // another is not created twice.
            let reserver: Option<String> = tx
                .query_row(
                    "SELECT r.owner FROM reservations r JOIN branches b ON b.name = r.name \
                     WHERE r.name = ?1 AND b.record IS NULL",
                    params![name],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| db("acquire", e))?;
            if reserver.is_some_and(|reserver| reserver != owner.id) {
                return Err(Error::BranchExists(name.clone()));
            }
            let current = lease_row(tx, name)?;
            if let Some(row) = &current {
                if row.owner.is_some() && row.incarnation == incarnation {
                    return Ok(Acquired::Held(row.clone()));
                }
            }
            let generation = current.map_or(1, |row| row.generation + 1);
            let now = now_ms();
            tx.execute(
                "INSERT INTO leases (branch, incarnation, generation, turn, owner, host, pid, \
                 pid_start, acquired_ms, expires_ms, deadline_ms) \
                 VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL) \
                 ON CONFLICT (branch) DO UPDATE SET incarnation = ?2, generation = ?3, \
                 turn = ?3, owner = ?4, host = ?5, pid = ?6, pid_start = ?7, \
                 acquired_ms = ?8, expires_ms = ?9, deadline_ms = NULL",
                params![
                    name,
                    incarnation,
                    int(generation),
                    owner.id,
                    owner.host,
                    owner.pid,
                    owner.start,
                    int(now),
                    int(now + ttl.as_millis() as u64),
                ],
            )
            .map_err(|e| db("acquire", e))?;
            put(tx, record)?;
            tx.execute("DELETE FROM reservations WHERE name = ?1", params![name])
                .map_err(|e| db("acquire", e))?;
            Ok(Acquired::Granted(Fence {
                branch: name.clone(),
                incarnation,
                generation,
                turn: generation,
            }))
        })
    }

    fn renew(&self, fence: &Fence, ttl: Duration) -> Result<(), Error> {
        self.tx(true, |tx| {
            check(tx, fence)?;
            tx.execute(
                "UPDATE leases SET expires_ms = ?2 WHERE branch = ?1",
                params![fence.branch, int(now_ms() + ttl.as_millis() as u64)],
            )
            .map_err(|e| db("renew", e))?;
            Ok(())
        })
    }

    fn finish(
        &self,
        fence: &Fence,
        record: Option<&Record>,
        event: Option<&RecordedEvent>,
    ) -> Result<(), Error> {
        self.tx(true, |tx| {
            check(tx, fence)?;
            if let Some(record) = record {
                put(tx, record)?;
            }
            if let Some(event) = event {
                insert_event(tx, &fence.branch, event)?;
            }
            tx.execute(
                "UPDATE leases SET owner = NULL, expires_ms = 0 WHERE branch = ?1",
                params![fence.branch],
            )
            .map_err(|e| db("release", e))?;
            Ok(())
        })
    }

    fn leases(&self) -> Result<Vec<LeaseRow>, Error> {
        self.query(|conn| {
            let mut statement = conn
                .prepare(
                    "SELECT branch, incarnation, generation, turn, owner, host, pid, pid_start, \
                     expires_ms, deadline_ms FROM leases WHERE owner IS NOT NULL ORDER BY branch",
                )
                .map_err(|e| db("leases", e))?;
            let rows = statement
                .query_map([], lease_from)
                .map_err(|e| db("leases", e))?;
            rows.collect::<Result<_, _>>().map_err(|e| db("leases", e))
        })
    }

    fn take_over(
        &self,
        lease: &LeaseRow,
        owner: &Owner,
        ttl: Duration,
    ) -> Result<Option<Fence>, Error> {
        self.tx(true, |tx| {
            let now = now_ms();
            let changed = tx
                .execute(
                    "UPDATE leases SET generation = generation + 1, owner = ?3, host = ?4, \
                     pid = ?5, pid_start = ?6, acquired_ms = ?7, expires_ms = ?8 \
                     WHERE branch = ?1 AND generation = ?2 AND owner IS NOT NULL",
                    params![
                        lease.branch,
                        int(lease.generation),
                        owner.id,
                        owner.host,
                        owner.pid,
                        owner.start,
                        int(now),
                        int(now + ttl.as_millis() as u64),
                    ],
                )
                .map_err(|e| db("take over", e))?;
            Ok((changed == 1).then(|| Fence {
                branch: lease.branch.clone(),
                incarnation: lease.incarnation,
                generation: lease.generation + 1,
                turn: lease.turn,
            }))
        })
    }

    fn set_deadline(&self, fence: &Fence, deadline_ms: Option<u64>) -> Result<(), Error> {
        self.tx(true, |tx| {
            check(tx, fence)?;
            tx.execute(
                "UPDATE leases SET deadline_ms = ?2 WHERE branch = ?1",
                params![fence.branch, deadline_ms.map(int)],
            )
            .map_err(|e| db("deadline", e))?;
            Ok(())
        })
    }

    fn begin_step(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        intent: &Value,
    ) -> Result<Begun, Error> {
        self.tx(true, |tx| {
            check(tx, fence)?;
            let row: Option<(String, Option<String>)> = tx
                .query_row(
                    "SELECT intent, outcome FROM steps \
                     WHERE incarnation = ?1 AND turn = ?2 AND step = ?3",
                    params![fence.incarnation, int(turn), step],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| db("step", e))?;
            match row {
                Some((_, Some(outcome))) => Ok(Begun::Done(decode(step, &outcome)?)),
                Some((intent, None)) => Ok(Begun::Pending(decode(step, &intent)?)),
                None => {
                    tx.execute(
                        "INSERT INTO steps (incarnation, turn, step, branch, generation, intent, \
                         started_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            fence.incarnation,
                            int(turn),
                            step,
                            fence.branch,
                            int(fence.generation),
                            encode(step, intent)?,
                            int(now_ms()),
                        ],
                    )
                    .map_err(|e| db("step", e))?;
                    Ok(Begun::Fresh)
                }
            }
        })
    }

    fn finish_step(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        outcome: &Value,
    ) -> Result<(), Error> {
        self.tx(true, |tx| {
            check(tx, fence)?;
            let changed = tx
                .execute(
                    "UPDATE steps SET outcome = ?4, finished_ms = ?5, generation = ?6 \
                     WHERE incarnation = ?1 AND turn = ?2 AND step = ?3",
                    params![
                        fence.incarnation,
                        int(turn),
                        step,
                        encode(step, outcome)?,
                        int(now_ms()),
                        int(fence.generation),
                    ],
                )
                .map_err(|e| db("step", e))?;
            match changed {
                1 => Ok(()),
                _ => Err(Error::State(format!(
                    "{}: step {step} of turn {turn} was never begun",
                    fence.branch
                ))),
            }
        })
    }

    fn abandon_step(&self, fence: &Fence, turn: u64, step: &str) -> Result<(), Error> {
        self.tx(true, |tx| {
            check(tx, fence)?;
            tx.execute(
                "DELETE FROM steps WHERE incarnation = ?1 AND turn = ?2 AND step = ?3 \
                 AND outcome IS NULL",
                params![fence.incarnation, int(turn), step],
            )
            .map_err(|e| db("step", e))?;
            Ok(())
        })
    }

    fn steps(&self, name: &str, turn: u64) -> Result<Vec<StepRow>, Error> {
        self.query(|conn| {
            let Some(incarnation) = incarnation(conn, name)? else {
                return Ok(Vec::new());
            };
            let mut statement = conn
                .prepare(
                    "SELECT step, intent, outcome FROM steps WHERE incarnation = ?1 AND turn = ?2 \
                     ORDER BY started_ms, step",
                )
                .map_err(|e| db("steps", e))?;
            let rows = statement
                .query_map(params![incarnation, int(turn)], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })
                .map_err(|e| db("steps", e))?;
            let mut steps = Vec::new();
            for row in rows {
                let (step, intent, outcome) = row.map_err(|e| db("steps", e))?;
                steps.push(StepRow {
                    intent: decode(&step, &intent)?,
                    outcome: outcome.map(|o| decode(&step, &o)).transpose()?,
                    step,
                });
            }
            Ok(steps)
        })
    }

    fn record_process(&self, fence: &Fence, process: &ProcessRow) -> Result<(), Error> {
        self.tx(true, |tx| {
            check(tx, fence)?;
            tx.execute(
                "INSERT OR REPLACE INTO processes (incarnation, turn, pid, branch, pgid, start, \
                 host, generation, recorded_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    fence.incarnation,
                    int(fence.turn),
                    process.pid,
                    fence.branch,
                    process.pgid,
                    process.start,
                    process.host,
                    int(fence.generation),
                    int(now_ms()),
                ],
            )
            .map_err(|e| db("process", e))?;
            Ok(())
        })
    }

    fn processes(&self, name: &str, turn: u64) -> Result<Vec<ProcessRow>, Error> {
        self.query(|conn| {
            let Some(incarnation) = incarnation(conn, name)? else {
                return Ok(Vec::new());
            };
            let mut statement = conn
                .prepare(
                    "SELECT pid, pgid, start, host FROM processes \
                     WHERE incarnation = ?1 AND turn = ?2 ORDER BY recorded_ms",
                )
                .map_err(|e| db("processes", e))?;
            let rows = statement
                .query_map(params![incarnation, int(turn)], |r| {
                    Ok(ProcessRow {
                        pid: u32::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                        pgid: u32::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                        start: r.get(2)?,
                        host: r.get(3)?,
                    })
                })
                .map_err(|e| db("processes", e))?;
            rows.collect::<Result<_, _>>()
                .map_err(|e| db("processes", e))
        })
    }

    fn request_cancel(&self, name: &str, by: &str, subtree: bool) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let Some(incarnation) = incarnation(tx, name)? else {
                return Err(Error::UnknownBranch(name.to_owned()));
            };
            let lease = lease_row(tx, name)?;
            let Some(lease) = lease.filter(|l| l.owner.is_some() && l.incarnation == incarnation)
            else {
                return Ok(false);
            };
            tx.execute(
                "INSERT OR IGNORE INTO cancels (incarnation, turn, branch, requested_by, at_ms, \
                 subtree) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    incarnation,
                    int(lease.turn),
                    name,
                    by,
                    int(now_ms()),
                    subtree
                ],
            )
            .map_err(|e| db("cancel", e))?;
            Ok(true)
        })
    }

    fn cancel_requested(&self, fence: &Fence) -> Result<Option<String>, Error> {
        self.query(|conn| {
            conn.query_row(
                "SELECT requested_by FROM cancels WHERE incarnation = ?1 AND turn = ?2",
                params![fence.incarnation, int(fence.turn)],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| db("cancel", e))
        })
    }

    fn append(
        &self,
        name: &str,
        event: &RecordedEvent,
        fence: Option<&Fence>,
    ) -> Result<u64, Error> {
        self.tx(false, |tx| {
            if let Some(fence) = fence {
                check(tx, fence)?;
            }
            insert_event(tx, name, event)
        })
    }

    fn events_since(
        &self,
        name: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<(u64, RecordedEvent)>, Error> {
        self.query(|conn| {
            let incarnation =
                incarnation(conn, name)?.ok_or_else(|| Error::UnknownBranch(name.to_owned()))?;
            let mut statement = conn
                .prepare(
                    "SELECT seq, at_ms, activity FROM events WHERE incarnation = ?1 AND seq > ?2 \
                     ORDER BY seq LIMIT ?3",
                )
                .map_err(|e| db("events", e))?;
            let rows = statement
                .query_map(params![incarnation, int(after), int(limit as u64)], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| db("events", e))?;
            let mut events = Vec::new();
            for row in rows {
                let (seq, at_ms, activity) = row.map_err(|e| db("events", e))?;
                let what = format!("event {seq} of {name}");
                events.push((uint(seq), event_from(&what, at_ms, &activity)?));
            }
            Ok(events)
        })
    }

    fn event_count(&self, name: &str) -> Result<u64, Error> {
        self.query(|conn| {
            let incarnation =
                incarnation(conn, name)?.ok_or_else(|| Error::UnknownBranch(name.to_owned()))?;
            conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE incarnation = ?1",
                params![incarnation],
                |r| r.get::<_, i64>(0),
            )
            .map(uint)
            .map_err(|e| db("events", e))
        })
    }

    fn feed_since(&self, after: u64, limit: usize) -> Result<Vec<FeedRow>, Error> {
        self.query(|conn| {
            let mut statement = conn
                .prepare(
                    "SELECT id, branch, at_ms, activity FROM events WHERE id > ?1 \
                     ORDER BY id LIMIT ?2",
                )
                .map_err(|e| db("feed", e))?;
            let rows = statement
                .query_map(params![int(after), int(limit as u64)], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| db("feed", e))?;
            let mut feed = Vec::new();
            for row in rows {
                let (id, branch, at_ms, activity) = row.map_err(|e| db("feed", e))?;
                feed.push(FeedRow {
                    id: uint(id),
                    event: event_from(&format!("feed entry {id}"), at_ms, &activity)?,
                    branch,
                });
            }
            Ok(feed)
        })
    }

    fn head(&self) -> Result<u64, Error> {
        self.query(|conn| {
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(uint)
            .map_err(|e| db("feed", e))
        })
    }

    fn send_message(&self, message: &Message) -> Result<Message, Error> {
        self.tx(true, |tx| {
            let at_ms = now_ms();
            tx.execute(
                "INSERT INTO messages \
                 (from_branch, to_branch, kind, text, in_reply_to, at_ms, delivered_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                params![
                    message.from,
                    message.to,
                    message.kind.as_str(),
                    message.text,
                    message.in_reply_to.map(int),
                    int(at_ms),
                ],
            )
            .map_err(|e| db("message", e))?;
            Ok(Message {
                id: uint(tx.last_insert_rowid()),
                at_ms,
                delivered: false,
                ..message.clone()
            })
        })
    }

    fn message(&self, id: u64) -> Result<Option<Message>, Error> {
        self.query(|conn| {
            conn.query_row(
                "SELECT id, from_branch, to_branch, kind, text, in_reply_to, at_ms, \
                 delivered_ms FROM messages WHERE id = ?1",
                params![int(id)],
                message_from,
            )
            .optional()
            .map_err(|e| db("message", e))
        })
    }

    fn inbox(&self, to: &str) -> Result<Vec<Message>, Error> {
        self.query(|conn| {
            let mut statement = conn
                .prepare(
                    "SELECT id, from_branch, to_branch, kind, text, in_reply_to, at_ms, \
                     delivered_ms FROM messages WHERE to_branch = ?1 ORDER BY id",
                )
                .map_err(|e| db("inbox", e))?;
            let rows = statement
                .query_map(params![to], message_from)
                .map_err(|e| db("inbox", e))?;
            rows.collect::<Result<_, _>>().map_err(|e| db("inbox", e))
        })
    }

    fn mark_delivered(&self, ids: &[u64]) -> Result<(), Error> {
        if ids.is_empty() {
            return Ok(());
        }
        self.tx(true, |tx| {
            let now = int(now_ms());
            for id in ids {
                tx.execute(
                    "UPDATE messages SET delivered_ms = ?2 \
                     WHERE id = ?1 AND delivered_ms IS NULL",
                    params![int(*id), now],
                )
                .map_err(|e| db("message", e))?;
            }
            Ok(())
        })
    }

    fn answer_to(&self, question_id: u64) -> Result<Option<Message>, Error> {
        self.query(|conn| {
            conn.query_row(
                "SELECT id, from_branch, to_branch, kind, text, in_reply_to, at_ms, \
                 delivered_ms FROM messages WHERE in_reply_to = ?1 ORDER BY id LIMIT 1",
                params![int(question_id)],
                message_from,
            )
            .optional()
            .map_err(|e| db("message", e))
        })
    }
}

/// Reads one `messages` row.
fn message_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let kind: String = r.get(3)?;
    let kind = kind.parse::<crate::MessageKind>().map_err(|_| {
        rusqlite::Error::InvalidColumnType(3, "kind".into(), rusqlite::types::Type::Text)
    })?;
    Ok(Message {
        id: uint(r.get::<_, i64>(0)?),
        from: r.get(1)?,
        to: r.get(2)?,
        kind,
        text: r.get(4)?,
        in_reply_to: r.get::<_, Option<i64>>(5)?.map(uint),
        at_ms: uint(r.get(6)?),
        delivered: r.get::<_, Option<i64>>(7)?.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Activity, BranchStatus};

    struct Temp(PathBuf);

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn processes_opening_a_new_store_at_once_all_succeed() {
        for round in 0..20 {
            let dir = std::env::temp_dir().join(format!(
                "branchyard-sqlite-{}-race-{round}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let _temp = Temp(dir.clone());
            let openers: Vec<_> = (0..4)
                .map(|_| {
                    let dir = dir.clone();
                    std::thread::spawn(move || Sqlite::open(&dir).map(|_| ()))
                })
                .collect();
            for opener in openers {
                opener.join().unwrap().unwrap();
            }
        }
    }

    fn open(name: &str) -> (Temp, Sqlite) {
        let dir =
            std::env::temp_dir().join(format!("branchyard-sqlite-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = Sqlite::open(&dir).unwrap();
        (Temp(dir), store)
    }

    fn record(name: &str) -> Record {
        serde_json::from_value(serde_json::json!({
            "info": {
                "name": name, "git_branch": format!("by/{name}"), "worktree": "/w",
                "prompt": "p", "harness": "h", "profile": "p", "session": null,
                "parent": null, "base": "b", "candidate": null,
                "status": {"state": "running"}, "turns": 0, "cost_usd": null,
                "created_at": 0
            },
            "created_ms": 0, "check": null, "command": null, "home": null,
            "cost_baseline": null
        }))
        .unwrap()
    }

    fn owner(id: &str) -> Owner {
        Owner {
            id: id.into(),
            host: crate::proc::host().into(),
            pid: std::process::id(),
            start: crate::proc::own_start().into(),
        }
    }

    fn event() -> RecordedEvent {
        RecordedEvent {
            at_ms: 1,
            activity: Activity::Status(BranchStatus::Running),
        }
    }

    fn granted(acquired: Acquired) -> Fence {
        match acquired {
            Acquired::Granted(fence) => fence,
            Acquired::Held(row) => panic!("held by {row:?}"),
        }
    }

    const TTL: Duration = Duration::from_secs(30);

    #[test]
    fn a_stale_owners_writes_are_fenced_once_the_lease_is_taken_over() {
        let (_temp, store) = open("fence");
        assert!(store.reserve("b", &owner("a")).unwrap());
        assert!(!store.reserve("b", &owner("a")).unwrap());
        let first = granted(store.acquire(&record("b"), &owner("a"), TTL).unwrap());
        assert_eq!((first.generation, first.turn), (1, 1));
        // A second engine is refused while the first holds the lease.
        let Acquired::Held(row) = store.acquire(&record("b"), &owner("b"), TTL).unwrap() else {
            panic!("a held lease was granted twice");
        };
        assert_eq!(row.owner.as_deref(), Some("a"));
        assert_eq!(row.stale(now_ms()), None, "a live owner in this process");
        store.append("b", &event(), Some(&first)).unwrap();

        // Recovery takes it over: same turn, next generation.
        let second = store.take_over(&row, &owner("b"), TTL).unwrap().unwrap();
        assert_eq!((second.generation, second.turn), (2, 1));
        assert_eq!(store.take_over(&row, &owner("c"), TTL).unwrap(), None);
        for refused in [
            store.write(&record("b"), Some(&first)),
            store.append("b", &event(), Some(&first)).map(|_| ()),
            store.renew(&first, TTL),
            store.set_deadline(&first, Some(1)),
            store.finish(&first, Some(&record("b")), None),
        ] {
            assert!(
                matches!(&refused, Err(Error::Fenced(why)) if why.contains("superseded")),
                "{refused:?}"
            );
        }
        assert_eq!(store.append("b", &event(), Some(&second)).unwrap(), 2);
        store
            .finish(&second, Some(&record("b")), Some(&event()))
            .unwrap();
        assert!(matches!(
            store.renew(&second, TTL),
            Err(Error::Fenced(why)) if why.contains("released")
        ));
        assert!(store.leases().unwrap().is_empty());
        assert_eq!(store.event_count("b").unwrap(), 3);
    }

    #[test]
    fn an_expired_lease_is_stale_and_a_new_turn_gets_the_next_generation() {
        let (_temp, store) = open("expiry");
        store.reserve("b", &owner("a")).unwrap();
        let fence = granted(
            store
                .acquire(&record("b"), &owner("a"), Duration::ZERO)
                .unwrap(),
        );
        let row = store.leases().unwrap().remove(0);
        assert!(row.stale(now_ms()).unwrap().contains("expired"));
        store.finish(&fence, None, None).unwrap();
        let next = granted(store.acquire(&record("b"), &owner("b"), TTL).unwrap());
        assert_eq!((next.generation, next.turn), (2, 2));
    }

    #[test]
    fn a_step_records_its_intent_once_and_replays_its_outcome() {
        let (_temp, store) = open("steps");
        store.reserve("b", &owner("a")).unwrap();
        let fence = granted(store.acquire(&record("b"), &owner("a"), TTL).unwrap());
        let intent = serde_json::json!({ "prompt": "p" });
        assert_eq!(
            store.begin_step(&fence, 1, "submit", &intent).unwrap(),
            Begun::Fresh
        );
        assert_eq!(
            store.begin_step(&fence, 1, "submit", &intent).unwrap(),
            Begun::Pending(intent.clone())
        );
        let outcome = serde_json::json!({ "turn": 1 });
        store.finish_step(&fence, 1, "submit", &outcome).unwrap();
        assert_eq!(
            store.begin_step(&fence, 1, "submit", &intent).unwrap(),
            Begun::Done(outcome.clone())
        );
        // A completed step is not forgotten; a pending one is.
        store.abandon_step(&fence, 1, "submit").unwrap();
        store.begin_step(&fence, 1, "merge", &intent).unwrap();
        store.abandon_step(&fence, 1, "merge").unwrap();
        let steps = store.steps("b", 1).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].outcome, Some(outcome));
        assert!(store.finish_step(&fence, 2, "submit", &intent).is_err());
    }

    #[test]
    fn a_cancel_is_bound_to_the_turn_it_was_asked_of() {
        let (_temp, store) = open("cancel");
        store.reserve("b", &owner("a")).unwrap();
        assert!(matches!(
            store.request_cancel("nope", "x", false),
            Err(Error::UnknownBranch(_))
        ));
        assert!(!store.request_cancel("b", "x", false).unwrap(), "no turn");
        let fence = granted(store.acquire(&record("b"), &owner("a"), TTL).unwrap());
        assert!(store.request_cancel("b", "first", true).unwrap());
        assert!(store.request_cancel("b", "second", false).unwrap());
        assert_eq!(
            store.cancel_requested(&fence).unwrap().as_deref(),
            Some("first")
        );
        store.finish(&fence, None, None).unwrap();
        let next = granted(store.acquire(&record("b"), &owner("a"), TTL).unwrap());
        assert_eq!(store.cancel_requested(&next).unwrap(), None);
    }

    #[test]
    fn a_reservation_is_reclaimed_only_from_a_gone_engine_or_once_expired() {
        let (_temp, store) = open("reservations");
        let mut exited = std::process::Command::new("true").spawn().unwrap();
        let dead = Owner {
            id: "dead".into(),
            pid: exited.id(),
            start: crate::proc::start_time(exited.id()).unwrap_or_else(|| "1".into()),
            host: crate::proc::host().into(),
        };
        exited.wait().unwrap();
        assert!(store.reserve("gone", &dead).unwrap());
        assert!(store.reserve("live", &owner("a")).unwrap());
        let elsewhere = Owner {
            host: "another-host/boot".into(),
            ..owner("far")
        };
        assert!(store.reserve("far", &elsewhere).unwrap());
        let rows = store.reservations().unwrap();
        let row = |name: &str| rows.iter().find(|r| r.name == name).unwrap().clone();
        let now = now_ms();
        assert!(row("gone")
            .stale(now)
            .unwrap()
            .contains("no longer running"));
        assert_eq!(row("live").stale(now), None);
        assert_eq!(row("far").stale(now), None, "another host's engine");
        let later = now + crate::state::RESERVATION_TTL.as_millis() as u64;
        assert!(row("far").stale(later).unwrap().contains("never created"));

        // Reclaiming frees the name once; a reservation made again since is
        // a different one and is kept.
        assert!(store.reclaim(&row("gone")).unwrap());
        assert!(!store.taken("gone").unwrap());
        assert!(!store.reclaim(&row("gone")).unwrap());
        assert!(store.reserve("gone", &owner("b")).unwrap());
        assert!(!store.reclaim(&row("gone")).unwrap());
        assert!(store.taken("gone").unwrap());

        // Only the reserving engine creates the branch; creating it ends
        // the reservation.
        assert!(matches!(
            store.acquire(&record("gone"), &owner("a"), TTL),
            Err(Error::BranchExists(_))
        ));
        let fence = granted(store.acquire(&record("gone"), &owner("b"), TTL).unwrap());
        store.finish(&fence, None, None).unwrap();
        let names: Vec<String> = store
            .reservations()
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["far", "live"]);
    }
}
