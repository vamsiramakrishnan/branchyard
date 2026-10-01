//! The local-mode [`Backend`]: SQLite in WAL mode at `.branchyard/state.db`.
//!
//! Writes run in `BEGIN IMMEDIATE` transactions, so a fence check and the
//! write it guards commit together and concurrent writers in other
//! processes wait (up to [`BUSY`]) instead of failing. Records, leases,
//! steps, processes, cancels and steered input commit with
//! `synchronous=FULL`; event
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

use crate::graph::{After, Dependency, GraphBackend, GraphCommit};
use crate::state::{
    now_ms, pick_port, Acquired, Backend, Begun, FeedRow, Fence, LeaseRow, Owner, PoolBackend,
    PortBackend, ProcessRow, Record, ReservationRow, SandboxBackend, SandboxKind, SandboxRow,
    SlotRow, SlotState, SteerRow, StepRow,
};
use crate::storage::{
    ArtifactRef, ArtifactRow, Identity, LegacyBinder, LegacyBranch, LockOutcome, NewArtifact,
    NewScratch, ScratchArea, ScratchLock, ScratchRow, Share, StorageBackend,
};
use crate::{Activity, BranchStatus, Error, Message, RecordedEvent, SteerState};

/// How long a write waits for another process's transaction.
const BUSY: Duration = Duration::from_secs(30);
/// 2: grants bound to incarnations (see `crate::storage::LegacyBinder`).
const SCHEMA: i64 = 2;

const TABLES: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS branches (
    incarnation INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    created_ms INTEGER NOT NULL,
    record TEXT,
    parent_incarnation INTEGER
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
CREATE TABLE IF NOT EXISTS steers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    incarnation INTEGER NOT NULL,
    turn INTEGER NOT NULL,
    branch TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    text TEXT NOT NULL,
    at_ms INTEGER NOT NULL,
    state TEXT NOT NULL,
    reason TEXT
);
CREATE INDEX IF NOT EXISTS steers_turn ON steers (incarnation, turn, state);
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
CREATE TABLE IF NOT EXISTS artifacts (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    digest TEXT NOT NULL,
    size INTEGER NOT NULL,
    name TEXT NOT NULL,
    media_type TEXT NOT NULL,
    publisher TEXT NOT NULL,
    ancestry TEXT NOT NULL,
    turn INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    labels TEXT NOT NULL,
    publisher_incarnation INTEGER,
    ancestry_incarnations TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE IF NOT EXISTS artifact_shares (
    id TEXT NOT NULL,
    branch TEXT NOT NULL,
    incarnation INTEGER,
    PRIMARY KEY (id, branch)
);
CREATE TABLE IF NOT EXISTS scratch_areas (
    name TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    ancestry TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    owner_incarnation INTEGER,
    ancestry_incarnations TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE IF NOT EXISTS scratch_shares (
    name TEXT NOT NULL,
    branch TEXT NOT NULL,
    incarnation INTEGER,
    PRIMARY KEY (name, branch)
);
CREATE TABLE IF NOT EXISTS scratch_locks (
    name TEXT PRIMARY KEY,
    holder TEXT NOT NULL,
    acquired_ms INTEGER NOT NULL,
    holder_incarnation INTEGER
);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_branch TEXT NOT NULL,
    to_branch TEXT NOT NULL,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    in_reply_to INTEGER,
    at_ms INTEGER NOT NULL,
    delivered_ms INTEGER,
    steer_id INTEGER,
    delivered_steer INTEGER,
    awaiting_until_ms INTEGER
);
CREATE INDEX IF NOT EXISTS messages_to ON messages (to_branch, id);
CREATE INDEX IF NOT EXISTS messages_steer ON messages (steer_id);
CREATE INDEX IF NOT EXISTS messages_from ON messages (from_branch, kind);
CREATE INDEX IF NOT EXISTS messages_reply ON messages (in_reply_to);
CREATE TABLE IF NOT EXISTS graph_revisions (
    parent TEXT PRIMARY KEY,
    revision INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS graph_edges (
    parent TEXT NOT NULL,
    dependent TEXT NOT NULL,
    prerequisite TEXT NOT NULL,
    after TEXT NOT NULL,
    PRIMARY KEY (dependent, prerequisite)
);
CREATE INDEX IF NOT EXISTS graph_edges_prerequisite ON graph_edges (prerequisite);
CREATE INDEX IF NOT EXISTS graph_edges_parent ON graph_edges (parent);
CREATE TABLE IF NOT EXISTS ports (
    port INTEGER PRIMARY KEY,
    branch TEXT NOT NULL UNIQUE,
    reserved_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sandboxes (
    branch TEXT NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    incarnation INTEGER NOT NULL,
    provider TEXT NOT NULL,
    turn INTEGER,
    detail TEXT NOT NULL,
    used_ms INTEGER NOT NULL,
    PRIMARY KEY (branch, kind, name)
);
CREATE INDEX IF NOT EXISTS sandboxes_provider ON sandboxes (kind, provider, used_ms);
CREATE TABLE IF NOT EXISTS outcomes (
    id TEXT PRIMARY KEY,
    repository TEXT NOT NULL,
    branch TEXT NOT NULL,
    kind TEXT NOT NULL,
    harness TEXT NOT NULL,
    model TEXT,
    effort TEXT,
    outcome TEXT NOT NULL,
    score REAL,
    cost_usd REAL,
    duration_ms INTEGER,
    turns INTEGER NOT NULL,
    routed INTEGER NOT NULL,
    recorded_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS outcomes_kind ON outcomes (kind, recorded_ms);
CREATE TABLE IF NOT EXISTS knowledge (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope_path TEXT,
    scope_kind TEXT,
    text TEXT NOT NULL,
    source TEXT NOT NULL,
    status TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    adopted_by TEXT,
    decided_ms INTEGER,
    note TEXT
);
CREATE TABLE IF NOT EXISTS pool_slots (
    id TEXT PRIMARY KEY,
    place TEXT NOT NULL,
    recipe TEXT NOT NULL,
    state TEXT NOT NULL,
    base TEXT NOT NULL,
    path TEXT NOT NULL,
    detail TEXT NOT NULL,
    host TEXT NOT NULL,
    pid INTEGER NOT NULL,
    pid_start TEXT NOT NULL,
    branch TEXT,
    created_ms INTEGER NOT NULL,
    changed_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS pool_slots_place ON pool_slots (place, created_ms);
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
    if let Some(parent) = &record.info.parent {
        bind_parent(tx, name, parent)?;
    }
    Ok(())
}

/// Bind `name`'s parent to the incarnation of the live branch `parent`,
/// once, if it is older than `name`: the first write that names a parent is
/// the child's creation, while its parent is alive. Grants follow this
/// binding after the parent is removed (see `crate::storage::Lineage`).
fn bind_parent(tx: &Transaction<'_>, name: &str, parent: &str) -> Result<(), Error> {
    tx.execute(
        "UPDATE branches SET parent_incarnation = (SELECT p.incarnation FROM branches p \
         WHERE p.name = ?2 AND p.record IS NOT NULL AND p.incarnation < branches.incarnation) \
         WHERE name = ?1 AND parent_incarnation IS NULL",
        params![name, parent],
    )
    .map_err(|e| db("write", e))?;
    Ok(())
}

/// Upgrade a schema 1 store: add the identity columns and bind every
/// name-only grant once, by [`LegacyBinder`]'s rule, in the transaction
/// that checked the schema.
fn upgrade_to_identities(tx: &Transaction<'_>) -> Result<(), Error> {
    let e = |error| db("upgrade to schema 2", error);
    for sql in [
        "ALTER TABLE branches ADD COLUMN parent_incarnation INTEGER",
        "ALTER TABLE artifacts ADD COLUMN publisher_incarnation INTEGER",
        "ALTER TABLE artifacts ADD COLUMN ancestry_incarnations TEXT NOT NULL DEFAULT '[]'",
        "ALTER TABLE artifact_shares ADD COLUMN incarnation INTEGER",
        "ALTER TABLE scratch_areas ADD COLUMN owner_incarnation INTEGER",
        "ALTER TABLE scratch_areas ADD COLUMN ancestry_incarnations TEXT NOT NULL DEFAULT '[]'",
        "ALTER TABLE scratch_shares ADD COLUMN incarnation INTEGER",
        "ALTER TABLE scratch_locks ADD COLUMN holder_incarnation INTEGER",
    ] {
        tx.execute(sql, []).map_err(e)?;
    }
    let mut branches = Vec::new();
    {
        let mut statement = tx
            .prepare("SELECT incarnation, name, created_ms, record FROM branches WHERE record IS NOT NULL")
            .map_err(e)?;
        let rows = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(e)?;
        for row in rows {
            let (incarnation, name, created_ms, text) = row.map_err(e)?;
            let record: Record = decode(&format!("record {name}"), &text)?;
            branches.push(LegacyBranch {
                name,
                incarnation,
                created_ms: uint(created_ms),
                parent: record.info.parent,
            });
        }
    }
    let binder = LegacyBinder::new(&branches);
    for branch in &branches {
        tx.execute(
            "UPDATE branches SET parent_incarnation = ?2 WHERE incarnation = ?1",
            params![branch.incarnation, binder.parent(branch)],
        )
        .map_err(e)?;
    }
    let rows: Vec<(i64, String, String, i64)> = {
        let mut statement = tx
            .prepare("SELECT seq, publisher, ancestry, created_ms FROM artifacts")
            .map_err(e)?;
        let rows = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)?
    };
    for (seq, publisher, ancestry, created_ms) in rows {
        let ancestry: Vec<String> = decode("artifact ancestry", &ancestry)?;
        tx.execute(
            "UPDATE artifacts SET publisher_incarnation = ?2, ancestry_incarnations = ?3 \
             WHERE seq = ?1",
            params![
                seq,
                binder.bind(&publisher, Some(uint(created_ms))),
                encode("ancestry", &binder.bind_all(&ancestry, uint(created_ms)))?,
            ],
        )
        .map_err(e)?;
    }
    let rows: Vec<(String, String, String, i64)> = {
        let mut statement = tx
            .prepare("SELECT name, owner, ancestry, created_ms FROM scratch_areas")
            .map_err(e)?;
        let rows = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)?
    };
    for (name, owner, ancestry, created_ms) in rows {
        let ancestry: Vec<String> = decode("scratch ancestry", &ancestry)?;
        tx.execute(
            "UPDATE scratch_areas SET owner_incarnation = ?2, ancestry_incarnations = ?3 \
             WHERE name = ?1",
            params![
                name,
                binder.bind(&owner, Some(uint(created_ms))),
                encode("ancestry", &binder.bind_all(&ancestry, uint(created_ms)))?,
            ],
        )
        .map_err(e)?;
    }
    for (table, key) in [("artifact_shares", "id"), ("scratch_shares", "name")] {
        let rows: Vec<(String, String)> = {
            let mut statement = tx
                .prepare(&format!("SELECT {key}, branch FROM {table}"))
                .map_err(e)?;
            let rows = statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(e)?;
            rows.collect::<Result<_, _>>().map_err(e)?
        };
        for (id, branch) in rows {
            tx.execute(
                &format!("UPDATE {table} SET incarnation = ?3 WHERE {key} = ?1 AND branch = ?2"),
                params![id, branch, binder.bind(&branch, None)],
            )
            .map_err(e)?;
        }
    }
    let rows: Vec<(String, String, i64)> = {
        let mut statement = tx
            .prepare("SELECT name, holder, acquired_ms FROM scratch_locks")
            .map_err(e)?;
        let rows = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)?
    };
    for (name, holder, acquired_ms) in rows {
        tx.execute(
            "UPDATE scratch_locks SET holder_incarnation = ?2 WHERE name = ?1",
            params![name, binder.bind(&holder, Some(uint(acquired_ms)))],
        )
        .map_err(e)?;
    }
    tx.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'schema'",
        params![SCHEMA.to_string()],
    )
    .map_err(e)?;
    Ok(())
}

/// Take `record`'s lease for a new turn and write it, unless a lease on
/// this incarnation is held: then that lease.
fn grant(
    tx: &Transaction<'_>,
    record: &Record,
    incarnation: i64,
    owner: &Owner,
    ttl: Duration,
) -> Result<Result<Fence, LeaseRow>, Error> {
    let name = &record.info.name;
    let current = lease_row(tx, name)?;
    if let Some(row) = &current {
        if row.owner.is_some() && row.incarnation == incarnation {
            return Ok(Err(row.clone()));
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
    Ok(Ok(Fence {
        branch: name.clone(),
        incarnation,
        generation,
        turn: generation,
    }))
}

/// Whether a live lease on `name`'s current incarnation is held.
fn held(tx: &Transaction<'_>, name: &str) -> Result<bool, Error> {
    let incarnation = incarnation(tx, name)?;
    Ok(lease_row(tx, name)?
        .is_some_and(|row| row.owner.is_some() && Some(row.incarnation) == incarnation))
}

fn after_text(after: After) -> &'static str {
    match after {
        After::Settled => "settled",
        After::Integrated => "integrated",
    }
}

fn after_from(text: &str) -> After {
    match text {
        "integrated" => After::Integrated,
        _ => After::Settled,
    }
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

fn steer_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SteerRow> {
    Ok(SteerRow {
        id: uint(r.get(0)?),
        branch: r.get(1)?,
        turn: uint(r.get(2)?),
        by: r.get(3)?,
        text: r.get(4)?,
        requested_ms: uint(r.get(5)?),
        state: SteerState::from_columns(&r.get::<_, String>(6)?, r.get(7)?),
        message: r.get::<_, Option<i64>>(8)?.map(uint),
        message_delivered: r.get::<_, Option<bool>>(9)?.unwrap_or(false),
    })
}

/// The columns [`steer_row`] reads, from `steers`.
const STEER_COLUMNS: &str = "id, branch, turn, requested_by, text, at_ms, state, reason, \
     (SELECT m.id FROM messages m WHERE m.steer_id = steers.id), \
     (SELECT m.delivered_ms IS NOT NULL FROM messages m WHERE m.steer_id = steers.id)";

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
                Some(Ok(1)) => upgrade_to_identities(tx),
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
                "DELETE FROM steers WHERE incarnation = ?1",
                "DELETE FROM leases WHERE incarnation = ?1",
                "DELETE FROM branches WHERE incarnation = ?1",
            ] {
                tx.execute(sql, params![incarnation])
                    .map_err(|e| db("delete", e))?;
            }
            // Its own dependencies and graph go; what depends on it keeps
            // the row, and is blocked for want of it.
            for sql in [
                "DELETE FROM graph_edges WHERE dependent = ?1",
                "DELETE FROM graph_revisions WHERE parent = ?1",
                "DELETE FROM ports WHERE branch = ?1",
                "DELETE FROM sandboxes WHERE branch = ?1",
            ] {
                tx.execute(sql, params![name])
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
            match grant(tx, record, incarnation, owner, ttl)? {
                Ok(fence) => Ok(Acquired::Granted(fence)),
                Err(row) => Ok(Acquired::Held(row)),
            }
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
        self.begin_step_delivering(fence, turn, step, intent, &[])
    }

    fn begin_step_delivering(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        intent: &Value,
        deliver: &[u64],
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
                    let now = int(now_ms());
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
                            now,
                        ],
                    )
                    .map_err(|e| db("step", e))?;
                    for id in deliver {
                        tx.execute(
                            "UPDATE messages SET delivered_ms = ?2 \
                             WHERE id = ?1 AND delivered_ms IS NULL",
                            params![int(*id), now],
                        )
                        .map_err(|e| db("message", e))?;
                    }
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
        self.abandon_step_delivering(fence, turn, step, &[])
    }

    fn abandon_step_delivering(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        deliver: &[u64],
    ) -> Result<(), Error> {
        self.tx(true, |tx| {
            check(tx, fence)?;
            for id in deliver {
                tx.execute(
                    "UPDATE messages SET delivered_ms = NULL WHERE id = ?1 \
                     AND delivered_steer IS NULL AND delivered_ms = (SELECT started_ms \
                     FROM steps WHERE incarnation = ?2 AND turn = ?3 AND step = ?4 \
                     AND outcome IS NULL)",
                    params![int(*id), fence.incarnation, int(turn), step],
                )
                .map_err(|e| db("message", e))?;
            }
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

    fn request_steer(
        &self,
        name: &str,
        by: &str,
        text: &str,
        message: Option<u64>,
    ) -> Result<Option<u64>, Error> {
        self.tx(true, |tx| {
            let Some(incarnation) = incarnation(tx, name)? else {
                return Err(Error::UnknownBranch(name.to_owned()));
            };
            let lease = lease_row(tx, name)?;
            let Some(lease) = lease.filter(|l| l.owner.is_some() && l.incarnation == incarnation)
            else {
                return Ok(None);
            };
            tx.execute(
                "INSERT INTO steers (incarnation, turn, branch, requested_by, text, at_ms, state) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending')",
                params![incarnation, int(lease.turn), name, by, text, int(now_ms())],
            )
            .map_err(|e| db("steer", e))?;
            let id = tx.last_insert_rowid();
            if let Some(message) = message {
                let linked = tx
                    .execute(
                        "UPDATE messages SET steer_id = ?2 \
                         WHERE id = ?1 AND delivered_ms IS NULL",
                        params![int(message), id],
                    )
                    .map_err(|e| db("message", e))?;
                if linked == 0 {
                    // Rolls the steer back with the transaction.
                    return Err(Error::Denied(format!(
                        "message #{message} is unknown or already delivered"
                    )));
                }
            }
            Ok(Some(uint(id)))
        })
    }

    fn pending_steers(&self, fence: &Fence) -> Result<Vec<SteerRow>, Error> {
        self.query(|conn| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {STEER_COLUMNS} FROM steers \
                         WHERE incarnation = ?1 AND turn = ?2 AND state = 'pending' ORDER BY id"
                ))
                .map_err(|e| db("steers", e))?;
            let rows = statement
                .query_map(params![fence.incarnation, int(fence.turn)], steer_row)
                .map_err(|e| db("steers", e))?;
            rows.collect::<Result<_, _>>().map_err(|e| db("steers", e))
        })
    }

    fn settle_steer(
        &self,
        fence: &Fence,
        id: u64,
        state: &SteerState,
    ) -> Result<Option<u64>, Error> {
        let (name, reason) = state.columns();
        self.tx(true, |tx| {
            check(tx, fence)?;
            let settled = tx
                .execute(
                    "UPDATE steers SET state = ?4, reason = ?5 \
                     WHERE id = ?1 AND incarnation = ?2 AND turn = ?3",
                    params![int(id), fence.incarnation, int(fence.turn), name, reason],
                )
                .map_err(|e| db("steer", e))?;
            if settled == 0 {
                return Ok(None);
            }
            match state {
                SteerState::Pending => Ok(None),
                SteerState::Delivered | SteerState::Accepted => tx
                    .query_row(
                        "UPDATE messages SET delivered_ms = ?2, delivered_steer = ?1 \
                         WHERE steer_id = ?1 AND delivered_ms IS NULL RETURNING id",
                        params![int(id), int(now_ms())],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()
                    .map(|m| m.map(uint))
                    .map_err(|e| db("message", e)),
                SteerState::Refused { .. } => {
                    tx.execute(
                        "UPDATE messages SET steer_id = NULL, delivered_steer = NULL, \
                         delivered_ms = CASE WHEN delivered_steer = ?1 THEN NULL \
                         ELSE delivered_ms END WHERE steer_id = ?1",
                        params![int(id)],
                    )
                    .map_err(|e| db("message", e))?;
                    Ok(None)
                }
            }
        })
    }

    fn steer(&self, name: &str, id: u64) -> Result<Option<SteerRow>, Error> {
        self.query(|conn| {
            let Some(incarnation) = incarnation(conn, name)? else {
                return Ok(None);
            };
            conn.query_row(
                &format!("SELECT {STEER_COLUMNS} FROM steers WHERE id = ?1 AND incarnation = ?2"),
                params![int(id), incarnation],
                steer_row,
            )
            .optional()
            .map_err(|e| db("steer", e))
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

    fn message_steer(&self, id: u64) -> Result<Option<u64>, Error> {
        self.query(|conn| {
            conn.query_row(
                "SELECT steer_id FROM messages WHERE id = ?1",
                params![int(id)],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()
            .map(|s| s.flatten().map(uint))
            .map_err(|e| db("message", e))
        })
    }

    fn set_awaiting(&self, id: u64, until_ms: Option<u64>) -> Result<(), Error> {
        self.tx(true, |tx| {
            tx.execute(
                "UPDATE messages SET awaiting_until_ms = ?2 WHERE id = ?1",
                params![int(id), until_ms.map(int)],
            )
            .map_err(|e| db("message", e))?;
            Ok(())
        })
    }

    fn awaiting_answer(&self, from: &str, now_ms: u64) -> Result<bool, Error> {
        self.query(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM messages q \
                 WHERE q.from_branch = ?1 AND q.kind = 'question' \
                 AND q.awaiting_until_ms > ?2 \
                 AND NOT EXISTS (SELECT 1 FROM messages a WHERE a.in_reply_to = q.id))",
                params![from, int(now_ms)],
                |r| r.get::<_, bool>(0),
            )
            .map_err(|e| db("message", e))
        })
    }
}

/// Reads one `messages` row.
fn edges(conn: &Connection, filter: &str, value: &str) -> Result<Vec<Dependency>, Error> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT dependent, prerequisite, after FROM graph_edges WHERE {filter} = ?1 \
             ORDER BY dependent, prerequisite"
        ))
        .map_err(|e| db("dependencies", e))?;
    let rows = statement
        .query_map(params![value], |r| {
            Ok(Dependency {
                dependent: r.get(0)?,
                prerequisite: r.get(1)?,
                after: after_from(&r.get::<_, String>(2)?),
            })
        })
        .map_err(|e| db("dependencies", e))?;
    rows.collect::<Result<_, _>>()
        .map_err(|e| db("dependencies", e))
}

fn graph_revision(conn: &Connection, parent: &str) -> Result<u64, Error> {
    let revision: Option<i64> = conn
        .query_row(
            "SELECT revision FROM graph_revisions WHERE parent = ?1",
            params![parent],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| db("graph revision", e))?;
    Ok(revision.map_or(0, uint))
}

impl PortBackend for Sqlite {
    fn reserve_port(
        &self,
        branch: &str,
        start: u16,
        usable: &(dyn Fn(u16) -> bool + Sync),
    ) -> Result<u16, Error> {
        let e = |error| db("port", error);
        self.tx(true, |tx| {
            let held: Option<i64> = tx
                .query_row(
                    "SELECT port FROM ports WHERE branch = ?1",
                    params![branch],
                    |r| r.get(0),
                )
                .optional()
                .map_err(e)?;
            if let Some(port) = held {
                return Ok(port as u16);
            }
            let taken = {
                let mut statement = tx.prepare("SELECT port FROM ports").map_err(e)?;
                let rows = statement.query_map([], |r| r.get::<_, i64>(0)).map_err(e)?;
                rows.map(|r| r.map(|p| p as u16))
                    .collect::<Result<std::collections::BTreeSet<u16>, _>>()
                    .map_err(e)?
            };
            let port = pick_port(start, &taken, usable)?;
            tx.execute(
                "INSERT INTO ports (port, branch, reserved_ms) VALUES (?1, ?2, ?3)",
                params![port, branch, int(now_ms())],
            )
            .map_err(e)?;
            Ok(port)
        })
    }

    fn port(&self, branch: &str) -> Result<Option<u16>, Error> {
        self.query(|conn| {
            conn.query_row(
                "SELECT port FROM ports WHERE branch = ?1",
                params![branch],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map(|p| p.map(|p| p as u16))
            .map_err(|error| db("port", error))
        })
    }
}

const SANDBOX_COLUMNS: &str = "branch, incarnation, kind, provider, name, turn, detail, used_ms";

fn sandbox_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(SandboxRow, String)> {
    let kind: String = r.get(2)?;
    Ok((
        SandboxRow {
            branch: r.get(0)?,
            incarnation: r.get(1)?,
            kind: SandboxKind::Kept,
            provider: r.get(3)?,
            name: r.get(4)?,
            turn: r.get::<_, Option<i64>>(5)?.map(|t| t as u32),
            detail: r.get(6)?,
            used_ms: uint(r.get(7)?),
        },
        kind,
    ))
}

fn sandbox_rows(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<SandboxRow>, Error> {
    let e = |error| db("sandboxes", error);
    let mut statement = conn.prepare(sql).map_err(e)?;
    let rows = statement.query_map(args, sandbox_row).map_err(e)?;
    let mut found = Vec::new();
    for row in rows {
        let (mut row, kind) = row.map_err(e)?;
        row.kind = SandboxKind::parse(&kind)?;
        found.push(row);
    }
    Ok(found)
}

impl SandboxBackend for Sqlite {
    fn put_sandbox(&self, row: &SandboxRow) -> Result<(), Error> {
        self.tx(true, |tx| {
            tx.execute(
                "INSERT OR REPLACE INTO sandboxes \
                 (branch, incarnation, kind, provider, name, turn, detail, used_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    row.branch,
                    row.incarnation,
                    row.kind.as_str(),
                    row.provider,
                    row.name,
                    row.turn.map(i64::from),
                    row.detail,
                    int(row.used_ms)
                ],
            )
            .map_err(|e| db("sandbox", e))?;
            Ok(())
        })
    }

    fn sandboxes(&self, branch: &str) -> Result<Vec<SandboxRow>, Error> {
        self.query(|conn| {
            sandbox_rows(
                conn,
                &format!(
                    "SELECT {SANDBOX_COLUMNS} FROM sandboxes WHERE branch = ?1 \
                     ORDER BY used_ms, name"
                ),
                &[&branch],
            )
        })
    }

    fn sandboxes_of(&self, kind: SandboxKind, provider: &str) -> Result<Vec<SandboxRow>, Error> {
        self.query(|conn| {
            sandbox_rows(
                conn,
                &format!(
                    "SELECT {SANDBOX_COLUMNS} FROM sandboxes WHERE kind = ?1 AND provider = ?2 \
                     ORDER BY used_ms, branch, name"
                ),
                &[&kind.as_str(), &provider],
            )
        })
    }

    fn take_sandbox(
        &self,
        branch: &str,
        kind: SandboxKind,
        name: &str,
    ) -> Result<Option<SandboxRow>, Error> {
        self.tx(true, |tx| {
            let found = sandbox_rows(
                tx,
                &format!(
                    "SELECT {SANDBOX_COLUMNS} FROM sandboxes \
                     WHERE branch = ?1 AND kind = ?2 AND name = ?3"
                ),
                &[&branch, &kind.as_str(), &name],
            )?;
            tx.execute(
                "DELETE FROM sandboxes WHERE branch = ?1 AND kind = ?2 AND name = ?3",
                params![branch, kind.as_str(), name],
            )
            .map_err(|e| db("sandbox", e))?;
            Ok(found.into_iter().next())
        })
    }
}

impl GraphBackend for Sqlite {
    fn graph_revision(&self, parent: &str) -> Result<u64, Error> {
        self.query(|conn| graph_revision(conn, parent))
    }

    fn dependencies(&self, parent: &str) -> Result<Vec<Dependency>, Error> {
        self.query(|conn| edges(conn, "parent", parent))
    }

    fn prerequisites(&self, dependent: &str) -> Result<Vec<Dependency>, Error> {
        self.query(|conn| edges(conn, "dependent", dependent))
    }

    fn dependents(&self, prerequisite: &str) -> Result<Vec<Dependency>, Error> {
        self.query(|conn| edges(conn, "prerequisite", prerequisite))
    }

    fn commit_graph(&self, commit: &GraphCommit) -> Result<u64, Error> {
        let parent = &commit.parent;
        self.tx(true, |tx| {
            let current = graph_revision(tx, parent)?;
            if let Some(expected) = commit.expected.filter(|e| *e != current) {
                return Err(Error::StaleRevision {
                    branch: parent.clone(),
                    expected,
                    actual: current,
                });
            }
            let mut owner =
                stored_record(tx, parent)?.ok_or_else(|| Error::UnknownBranch(parent.clone()))?;
            for record in &commit.create {
                let name = &record.info.name;
                if incarnation(tx, name)?.is_some() {
                    return Err(Error::BranchExists(name.clone()));
                }
                put(tx, record)?;
                if !owner.info.children.contains(name) {
                    owner.info.children.push(name.clone());
                }
            }
            tx.execute(
                "UPDATE branches SET record = ?2 WHERE name = ?1",
                params![parent, encode(parent, &owner)?],
            )
            .map_err(|e| db("commit graph", e))?;
            let created: Vec<&String> = commit.create.iter().map(|r| &r.info.name).collect();
            let touched: std::collections::BTreeSet<&String> = commit
                .add
                .iter()
                .map(|d| &d.dependent)
                .chain(commit.remove.iter().map(|d| &d.dependent))
                .filter(|name| !created.contains(name))
                .collect();
            for name in touched {
                let mut record =
                    stored_record(tx, name)?.ok_or_else(|| Error::UnknownBranch(name.clone()))?;
                if !crate::graph::unstarted(&record.info.status) || held(tx, name)? {
                    return Err(Error::Denied(format!(
                        "{name} has already started, so its dependencies can no longer change"
                    )));
                }
                if record.info.status != BranchStatus::Waiting {
                    record.info.status = BranchStatus::Waiting;
                    put(tx, &record)?;
                    insert_event(
                        tx,
                        name,
                        &RecordedEvent {
                            at_ms: now_ms(),
                            activity: Activity::Status(BranchStatus::Waiting),
                        },
                    )?;
                }
            }
            for d in &commit.remove {
                let removed = tx
                    .execute(
                        "DELETE FROM graph_edges WHERE parent = ?1 AND dependent = ?2 \
                         AND prerequisite = ?3",
                        params![parent, d.dependent, d.prerequisite],
                    )
                    .map_err(|e| db("commit graph", e))?;
                if removed == 0 {
                    return Err(Error::Denied(format!(
                        "{} does not depend on {}",
                        d.dependent, d.prerequisite
                    )));
                }
            }
            for d in &commit.add {
                let inserted = tx
                    .execute(
                        "INSERT OR IGNORE INTO graph_edges (parent, dependent, prerequisite, \
                         after) VALUES (?1, ?2, ?3, ?4)",
                        params![parent, d.dependent, d.prerequisite, after_text(d.after)],
                    )
                    .map_err(|e| db("commit graph", e))?;
                if inserted == 0 {
                    return Err(Error::Denied(format!(
                        "{} already depends on {}",
                        d.dependent, d.prerequisite
                    )));
                }
            }
            let next = current + 1;
            tx.execute(
                "INSERT INTO graph_revisions (parent, revision) VALUES (?1, ?2) \
                 ON CONFLICT (parent) DO UPDATE SET revision = ?2",
                params![parent, int(next)],
            )
            .map_err(|e| db("commit graph", e))?;
            Ok(next)
        })
    }

    fn claim(&self, record: &Record, owner: &Owner, ttl: Duration) -> Result<Option<Fence>, Error> {
        let name = &record.info.name;
        self.tx(true, |tx| {
            let waiting = stored_record(tx, name)?
                .is_some_and(|stored| stored.info.status == BranchStatus::Waiting);
            let Some(incarnation) = incarnation(tx, name)? else {
                return Ok(None);
            };
            if !waiting {
                return Ok(None);
            }
            Ok(grant(tx, record, incarnation, owner, ttl)?.ok())
        })
    }

    fn settle_waiting(&self, record: &Record, event: &RecordedEvent) -> Result<bool, Error> {
        let name = &record.info.name;
        self.tx(true, |tx| {
            let waiting = stored_record(tx, name)?
                .is_some_and(|stored| stored.info.status == BranchStatus::Waiting);
            if !waiting || held(tx, name)? {
                return Ok(false);
            }
            put(tx, record)?;
            insert_event(tx, name, event)?;
            Ok(true)
        })
    }
}

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

/// One `artifacts` row, in `ARTIFACT_COLUMNS` order.
struct ArtifactCols {
    id: String,
    digest: String,
    size: i64,
    name: String,
    media_type: String,
    publisher: String,
    ancestry: String,
    turn: i64,
    created_ms: i64,
    labels: String,
    publisher_incarnation: Option<i64>,
    ancestry_incarnations: String,
}

fn artifact_row_from(cols: ArtifactCols) -> Result<ArtifactRow, Error> {
    Ok(ArtifactRow {
        artifact: ArtifactRef {
            id: cols.id,
            digest: cols.digest,
            size: uint(cols.size),
            name: cols.name,
            media_type: cols.media_type,
            publisher_branch: cols.publisher,
            turn: uint(cols.turn),
            created_at: uint(cols.created_ms) / 1000,
            labels: decode("artifact labels", &cols.labels)?,
        },
        ancestry: decode("artifact ancestry", &cols.ancestry)?,
        publisher_incarnation: cols.publisher_incarnation,
        ancestry_incarnations: decode("artifact ancestry", &cols.ancestry_incarnations)?,
    })
}

const ARTIFACT_COLUMNS: &str = "id, digest, size, name, media_type, publisher, ancestry, turn, \
     created_ms, labels, publisher_incarnation, ancestry_incarnations";

fn artifact_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactCols> {
    Ok(ArtifactCols {
        id: row.get(0)?,
        digest: row.get(1)?,
        size: row.get(2)?,
        name: row.get(3)?,
        media_type: row.get(4)?,
        publisher: row.get(5)?,
        ancestry: row.get(6)?,
        turn: row.get(7)?,
        created_ms: row.get(8)?,
        labels: row.get(9)?,
        publisher_incarnation: row.get(10)?,
        ancestry_incarnations: row.get(11)?,
    })
}

const SCRATCH_COLUMNS: &str =
    "name, owner, ancestry, created_ms, owner_incarnation, ancestry_incarnations";

type ScratchCols = (String, String, String, i64, Option<i64>, String);

fn scratch_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ScratchCols> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}

fn scratch_row_from(cols: ScratchCols) -> Result<ScratchRow, Error> {
    let (name, owner, ancestry, created_ms, owner_incarnation, ancestry_incarnations) = cols;
    Ok(ScratchRow {
        area: ScratchArea {
            name,
            owner_branch: owner,
            created_at: uint(created_ms) / 1000,
        },
        ancestry: decode("scratch ancestry", &ancestry)?,
        owner_incarnation,
        ancestry_incarnations: decode("scratch ancestry", &ancestry_incarnations)?,
    })
}

/// Whether the branch at `incarnation`, as stored in this transaction,
/// says `running`; a removed one does not.
fn is_running(tx: &Transaction<'_>, incarnation: i64) -> Result<bool, Error> {
    let text: Option<Option<String>> = tx
        .query_row(
            "SELECT record FROM branches WHERE incarnation = ?1",
            params![incarnation],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| db("read", e))?;
    match text.flatten() {
        Some(text) => {
            let record: Record = decode("record", &text)?;
            Ok(record.info.status == BranchStatus::Running)
        }
        None => Ok(false),
    }
}

/// The shares in `table` keyed by `key` = `value`.
fn shares(conn: &Connection, table: &str, key: &str, value: &str) -> Result<Vec<Share>, Error> {
    let mut statement = conn
        .prepare(&format!(
            "SELECT branch, incarnation FROM {table} WHERE {key} = ?1"
        ))
        .map_err(|e| db(table, e))?;
    let rows = statement
        .query_map(params![value], |r| {
            Ok(Share {
                branch: r.get(0)?,
                incarnation: r.get(1)?,
            })
        })
        .map_err(|e| db(table, e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| db(table, e))
}

impl StorageBackend for Sqlite {
    fn identities(&self) -> Result<Vec<Identity>, Error> {
        self.query(|conn| {
            let mut statement = conn
                .prepare(
                    "SELECT incarnation, name, parent_incarnation, record FROM branches \
                     WHERE record IS NOT NULL",
                )
                .map_err(|e| db("identities", e))?;
            let rows = statement
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| db("identities", e))?;
            let mut found = Vec::new();
            for row in rows {
                let (incarnation, name, parent_incarnation, text) =
                    row.map_err(|e| db("identities", e))?;
                let record: Record = decode(&format!("record {name}"), &text)?;
                found.push(Identity {
                    name,
                    incarnation,
                    parent_incarnation,
                    parent: record.info.parent,
                });
            }
            Ok(found)
        })
    }

    fn create_artifact(&self, new: &NewArtifact) -> Result<ArtifactRow, Error> {
        self.tx(true, |tx| {
            let now = int(now_ms());
            let ancestry = encode("ancestry", &new.ancestry)?;
            let ancestry_incarnations = encode("ancestry", &new.ancestry_incarnations)?;
            let labels = encode("labels", &new.labels)?;
            tx.execute(
                "INSERT INTO artifacts \
                 (id, digest, size, name, media_type, publisher, ancestry, turn, created_ms, \
                  labels, publisher_incarnation, ancestry_incarnations) \
                 VALUES ('', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    new.digest,
                    int(new.size),
                    new.name,
                    new.media_type,
                    new.publisher_branch,
                    ancestry,
                    int(new.turn),
                    now,
                    labels,
                    new.publisher_incarnation,
                    ancestry_incarnations,
                ],
            )
            .map_err(|e| db("artifact", e))?;
            let seq = tx.last_insert_rowid();
            let id = format!("art{seq}");
            tx.execute(
                "UPDATE artifacts SET id = ?1 WHERE seq = ?2",
                params![id, seq],
            )
            .map_err(|e| db("artifact", e))?;
            artifact_row_from(ArtifactCols {
                id,
                digest: new.digest.clone(),
                size: int(new.size),
                name: new.name.clone(),
                media_type: new.media_type.clone(),
                publisher: new.publisher_branch.clone(),
                ancestry,
                turn: int(new.turn),
                created_ms: now,
                labels,
                publisher_incarnation: Some(new.publisher_incarnation),
                ancestry_incarnations,
            })
        })
    }

    fn artifact(&self, id: &str) -> Result<Option<ArtifactRow>, Error> {
        self.query(|conn| {
            conn.query_row(
                &format!("SELECT {ARTIFACT_COLUMNS} FROM artifacts WHERE id = ?1"),
                params![id],
                artifact_from_row,
            )
            .optional()
            .map_err(|e| db("artifact", e))?
            .map(artifact_row_from)
            .transpose()
        })
    }

    fn artifacts(&self) -> Result<Vec<ArtifactRow>, Error> {
        self.query(|conn| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {ARTIFACT_COLUMNS} FROM artifacts ORDER BY seq"
                ))
                .map_err(|e| db("artifacts", e))?;
            let rows = statement
                .query_map([], artifact_from_row)
                .map_err(|e| db("artifacts", e))?;
            let mut found = Vec::new();
            for row in rows {
                let cols = row.map_err(|e| db("artifacts", e))?;
                found.push(artifact_row_from(cols)?);
            }
            Ok(found)
        })
    }

    fn artifact_shares(&self, id: &str) -> Result<Vec<Share>, Error> {
        self.query(|conn| shares(conn, "artifact_shares", "id", id))
    }

    fn share_artifact(&self, id: &str, branch: &str, incarnation: i64) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let exists: bool = tx
                .query_row("SELECT 1 FROM artifacts WHERE id = ?1", params![id], |_| {
                    Ok(true)
                })
                .optional()
                .map_err(|e| db("artifact", e))?
                .unwrap_or(false);
            if exists {
                tx.execute(
                    "INSERT INTO artifact_shares (id, branch, incarnation) VALUES (?1, ?2, ?3) \
                     ON CONFLICT (id, branch) DO UPDATE SET incarnation = excluded.incarnation",
                    params![id, branch, incarnation],
                )
                .map_err(|e| db("artifact share", e))?;
            }
            Ok(exists)
        })
    }

    fn delete_artifact(&self, id: &str) -> Result<(), Error> {
        self.tx(true, |tx| {
            tx.execute("DELETE FROM artifacts WHERE id = ?1", params![id])
                .map_err(|e| db("artifact", e))?;
            tx.execute("DELETE FROM artifact_shares WHERE id = ?1", params![id])
                .map_err(|e| db("artifact share", e))?;
            Ok(())
        })
    }

    fn digest_refcount(&self, digest: &str) -> Result<u64, Error> {
        self.query(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM artifacts WHERE digest = ?1",
                params![digest],
                |r| r.get::<_, i64>(0),
            )
            .map(uint)
            .map_err(|e| db("artifact", e))
        })
    }

    fn create_scratch(&self, new: &NewScratch) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let inserted = tx
                .execute(
                    "INSERT OR IGNORE INTO scratch_areas (name, owner, ancestry, created_ms, \
                     owner_incarnation, ancestry_incarnations) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        new.name,
                        new.owner,
                        encode("ancestry", &new.ancestry)?,
                        int(now_ms()),
                        new.owner_incarnation,
                        encode("ancestry", &new.ancestry_incarnations)?,
                    ],
                )
                .map_err(|e| db("scratch", e))?;
            Ok(inserted == 1)
        })
    }

    fn scratch(&self, name: &str) -> Result<Option<ScratchRow>, Error> {
        self.query(|conn| {
            conn.query_row(
                &format!("SELECT {SCRATCH_COLUMNS} FROM scratch_areas WHERE name = ?1"),
                params![name],
                scratch_from_row,
            )
            .optional()
            .map_err(|e| db("scratch", e))?
            .map(scratch_row_from)
            .transpose()
        })
    }

    fn scratch_list(&self) -> Result<Vec<ScratchRow>, Error> {
        self.query(|conn| {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {SCRATCH_COLUMNS} FROM scratch_areas ORDER BY created_ms"
                ))
                .map_err(|e| db("scratch", e))?;
            let rows = statement
                .query_map([], scratch_from_row)
                .map_err(|e| db("scratch", e))?;
            let mut found = Vec::new();
            for row in rows {
                found.push(scratch_row_from(row.map_err(|e| db("scratch", e))?)?);
            }
            Ok(found)
        })
    }

    fn scratch_shares(&self, name: &str) -> Result<Vec<Share>, Error> {
        self.query(|conn| shares(conn, "scratch_shares", "name", name))
    }

    fn share_scratch(&self, name: &str, branch: &str, incarnation: i64) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let exists: bool = tx
                .query_row(
                    "SELECT 1 FROM scratch_areas WHERE name = ?1",
                    params![name],
                    |_| Ok(true),
                )
                .optional()
                .map_err(|e| db("scratch", e))?
                .unwrap_or(false);
            if exists {
                tx.execute(
                    "INSERT INTO scratch_shares (name, branch, incarnation) VALUES (?1, ?2, ?3) \
                     ON CONFLICT (name, branch) DO UPDATE SET incarnation = excluded.incarnation",
                    params![name, branch, incarnation],
                )
                .map_err(|e| db("scratch share", e))?;
            }
            Ok(exists)
        })
    }

    fn delete_scratch(&self, name: &str) -> Result<(), Error> {
        self.tx(true, |tx| {
            tx.execute("DELETE FROM scratch_areas WHERE name = ?1", params![name])
                .map_err(|e| db("scratch", e))?;
            tx.execute("DELETE FROM scratch_shares WHERE name = ?1", params![name])
                .map_err(|e| db("scratch share", e))?;
            tx.execute("DELETE FROM scratch_locks WHERE name = ?1", params![name])
                .map_err(|e| db("scratch lock", e))?;
            Ok(())
        })
    }

    fn scratch_lock(
        &self,
        name: &str,
        branch: &str,
        incarnation: i64,
    ) -> Result<Option<LockOutcome>, Error> {
        self.tx(true, |tx| {
            let known: bool = tx
                .query_row(
                    "SELECT 1 FROM scratch_areas WHERE name = ?1",
                    params![name],
                    |_| Ok(true),
                )
                .optional()
                .map_err(|e| db("scratch", e))?
                .unwrap_or(false);
            if !known {
                return Ok(None);
            }
            let current: Option<(String, i64, Option<i64>)> = tx
                .query_row(
                    "SELECT holder, acquired_ms, holder_incarnation FROM scratch_locks \
                     WHERE name = ?1",
                    params![name],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(|e| db("scratch lock", e))?;
            let grant = |tx: &Transaction<'_>| -> Result<LockOutcome, Error> {
                let now = int(now_ms());
                tx.execute(
                    "INSERT INTO scratch_locks (name, holder, acquired_ms, holder_incarnation) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT(name) DO UPDATE SET \
                     holder = excluded.holder, acquired_ms = excluded.acquired_ms, \
                     holder_incarnation = excluded.holder_incarnation",
                    params![name, branch, now, incarnation],
                )
                .map_err(|e| db("scratch lock", e))?;
                Ok(LockOutcome::Granted(ScratchLock {
                    name: name.to_owned(),
                    holder_branch: branch.to_owned(),
                    acquired_at: uint(now) / 1000,
                }))
            };
            match current {
                None => Ok(Some(grant(tx)?)),
                Some((_, _, holder)) if holder == Some(incarnation) => Ok(Some(grant(tx)?)),
                // An unbound holder (see `LegacyBinder`) is gone.
                Some((_, _, None)) => Ok(Some(grant(tx)?)),
                Some((_, _, Some(holder))) if !is_running(tx, holder)? => Ok(Some(grant(tx)?)),
                Some((holder, acquired_ms, _)) => Ok(Some(LockOutcome::Held(ScratchLock {
                    name: name.to_owned(),
                    holder_branch: holder,
                    acquired_at: uint(acquired_ms) / 1000,
                }))),
            }
        })
    }

    fn scratch_unlock(&self, name: &str, incarnation: i64) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let changed = tx
                .execute(
                    "DELETE FROM scratch_locks WHERE name = ?1 AND holder_incarnation = ?2",
                    params![name, incarnation],
                )
                .map_err(|e| db("scratch lock", e))?;
            Ok(changed == 1)
        })
    }

    fn scratch_lock_state(&self, name: &str) -> Result<Option<ScratchLock>, Error> {
        self.query(|conn| {
            conn.query_row(
                "SELECT holder, acquired_ms FROM scratch_locks WHERE name = ?1",
                params![name],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|e| db("scratch lock", e))
            .map(|opt| {
                opt.map(|(holder, acquired_ms)| ScratchLock {
                    name: name.to_owned(),
                    holder_branch: holder,
                    acquired_at: uint(acquired_ms) / 1000,
                })
            })
        })
    }
}

const OUTCOME_COLUMNS: &str = "id, repository, branch, kind, harness, model, effort, outcome, \
     score, cost_usd, duration_ms, turns, routed, recorded_ms";

type OutcomeColumns = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<f64>,
    Option<f64>,
    Option<i64>,
    i64,
    bool,
    i64,
);

fn outcome_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<OutcomeColumns> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
        r.get(12)?,
        r.get(13)?,
    ))
}

fn outcome_record(c: OutcomeColumns) -> Result<crate::fleet::OutcomeRecord, Error> {
    Ok(crate::fleet::OutcomeRecord {
        id: c.0,
        repo: c.1,
        branch: c.2,
        kind: c
            .3
            .parse()
            .map_err(|e| Error::State(format!("outcome kind: {e}")))?,
        harness: c.4,
        model: c.5,
        effort: c.6,
        outcome: crate::fleet::BranchOutcome::parse(&c.7)?,
        score: c.8,
        cost_usd: c.9,
        duration_ms: c.10.map(uint),
        turns: u32::try_from(c.11).unwrap_or(0),
        routed: c.12,
        recorded_ms: uint(c.13),
    })
}

impl crate::fleet::OutcomeBackend for Sqlite {
    fn put_outcome(&self, row: &crate::fleet::OutcomeRecord) -> Result<(), Error> {
        // Statistics derived from branches, not a branch's state: committed
        // as event appends are, without a sync of its own.
        self.tx(false, |tx| {
            tx.execute(
                &format!(
                    "INSERT OR REPLACE INTO outcomes ({OUTCOME_COLUMNS}) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
                ),
                params![
                    row.id,
                    row.repo,
                    row.branch,
                    row.kind.as_str(),
                    row.harness,
                    row.model,
                    row.effort,
                    row.outcome.as_str(),
                    row.score,
                    row.cost_usd,
                    row.duration_ms.map(int),
                    i64::from(row.turns),
                    row.routed,
                    int(row.recorded_ms)
                ],
            )
            .map_err(|e| db("outcome", e))?;
            Ok(())
        })
    }

    fn outcome(&self, id: &str) -> Result<Option<crate::fleet::OutcomeRecord>, Error> {
        let found = self.query(|conn| {
            conn.query_row(
                &format!("SELECT {OUTCOME_COLUMNS} FROM outcomes WHERE id = ?1"),
                params![id],
                outcome_row,
            )
            .optional()
            .map_err(|e| db("outcome", e))
        })?;
        found.map(outcome_record).transpose()
    }

    fn outcomes(
        &self,
        kind: Option<crate::fleet::TaskKind>,
    ) -> Result<Vec<crate::fleet::OutcomeRecord>, Error> {
        let rows = self.query(|conn| {
            let e = |error| db("outcomes", error);
            let kind = kind.map(|k| k.as_str().to_owned());
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {OUTCOME_COLUMNS} FROM outcomes WHERE ?1 IS NULL OR kind = ?1 \
                     ORDER BY recorded_ms, id"
                ))
                .map_err(e)?;
            let rows = statement.query_map(params![kind], outcome_row).map_err(e)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(e)
        })?;
        rows.into_iter().map(outcome_record).collect()
    }
}

const KNOWLEDGE_COLUMNS: &str = "id, scope_path, scope_kind, text, source, status, created_ms, \
     adopted_by, decided_ms, note";

type KnowledgeColumns = (
    i64,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
    i64,
    Option<String>,
    Option<i64>,
    Option<String>,
);

fn knowledge_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<KnowledgeColumns> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
    ))
}

fn knowledge_entry(c: KnowledgeColumns) -> Result<crate::KnowledgeEntry, Error> {
    Ok(crate::KnowledgeEntry {
        id: uint(c.0),
        scope: crate::KnowledgeScope {
            path: c.1,
            kind: c
                .2
                .map(|k| k.parse())
                .transpose()
                .map_err(|e| Error::State(format!("knowledge kind: {e}")))?,
        },
        text: c.3,
        source: decode("knowledge source", &c.4)?,
        status: c
            .5
            .parse()
            .map_err(|e| Error::State(format!("knowledge status: {e}")))?,
        created_ms: uint(c.6),
        adopted_by: c.7,
        decided_ms: c.8.map(uint),
        note: c.9,
    })
}

impl crate::knowledge::KnowledgeBackend for Sqlite {
    fn add_knowledge(&self, entry: &crate::KnowledgeEntry) -> Result<crate::KnowledgeEntry, Error> {
        let source = encode("knowledge source", &entry.source)?;
        let created = match entry.created_ms {
            0 => now_ms(),
            at => at,
        };
        let id = self.tx(true, |tx| {
            tx.execute(
                "INSERT INTO knowledge (scope_path, scope_kind, text, source, status, created_ms, \
                 adopted_by, decided_ms, note) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    entry.scope.path,
                    entry.scope.kind.map(|k| k.as_str()),
                    entry.text,
                    source,
                    entry.status.as_str(),
                    int(created),
                    entry.adopted_by,
                    entry.decided_ms.map(int),
                    entry.note
                ],
            )
            .map_err(|e| db("knowledge", e))?;
            Ok(tx.last_insert_rowid())
        })?;
        Ok(crate::KnowledgeEntry {
            id: uint(id),
            created_ms: created,
            ..entry.clone()
        })
    }

    fn knowledge(&self, id: u64) -> Result<Option<crate::KnowledgeEntry>, Error> {
        let found = self.query(|conn| {
            conn.query_row(
                &format!("SELECT {KNOWLEDGE_COLUMNS} FROM knowledge WHERE id = ?1"),
                params![int(id)],
                knowledge_row,
            )
            .optional()
            .map_err(|e| db("knowledge", e))
        })?;
        found.map(knowledge_entry).transpose()
    }

    fn knowledge_entries(&self) -> Result<Vec<crate::KnowledgeEntry>, Error> {
        let rows = self.query(|conn| {
            let e = |error| db("knowledge", error);
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {KNOWLEDGE_COLUMNS} FROM knowledge ORDER BY id"
                ))
                .map_err(e)?;
            let rows = statement.query_map([], knowledge_row).map_err(e)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(e)
        })?;
        rows.into_iter().map(knowledge_entry).collect()
    }

    fn put_knowledge(
        &self,
        entry: &crate::KnowledgeEntry,
        expected: crate::KnowledgeStatus,
    ) -> Result<bool, Error> {
        let source = encode("knowledge source", &entry.source)?;
        self.tx(true, |tx| {
            let changed = tx
                .execute(
                    "UPDATE knowledge SET scope_path = ?2, scope_kind = ?3, text = ?4, \
                     source = ?5, status = ?6, adopted_by = ?7, decided_ms = ?8, note = ?9 \
                     WHERE id = ?1 AND status = ?10",
                    params![
                        int(entry.id),
                        entry.scope.path,
                        entry.scope.kind.map(|k| k.as_str()),
                        entry.text,
                        source,
                        entry.status.as_str(),
                        entry.adopted_by,
                        entry.decided_ms.map(int),
                        entry.note,
                        expected.as_str()
                    ],
                )
                .map_err(|e| db("knowledge", e))?;
            Ok(changed == 1)
        })
    }

    fn remove_knowledge(&self, id: u64) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let changed = tx
                .execute("DELETE FROM knowledge WHERE id = ?1", params![int(id)])
                .map_err(|e| db("knowledge", e))?;
            Ok(changed == 1)
        })
    }
}

const SLOT_COLUMNS: &str = "id, place, recipe, state, base, path, detail, host, pid, pid_start, \
     branch, created_ms, changed_ms";

type SlotColumns = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    String,
    Option<String>,
    i64,
    i64,
);

fn slot_columns(r: &rusqlite::Row<'_>) -> rusqlite::Result<SlotColumns> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
        r.get(12)?,
    ))
}

fn slot_row(c: SlotColumns) -> Result<SlotRow, Error> {
    Ok(SlotRow {
        id: c.0,
        place: c.1,
        recipe: c.2,
        state: SlotState::parse(&c.3)?,
        base: c.4,
        path: c.5,
        detail: c.6,
        host: c.7,
        pid: u32::try_from(c.8).unwrap_or(0),
        start: c.9,
        branch: c.10,
        created_ms: uint(c.11),
        changed_ms: uint(c.12),
    })
}

impl PoolBackend for Sqlite {
    fn insert_slot(&self, row: &SlotRow) -> Result<(), Error> {
        self.tx(true, |tx| {
            tx.execute(
                &format!(
                    "INSERT INTO pool_slots ({SLOT_COLUMNS}) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
                ),
                params![
                    row.id,
                    row.place,
                    row.recipe,
                    row.state.as_str(),
                    row.base,
                    row.path,
                    row.detail,
                    row.host,
                    i64::from(row.pid),
                    row.start,
                    row.branch,
                    int(row.created_ms),
                    int(row.changed_ms)
                ],
            )
            .map_err(|e| db("pool slot", e))?;
            Ok(())
        })
    }

    fn slots(&self, place: Option<&str>) -> Result<Vec<SlotRow>, Error> {
        let rows = self.query(|conn| {
            let e = |error| db("pool slots", error);
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {SLOT_COLUMNS} FROM pool_slots \
                     WHERE ?1 IS NULL OR place = ?1 ORDER BY created_ms, id"
                ))
                .map_err(e)?;
            let rows = statement
                .query_map(params![place], slot_columns)
                .map_err(e)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(e)
        })?;
        rows.into_iter().map(slot_row).collect()
    }

    fn update_slot(&self, row: &SlotRow, expected: SlotState) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let changed = tx
                .execute(
                    "UPDATE pool_slots SET state = ?2, base = ?3, path = ?4, detail = ?5, \
                     host = ?6, pid = ?7, pid_start = ?8, branch = ?9, changed_ms = ?10 \
                     WHERE id = ?1 AND state = ?11",
                    params![
                        row.id,
                        row.state.as_str(),
                        row.base,
                        row.path,
                        row.detail,
                        row.host,
                        i64::from(row.pid),
                        row.start,
                        row.branch,
                        int(row.changed_ms),
                        expected.as_str()
                    ],
                )
                .map_err(|e| db("pool slot", e))?;
            Ok(changed == 1)
        })
    }

    fn delete_slot(&self, id: &str) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let changed = tx
                .execute("DELETE FROM pool_slots WHERE id = ?1", params![id])
                .map_err(|e| db("pool slot", e))?;
            Ok(changed == 1)
        })
    }
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
