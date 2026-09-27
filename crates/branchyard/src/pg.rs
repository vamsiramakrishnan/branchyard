//! The PostgreSQL [`Backend`], behind the `postgres` feature: the same
//! operations and guarantees as [`crate::sqlite`], for a repository whose
//! state a server keeps in a database.
//!
//! Every repository's rows carry its scope (the name the server serves it
//! under), so one database, or one schema, holds several repositories.
//! Tables are created in the connection's `search_path` schema.
//!
//! Writes run in `SERIALIZABLE` transactions and are retried from the
//! start when PostgreSQL reports a serialization failure or a deadlock, so
//! they behave as SQLite's serialized `BEGIN IMMEDIATE` transactions do: a
//! fence check and the write it guards commit together, and two engines
//! never both take a lease. Records, leases, steps, processes, cancels and
//! steered input commit with `synchronous_commit = on`; event appends with `off`, which
//! survives a crash of this process but may lose the last appends to a
//! database crash, as SQLite's `synchronous=NORMAL` does. A later
//! synchronous commit makes every earlier one durable too.
//!
//! Feed positions come from a per-repository counter row that each append
//! updates, so appends to one repository serialize on it and positions are
//! assigned in commit order: a reader that sees position N sees every
//! position before it. Readers wait by polling, as with SQLite across
//! processes; nothing listens for `NOTIFY`.
//!
//! Lease expiry compares times from the engines' own clocks, as with
//! SQLite. Engines on several hosts need synchronized clocks.

use std::fmt;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use postgres::error::SqlState;
use postgres::{Client, NoTls, Row, Transaction};
use serde_json::Value;

use crate::state::{
    now_ms, Acquired, Backend, Begun, FeedRow, Fence, LeaseRow, Owner, ProcessRow, Record,
    ReservationRow, SteerRow, StepRow,
};
use crate::storage::{
    ArtifactRef, ArtifactRow, LockOutcome, NewArtifact, ScratchArea, ScratchLock, ScratchRow,
    StorageBackend,
};
use crate::{BranchStatus, Error, RecordedEvent, SteerState};

const SCHEMA: i64 = 1;
/// How long a write keeps retrying serialization failures.
const RETRY_FOR: Duration = Duration::from_secs(30);

const TABLES: &str = "
CREATE TABLE IF NOT EXISTS by_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS by_branches (
    incarnation BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo TEXT NOT NULL,
    name TEXT NOT NULL,
    created_ms BIGINT NOT NULL,
    record TEXT,
    UNIQUE (repo, name)
);
CREATE TABLE IF NOT EXISTS by_leases (
    repo TEXT NOT NULL,
    branch TEXT NOT NULL,
    incarnation BIGINT NOT NULL,
    generation BIGINT NOT NULL,
    turn BIGINT NOT NULL,
    owner TEXT,
    host TEXT NOT NULL,
    pid BIGINT NOT NULL,
    pid_start TEXT NOT NULL,
    acquired_ms BIGINT NOT NULL,
    expires_ms BIGINT NOT NULL,
    deadline_ms BIGINT,
    PRIMARY KEY (repo, branch)
);
CREATE TABLE IF NOT EXISTS by_steps (
    incarnation BIGINT NOT NULL,
    turn BIGINT NOT NULL,
    step TEXT NOT NULL,
    branch TEXT NOT NULL,
    generation BIGINT NOT NULL,
    intent TEXT NOT NULL,
    outcome TEXT,
    started_ms BIGINT NOT NULL,
    finished_ms BIGINT,
    PRIMARY KEY (incarnation, turn, step)
);
CREATE TABLE IF NOT EXISTS by_processes (
    incarnation BIGINT NOT NULL,
    turn BIGINT NOT NULL,
    pid BIGINT NOT NULL,
    branch TEXT NOT NULL,
    pgid BIGINT NOT NULL,
    start TEXT NOT NULL,
    host TEXT NOT NULL,
    generation BIGINT NOT NULL,
    recorded_ms BIGINT NOT NULL,
    PRIMARY KEY (incarnation, turn, pid)
);
CREATE TABLE IF NOT EXISTS by_cancels (
    incarnation BIGINT NOT NULL,
    turn BIGINT NOT NULL,
    branch TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    at_ms BIGINT NOT NULL,
    subtree BOOLEAN NOT NULL,
    PRIMARY KEY (incarnation, turn)
);
CREATE TABLE IF NOT EXISTS by_steers (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    incarnation BIGINT NOT NULL,
    turn BIGINT NOT NULL,
    branch TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    text TEXT NOT NULL,
    at_ms BIGINT NOT NULL,
    state TEXT NOT NULL,
    reason TEXT
);
CREATE INDEX IF NOT EXISTS by_steers_turn ON by_steers (incarnation, turn, state);
CREATE TABLE IF NOT EXISTS by_reservations (
    repo TEXT NOT NULL,
    name TEXT NOT NULL,
    owner TEXT NOT NULL,
    host TEXT NOT NULL,
    pid BIGINT NOT NULL,
    pid_start TEXT NOT NULL,
    reserved_ms BIGINT NOT NULL,
    PRIMARY KEY (repo, name)
);
CREATE TABLE IF NOT EXISTS by_feed_heads (
    repo TEXT PRIMARY KEY,
    head BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS by_events (
    repo TEXT NOT NULL,
    id BIGINT NOT NULL,
    branch TEXT NOT NULL,
    incarnation BIGINT NOT NULL,
    seq BIGINT NOT NULL,
    at_ms BIGINT NOT NULL,
    activity TEXT NOT NULL,
    PRIMARY KEY (repo, id),
    UNIQUE (incarnation, seq)
);
CREATE TABLE IF NOT EXISTS by_artifacts (
    seq BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo TEXT NOT NULL,
    id TEXT NOT NULL,
    digest TEXT NOT NULL,
    size BIGINT NOT NULL,
    name TEXT NOT NULL,
    media_type TEXT NOT NULL,
    publisher TEXT NOT NULL,
    ancestry TEXT NOT NULL,
    turn BIGINT NOT NULL,
    created_ms BIGINT NOT NULL,
    labels TEXT NOT NULL,
    UNIQUE (repo, id)
);
CREATE TABLE IF NOT EXISTS by_artifact_shares (
    repo TEXT NOT NULL,
    id TEXT NOT NULL,
    branch TEXT NOT NULL,
    PRIMARY KEY (repo, id, branch)
);
CREATE TABLE IF NOT EXISTS by_scratch_areas (
    repo TEXT NOT NULL,
    name TEXT NOT NULL,
    owner TEXT NOT NULL,
    ancestry TEXT NOT NULL,
    created_ms BIGINT NOT NULL,
    PRIMARY KEY (repo, name)
);
CREATE TABLE IF NOT EXISTS by_scratch_shares (
    repo TEXT NOT NULL,
    name TEXT NOT NULL,
    branch TEXT NOT NULL,
    PRIMARY KEY (repo, name, branch)
);
CREATE TABLE IF NOT EXISTS by_scratch_locks (
    repo TEXT NOT NULL,
    name TEXT NOT NULL,
    holder TEXT NOT NULL,
    acquired_ms BIGINT NOT NULL,
    PRIMARY KEY (repo, name)
);
";

fn steer_row(r: &Row) -> SteerRow {
    SteerRow {
        id: uint(r.get::<_, i64>(0)),
        branch: r.get(1),
        turn: uint(r.get::<_, i64>(2)),
        by: r.get(3),
        text: r.get(4),
        requested_ms: uint(r.get::<_, i64>(5)),
        state: SteerState::from_columns(&r.get::<_, String>(6), r.get(7)),
    }
}

/// Branch state for one repository scope in a PostgreSQL database.
pub(crate) struct Postgres {
    url: String,
    repo: String,
    conn: Mutex<Option<Client>>,
}

impl fmt::Debug for Postgres {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Postgres")
            .field("url", &redact(&self.url))
            .field("repo", &self.repo)
            .finish()
    }
}

/// `url` without a password.
pub(crate) fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    match rest.split_once('@') {
        Some((user, host)) => match user.split_once(':') {
            Some((name, _)) => format!("{scheme}://{name}:***@{host}"),
            None => url.to_owned(),
        },
        None => url.to_owned(),
    }
}

/// Why a transaction's work stopped: a conflict to retry, or a failure.
enum Fail {
    Retry(postgres::Error),
    Error(Error),
}

impl From<Error> for Fail {
    fn from(error: Error) -> Self {
        Fail::Error(error)
    }
}

type R<T> = Result<T, Fail>;

/// A database error in `context`, retried when it is a serialization
/// failure or a deadlock.
fn db(context: &'static str) -> impl FnOnce(postgres::Error) -> Fail {
    move |error| match error.code() {
        Some(code) if *code == SqlState::T_R_SERIALIZATION_FAILURE => Fail::Retry(error),
        Some(code) if *code == SqlState::T_R_DEADLOCK_DETECTED => Fail::Retry(error),
        _ => Fail::Error(state(context, &error)),
    }
}

fn state(context: &str, error: &postgres::Error) -> Error {
    match error.as_db_error() {
        Some(db) => Error::State(format!("{context}: {}", db.message())),
        None => Error::State(format!("{context}: {error}")),
    }
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

/// Run `f`, on another thread when this one is inside a Tokio runtime,
/// where the synchronous client cannot block.
fn off_runtime<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(_) => std::thread::scope(|scope| {
            scope
                .spawn(f)
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        }),
        Err(_) => f(),
    }
}

/// Drop `client`, which blocks to close its connection, off any Tokio
/// runtime.
pub(crate) fn close(client: Client) {
    off_runtime(move || drop(client));
}

impl Drop for Postgres {
    fn drop(&mut self) {
        let conn = self.conn.get_mut().unwrap_or_else(|e| e.into_inner());
        if let Some(client) = conn.take() {
            close(client);
        }
    }
}

/// Connect to `url`. Only `sslmode=disable` or `prefer` work: TLS to the
/// database is not built in.
pub(crate) fn connect(url: &str) -> Result<Client, Error> {
    off_runtime(|| Client::connect(url, NoTls))
        .map_err(|e| state(&format!("connect to {}", redact(url)), &e))
}

impl Postgres {
    /// Connect, create the tables if they are missing, and check the
    /// schema version.
    pub fn open(url: &str, repo: &str) -> Result<Postgres, Error> {
        let store = Postgres {
            url: url.to_owned(),
            repo: repo.to_owned(),
            conn: Mutex::new(Some(connect(url)?)),
        };
        store.tx(true, |tx| {
            // Concurrent CREATE TABLE IF NOT EXISTS can collide; one at a
            // time.
            tx.execute("SELECT pg_advisory_xact_lock(7390184325)", &[])
                .map_err(db("schema"))?;
            tx.batch_execute(TABLES).map_err(db("schema"))?;
            let version: Option<String> = tx
                .query_opt("SELECT value FROM by_meta WHERE key = 'schema'", &[])
                .map_err(db("schema"))?
                .map(|r| r.get(0));
            match version.map(|v| v.parse::<i64>()) {
                None => {
                    tx.execute(
                        "INSERT INTO by_meta (key, value) VALUES ('schema', $1)",
                        &[&SCHEMA.to_string()],
                    )
                    .map_err(db("schema"))?;
                    Ok(())
                }
                Some(Ok(SCHEMA)) => Ok(()),
                Some(other) => Err(Fail::Error(Error::State(format!(
                    "the database has Branchyard schema {other:?}; this version understands \
                     {SCHEMA}"
                )))),
            }
        })?;
        Ok(store)
    }

    fn lock(&self) -> MutexGuard<'_, Option<Client>> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The connection, reconnecting after it closed.
    fn client<'a>(
        &self,
        guard: &'a mut MutexGuard<'_, Option<Client>>,
    ) -> Result<&'a mut Client, Error> {
        if guard.as_ref().is_none_or(Client::is_closed) {
            let fresh = connect(&self.url)?;
            if let Some(old) = guard.replace(fresh) {
                close(old);
            }
        }
        Ok(guard.as_mut().expect("connected above"))
    }

    /// Run `f` in a serializable transaction, retrying it from the start
    /// on a serialization failure or deadlock. `full` chooses whether the
    /// commit waits for the database's durable flush.
    fn tx<T: Send>(
        &self,
        full: bool,
        f: impl Fn(&mut Transaction<'_>) -> R<T> + Send + Sync,
    ) -> Result<T, Error> {
        let mut guard = self.lock();
        let client = self.client(&mut guard)?;
        off_runtime(|| {
            let deadline = Instant::now() + RETRY_FOR;
            let mut pause = Duration::from_millis(1);
            loop {
                let attempt = (|| {
                    let mut tx = client
                        .build_transaction()
                        .isolation_level(postgres::IsolationLevel::Serializable)
                        .start()
                        .map_err(db("begin"))?;
                    if !full {
                        tx.batch_execute("SET LOCAL synchronous_commit = off")
                            .map_err(db("begin"))?;
                    }
                    let value = f(&mut tx)?;
                    tx.commit().map_err(db("commit"))?;
                    Ok(value)
                })();
                match attempt {
                    Ok(value) => return Ok(value),
                    Err(Fail::Error(error)) => return Err(error),
                    Err(Fail::Retry(error)) if Instant::now() >= deadline => {
                        return Err(state("a transaction kept conflicting", &error))
                    }
                    Err(Fail::Retry(_)) => {
                        std::thread::sleep(pause);
                        pause = (pause * 2).min(Duration::from_millis(50));
                    }
                }
            }
        })
    }

    /// Run a read outside any explicit transaction.
    fn query<T: Send>(
        &self,
        f: impl FnOnce(&mut Client) -> Result<T, postgres::Error> + Send,
    ) -> Result<T, Error> {
        let mut guard = self.lock();
        let client = self.client(&mut guard)?;
        off_runtime(|| f(client)).map_err(|e| state("read", &e))
    }

    fn check(&self, tx: &mut Transaction<'_>, fence: &Fence) -> R<()> {
        let row = tx
            .query_opt(
                "SELECT incarnation, generation, owner FROM by_leases \
                 WHERE repo = $1 AND branch = $2",
                &[&self.repo, &fence.branch],
            )
            .map_err(db("lease"))?;
        match row.map(|r| {
            (
                r.get::<_, i64>(0),
                r.get::<_, i64>(1),
                r.get::<_, Option<String>>(2),
            )
        }) {
            Some((incarnation, generation, Some(_)))
                if incarnation == fence.incarnation && uint(generation) == fence.generation =>
            {
                Ok(())
            }
            Some((_, _, None)) => Err(fenced(fence, "was released").into()),
            Some(_) => Err(fenced(fence, "was superseded").into()),
            None => Err(fenced(fence, "no longer exists").into()),
        }
    }

    fn incarnation(&self, tx: &mut Transaction<'_>, name: &str) -> R<Option<i64>> {
        Ok(tx
            .query_opt(
                "SELECT incarnation FROM by_branches WHERE repo = $1 AND name = $2",
                &[&self.repo, &name],
            )
            .map_err(db("branch"))?
            .map(|r| r.get(0)))
    }

    fn stored_record(&self, tx: &mut Transaction<'_>, name: &str) -> R<Option<Record>> {
        let text: Option<String> = tx
            .query_opt(
                "SELECT record FROM by_branches WHERE repo = $1 AND name = $2",
                &[&self.repo, &name],
            )
            .map_err(db("read"))?
            .and_then(|r| r.get(0));
        match text {
            Some(text) => Ok(Some(decode(&format!("record {name}"), &text)?)),
            None => Ok(None),
        }
    }

    /// Write `record`, keeping the stored children; inserts the branch when
    /// it has no row.
    fn put(&self, tx: &mut Transaction<'_>, record: &Record) -> R<()> {
        let name = &record.info.name;
        let mut record = record.clone();
        if let Some(current) = self.stored_record(tx, name)? {
            record.info.children = current.info.children;
        }
        let text = encode(name, &record)?;
        let created = int(record.created_ms);
        let updated = tx
            .execute(
                "UPDATE by_branches SET record = $3, created_ms = $4 WHERE repo = $1 AND name = $2",
                &[&self.repo, name, &text, &created],
            )
            .map_err(db("write"))?;
        if updated == 0 {
            tx.execute(
                "INSERT INTO by_branches (repo, name, created_ms, record) VALUES ($1, $2, $3, $4)",
                &[&self.repo, name, &created, &text],
            )
            .map_err(db("write"))?;
        }
        Ok(())
    }

    /// Append `event` to `name`'s log at the next feed position; returns
    /// its sequence number in the branch.
    fn insert_event(&self, tx: &mut Transaction<'_>, name: &str, event: &RecordedEvent) -> R<u64> {
        let activity = encode("event", &event.activity)?;
        let incarnation = self
            .incarnation(tx, name)?
            .ok_or_else(|| Error::UnknownBranch(name.to_owned()))?;
        let seq: i64 = tx
            .query_one(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM by_events WHERE incarnation = $1",
                &[&incarnation],
            )
            .map_err(db("append"))?
            .get(0);
        let id: i64 = tx
            .query_one(
                "INSERT INTO by_feed_heads (repo, head) VALUES ($1, 1) \
                 ON CONFLICT (repo) DO UPDATE SET head = by_feed_heads.head + 1 RETURNING head",
                &[&self.repo],
            )
            .map_err(db("append"))?
            .get(0);
        tx.execute(
            "INSERT INTO by_events (repo, id, branch, incarnation, seq, at_ms, activity) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            &[
                &self.repo,
                &id,
                &name,
                &incarnation,
                &seq,
                &int(event.at_ms),
                &activity,
            ],
        )
        .map_err(db("append"))?;
        Ok(uint(seq))
    }

    fn lease_row(&self, tx: &mut Transaction<'_>, name: &str) -> R<Option<LeaseRow>> {
        Ok(tx
            .query_opt(
                "SELECT branch, incarnation, generation, turn, owner, host, pid, pid_start, \
                 expires_ms, deadline_ms FROM by_leases WHERE repo = $1 AND branch = $2",
                &[&self.repo, &name],
            )
            .map_err(db("lease"))?
            .map(|r| lease_from(&r)))
    }
}

fn lease_from(r: &Row) -> LeaseRow {
    LeaseRow {
        branch: r.get(0),
        incarnation: r.get(1),
        generation: uint(r.get(2)),
        turn: uint(r.get(3)),
        owner: r.get(4),
        host: r.get(5),
        pid: u32::try_from(r.get::<_, i64>(6)).unwrap_or(0),
        start: r.get(7),
        expires_ms: uint(r.get(8)),
        deadline_ms: r.get::<_, Option<i64>>(9).map(uint),
    }
}

fn event_from(what: &str, at_ms: i64, activity: &str) -> Result<RecordedEvent, Error> {
    Ok(RecordedEvent {
        at_ms: uint(at_ms),
        activity: decode(what, activity)?,
    })
}

impl Backend for Postgres {
    fn reserve(&self, name: &str, owner: &Owner) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let now = int(now_ms());
            let inserted = tx
                .execute(
                    "INSERT INTO by_branches (repo, name, created_ms, record) \
                     VALUES ($1, $2, $3, NULL) ON CONFLICT (repo, name) DO NOTHING",
                    &[&self.repo, &name, &now],
                )
                .map_err(db("reserve"))?;
            if inserted == 1 {
                tx.execute(
                    "INSERT INTO by_reservations (repo, name, owner, host, pid, pid_start, \
                     reserved_ms) VALUES ($1, $2, $3, $4, $5, $6, $7) \
                     ON CONFLICT (repo, name) DO UPDATE SET owner = $3, host = $4, pid = $5, \
                     pid_start = $6, reserved_ms = $7",
                    &[
                        &self.repo,
                        &name,
                        &owner.id,
                        &owner.host,
                        &i64::from(owner.pid),
                        &owner.start,
                        &now,
                    ],
                )
                .map_err(db("reserve"))?;
            }
            Ok(inserted == 1)
        })
    }

    fn release(&self, name: &str) -> Result<(), Error> {
        self.tx(true, |tx| {
            let released = tx
                .execute(
                    "DELETE FROM by_branches WHERE repo = $1 AND name = $2 AND record IS NULL",
                    &[&self.repo, &name],
                )
                .map_err(db("release"))?;
            if released == 1 {
                tx.execute(
                    "DELETE FROM by_reservations WHERE repo = $1 AND name = $2",
                    &[&self.repo, &name],
                )
                .map_err(db("release"))?;
            }
            Ok(())
        })
    }

    fn reservations(&self) -> Result<Vec<ReservationRow>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT r.name, r.owner, r.host, r.pid, r.pid_start, r.reserved_ms \
                 FROM by_reservations r JOIN by_branches b \
                 ON b.repo = r.repo AND b.name = r.name \
                 WHERE r.repo = $1 AND b.record IS NULL ORDER BY r.name",
                &[&self.repo],
            )
        })?;
        Ok(rows
            .iter()
            .map(|r| ReservationRow {
                name: r.get(0),
                owner: r.get(1),
                host: r.get(2),
                pid: u32::try_from(r.get::<_, i64>(3)).unwrap_or(0),
                start: r.get(4),
                reserved_ms: uint(r.get(5)),
            })
            .collect())
    }

    fn reclaim(&self, row: &ReservationRow) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let removed = tx
                .execute(
                    "DELETE FROM by_reservations \
                     WHERE repo = $1 AND name = $2 AND owner = $3 AND reserved_ms = $4",
                    &[&self.repo, &row.name, &row.owner, &int(row.reserved_ms)],
                )
                .map_err(db("reclaim"))?;
            if removed == 0 {
                return Ok(false);
            }
            let freed = tx
                .execute(
                    "DELETE FROM by_branches WHERE repo = $1 AND name = $2 AND record IS NULL",
                    &[&self.repo, &row.name],
                )
                .map_err(db("reclaim"))?;
            Ok(freed == 1)
        })
    }

    fn taken(&self, name: &str) -> Result<bool, Error> {
        self.query(|c| {
            c.query_opt(
                "SELECT 1 FROM by_branches WHERE repo = $1 AND name = $2",
                &[&self.repo, &name],
            )
        })
        .map(|row| row.is_some())
    }

    fn read(&self, name: &str) -> Result<Option<Record>, Error> {
        let text: Option<String> = self
            .query(|c| {
                c.query_opt(
                    "SELECT record FROM by_branches WHERE repo = $1 AND name = $2",
                    &[&self.repo, &name],
                )
            })?
            .and_then(|r| r.get(0));
        text.map(|t| decode(&format!("record {name}"), &t))
            .transpose()
    }

    fn list(&self) -> Result<Vec<Record>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT name, record FROM by_branches WHERE repo = $1 AND record IS NOT NULL \
                 ORDER BY created_ms, name",
                &[&self.repo],
            )
        })?;
        rows.iter()
            .map(|r| {
                decode(
                    &format!("record {}", r.get::<_, String>(0)),
                    &r.get::<_, String>(1),
                )
            })
            .collect()
    }

    fn write(&self, record: &Record, fence: Option<&Fence>) -> Result<(), Error> {
        self.tx(true, |tx| {
            if let Some(fence) = fence {
                self.check(tx, fence)?;
            }
            self.put(tx, record)
        })
    }

    fn add_child(&self, parent: &str, child: &str) -> Result<(), Error> {
        self.tx(true, |tx| {
            let mut record = self
                .stored_record(tx, parent)?
                .ok_or_else(|| Error::UnknownBranch(parent.into()))?;
            if !record.info.children.iter().any(|c| c == child) {
                record.info.children.push(child.to_owned());
            }
            tx.execute(
                "UPDATE by_branches SET record = $3 WHERE repo = $1 AND name = $2",
                &[&self.repo, &parent, &encode(parent, &record)?],
            )
            .map_err(db("add child"))?;
            Ok(())
        })
    }

    fn delete(&self, name: &str) -> Result<(), Error> {
        self.tx(true, |tx| {
            let Some(incarnation) = self.incarnation(tx, name)? else {
                return Ok(());
            };
            tx.execute(
                "DELETE FROM by_reservations WHERE repo = $1 AND name = $2",
                &[&self.repo, &name],
            )
            .map_err(db("delete"))?;
            for sql in [
                "DELETE FROM by_steps WHERE incarnation = $1",
                "DELETE FROM by_processes WHERE incarnation = $1",
                "DELETE FROM by_cancels WHERE incarnation = $1",
                "DELETE FROM by_steers WHERE incarnation = $1",
                "DELETE FROM by_leases WHERE incarnation = $1",
                "DELETE FROM by_branches WHERE incarnation = $1",
            ] {
                tx.execute(sql, &[&incarnation]).map_err(db("delete"))?;
            }
            Ok(())
        })
    }

    fn acquire(&self, record: &Record, owner: &Owner, ttl: Duration) -> Result<Acquired, Error> {
        let name = &record.info.name;
        self.tx(true, |tx| {
            let incarnation = self
                .incarnation(tx, name)?
                .ok_or_else(|| Error::UnknownBranch(name.clone()))?;
            // Creating a reserved name: only its reserving engine may, so a
            // reservation reclaimed from a live but slow engine and taken by
            // another is not created twice.
            let reserver: Option<String> = tx
                .query_opt(
                    "SELECT r.owner FROM by_reservations r JOIN by_branches b \
                     ON b.repo = r.repo AND b.name = r.name \
                     WHERE r.repo = $1 AND r.name = $2 AND b.record IS NULL",
                    &[&self.repo, name],
                )
                .map_err(db("acquire"))?
                .map(|row| row.get(0));
            if reserver.is_some_and(|reserver| reserver != owner.id) {
                return Err(Error::BranchExists(name.clone()).into());
            }
            let current = self.lease_row(tx, name)?;
            if let Some(row) = &current {
                if row.owner.is_some() && row.incarnation == incarnation {
                    return Ok(Acquired::Held(row.clone()));
                }
            }
            let generation = current.map_or(1, |row| row.generation + 1);
            let now = now_ms();
            tx.execute(
                "INSERT INTO by_leases (repo, branch, incarnation, generation, turn, owner, host, \
                 pid, pid_start, acquired_ms, expires_ms, deadline_ms) \
                 VALUES ($1, $2, $3, $4, $4, $5, $6, $7, $8, $9, $10, NULL) \
                 ON CONFLICT (repo, branch) DO UPDATE SET incarnation = $3, generation = $4, \
                 turn = $4, owner = $5, host = $6, pid = $7, pid_start = $8, \
                 acquired_ms = $9, expires_ms = $10, deadline_ms = NULL",
                &[
                    &self.repo,
                    name,
                    &incarnation,
                    &int(generation),
                    &owner.id,
                    &owner.host,
                    &i64::from(owner.pid),
                    &owner.start,
                    &int(now),
                    &int(now + ttl.as_millis() as u64),
                ],
            )
            .map_err(db("acquire"))?;
            self.put(tx, record)?;
            tx.execute(
                "DELETE FROM by_reservations WHERE repo = $1 AND name = $2",
                &[&self.repo, name],
            )
            .map_err(db("acquire"))?;
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
            self.check(tx, fence)?;
            tx.execute(
                "UPDATE by_leases SET expires_ms = $3 WHERE repo = $1 AND branch = $2",
                &[
                    &self.repo,
                    &fence.branch,
                    &int(now_ms() + ttl.as_millis() as u64),
                ],
            )
            .map_err(db("renew"))?;
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
            self.check(tx, fence)?;
            if let Some(record) = record {
                self.put(tx, record)?;
            }
            if let Some(event) = event {
                self.insert_event(tx, &fence.branch, event)?;
            }
            tx.execute(
                "UPDATE by_leases SET owner = NULL, expires_ms = 0 WHERE repo = $1 AND branch = $2",
                &[&self.repo, &fence.branch],
            )
            .map_err(db("release"))?;
            Ok(())
        })
    }

    fn leases(&self) -> Result<Vec<LeaseRow>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT branch, incarnation, generation, turn, owner, host, pid, pid_start, \
                 expires_ms, deadline_ms FROM by_leases WHERE repo = $1 AND owner IS NOT NULL \
                 ORDER BY branch",
                &[&self.repo],
            )
        })?;
        Ok(rows.iter().map(lease_from).collect())
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
                    "UPDATE by_leases SET generation = generation + 1, owner = $4, host = $5, \
                     pid = $6, pid_start = $7, acquired_ms = $8, expires_ms = $9 \
                     WHERE repo = $1 AND branch = $2 AND generation = $3 AND owner IS NOT NULL",
                    &[
                        &self.repo,
                        &lease.branch,
                        &int(lease.generation),
                        &owner.id,
                        &owner.host,
                        &i64::from(owner.pid),
                        &owner.start,
                        &int(now),
                        &int(now + ttl.as_millis() as u64),
                    ],
                )
                .map_err(db("take over"))?;
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
            self.check(tx, fence)?;
            tx.execute(
                "UPDATE by_leases SET deadline_ms = $3 WHERE repo = $1 AND branch = $2",
                &[&self.repo, &fence.branch, &deadline_ms.map(int)],
            )
            .map_err(db("deadline"))?;
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
            self.check(tx, fence)?;
            let row = tx
                .query_opt(
                    "SELECT intent, outcome FROM by_steps \
                     WHERE incarnation = $1 AND turn = $2 AND step = $3",
                    &[&fence.incarnation, &int(turn), &step],
                )
                .map_err(db("step"))?
                .map(|r| (r.get::<_, String>(0), r.get::<_, Option<String>>(1)));
            match row {
                Some((_, Some(outcome))) => Ok(Begun::Done(decode(step, &outcome)?)),
                Some((intent, None)) => Ok(Begun::Pending(decode(step, &intent)?)),
                None => {
                    tx.execute(
                        "INSERT INTO by_steps (incarnation, turn, step, branch, generation, \
                         intent, started_ms) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                        &[
                            &fence.incarnation,
                            &int(turn),
                            &step,
                            &fence.branch,
                            &int(fence.generation),
                            &encode(step, intent)?,
                            &int(now_ms()),
                        ],
                    )
                    .map_err(db("step"))?;
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
            self.check(tx, fence)?;
            let changed = tx
                .execute(
                    "UPDATE by_steps SET outcome = $4, finished_ms = $5, generation = $6 \
                     WHERE incarnation = $1 AND turn = $2 AND step = $3",
                    &[
                        &fence.incarnation,
                        &int(turn),
                        &step,
                        &encode(step, outcome)?,
                        &int(now_ms()),
                        &int(fence.generation),
                    ],
                )
                .map_err(db("step"))?;
            match changed {
                1 => Ok(()),
                _ => Err(Error::State(format!(
                    "{}: step {step} of turn {turn} was never begun",
                    fence.branch
                ))
                .into()),
            }
        })
    }

    fn abandon_step(&self, fence: &Fence, turn: u64, step: &str) -> Result<(), Error> {
        self.tx(true, |tx| {
            self.check(tx, fence)?;
            tx.execute(
                "DELETE FROM by_steps WHERE incarnation = $1 AND turn = $2 AND step = $3 \
                 AND outcome IS NULL",
                &[&fence.incarnation, &int(turn), &step],
            )
            .map_err(db("step"))?;
            Ok(())
        })
    }

    fn steps(&self, name: &str, turn: u64) -> Result<Vec<StepRow>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT s.step, s.intent, s.outcome FROM by_steps s \
                 JOIN by_branches b ON b.incarnation = s.incarnation \
                 WHERE b.repo = $1 AND b.name = $2 AND s.turn = $3 \
                 ORDER BY s.started_ms, s.step",
                &[&self.repo, &name, &int(turn)],
            )
        })?;
        rows.iter()
            .map(|r| {
                let step: String = r.get(0);
                Ok(StepRow {
                    intent: decode(&step, &r.get::<_, String>(1))?,
                    outcome: r
                        .get::<_, Option<String>>(2)
                        .map(|o| decode(&step, &o))
                        .transpose()?,
                    step,
                })
            })
            .collect()
    }

    fn record_process(&self, fence: &Fence, process: &ProcessRow) -> Result<(), Error> {
        self.tx(true, |tx| {
            self.check(tx, fence)?;
            tx.execute(
                "INSERT INTO by_processes (incarnation, turn, pid, branch, pgid, start, host, \
                 generation, recorded_ms) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT (incarnation, turn, pid) DO UPDATE SET branch = $4, pgid = $5, \
                 start = $6, host = $7, generation = $8, recorded_ms = $9",
                &[
                    &fence.incarnation,
                    &int(fence.turn),
                    &i64::from(process.pid),
                    &fence.branch,
                    &i64::from(process.pgid),
                    &process.start,
                    &process.host,
                    &int(fence.generation),
                    &int(now_ms()),
                ],
            )
            .map_err(db("process"))?;
            Ok(())
        })
    }

    fn processes(&self, name: &str, turn: u64) -> Result<Vec<ProcessRow>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT p.pid, p.pgid, p.start, p.host FROM by_processes p \
                 JOIN by_branches b ON b.incarnation = p.incarnation \
                 WHERE b.repo = $1 AND b.name = $2 AND p.turn = $3 ORDER BY p.recorded_ms",
                &[&self.repo, &name, &int(turn)],
            )
        })?;
        Ok(rows
            .iter()
            .map(|r| ProcessRow {
                pid: u32::try_from(r.get::<_, i64>(0)).unwrap_or(0),
                pgid: u32::try_from(r.get::<_, i64>(1)).unwrap_or(0),
                start: r.get(2),
                host: r.get(3),
            })
            .collect())
    }

    fn request_cancel(&self, name: &str, by: &str, subtree: bool) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let Some(incarnation) = self.incarnation(tx, name)? else {
                return Err(Error::UnknownBranch(name.to_owned()).into());
            };
            let lease = self.lease_row(tx, name)?;
            let Some(lease) = lease.filter(|l| l.owner.is_some() && l.incarnation == incarnation)
            else {
                return Ok(false);
            };
            tx.execute(
                "INSERT INTO by_cancels (incarnation, turn, branch, requested_by, at_ms, subtree) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (incarnation, turn) DO NOTHING",
                &[
                    &incarnation,
                    &int(lease.turn),
                    &name,
                    &by,
                    &int(now_ms()),
                    &subtree,
                ],
            )
            .map_err(db("cancel"))?;
            Ok(true)
        })
    }

    fn cancel_requested(&self, fence: &Fence) -> Result<Option<String>, Error> {
        Ok(self
            .query(|c| {
                c.query_opt(
                    "SELECT requested_by FROM by_cancels WHERE incarnation = $1 AND turn = $2",
                    &[&fence.incarnation, &int(fence.turn)],
                )
            })?
            .map(|r| r.get(0)))
    }

    fn request_steer(&self, name: &str, by: &str, text: &str) -> Result<Option<u64>, Error> {
        self.tx(true, |tx| {
            let Some(incarnation) = self.incarnation(tx, name)? else {
                return Err(Error::UnknownBranch(name.to_owned()).into());
            };
            let lease = self.lease_row(tx, name)?;
            let Some(lease) = lease.filter(|l| l.owner.is_some() && l.incarnation == incarnation)
            else {
                return Ok(None);
            };
            let row = tx
                .query_one(
                    "INSERT INTO by_steers (incarnation, turn, branch, requested_by, text, at_ms, \
                     state) VALUES ($1, $2, $3, $4, $5, $6, 'pending') RETURNING id",
                    &[
                        &incarnation,
                        &int(lease.turn),
                        &name,
                        &by,
                        &text,
                        &int(now_ms()),
                    ],
                )
                .map_err(db("steer"))?;
            Ok(Some(uint(row.get::<_, i64>(0))))
        })
    }

    fn pending_steers(&self, fence: &Fence) -> Result<Vec<SteerRow>, Error> {
        Ok(self
            .query(|c| {
                c.query(
                    "SELECT id, branch, turn, requested_by, text, at_ms, state, reason \
                     FROM by_steers WHERE incarnation = $1 AND turn = $2 AND state = 'pending' \
                     ORDER BY id",
                    &[&fence.incarnation, &int(fence.turn)],
                )
            })?
            .iter()
            .map(steer_row)
            .collect())
    }

    fn settle_steer(&self, fence: &Fence, id: u64, state: &SteerState) -> Result<(), Error> {
        let (name, reason) = state.columns();
        self.tx(true, |tx| {
            self.check(tx, fence)?;
            tx.execute(
                "UPDATE by_steers SET state = $4, reason = $5 \
                 WHERE id = $1 AND incarnation = $2 AND turn = $3",
                &[
                    &int(id),
                    &fence.incarnation,
                    &int(fence.turn),
                    &name,
                    &reason,
                ],
            )
            .map_err(db("steer"))?;
            Ok(())
        })
    }

    fn steer(&self, name: &str, id: u64) -> Result<Option<SteerRow>, Error> {
        let repo = self.repo.clone();
        let name = name.to_owned();
        Ok(self
            .query(move |c| {
                c.query_opt(
                    "SELECT s.id, s.branch, s.turn, s.requested_by, s.text, s.at_ms, s.state, \
                     s.reason FROM by_steers s JOIN by_branches b \
                     ON b.incarnation = s.incarnation \
                     WHERE b.repo = $1 AND b.name = $2 AND s.id = $3",
                    &[&repo, &name, &int(id)],
                )
            })?
            .as_ref()
            .map(steer_row))
    }

    fn append(
        &self,
        name: &str,
        event: &RecordedEvent,
        fence: Option<&Fence>,
    ) -> Result<u64, Error> {
        self.tx(false, |tx| {
            if let Some(fence) = fence {
                self.check(tx, fence)?;
            }
            self.insert_event(tx, name, event)
        })
    }

    fn events_since(
        &self,
        name: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<(u64, RecordedEvent)>, Error> {
        let (known, rows) = self.query(|c| {
            let known = c
                .query_opt(
                    "SELECT 1 FROM by_branches WHERE repo = $1 AND name = $2",
                    &[&self.repo, &name],
                )?
                .is_some();
            let rows = c.query(
                "SELECT e.seq, e.at_ms, e.activity FROM by_events e \
                 JOIN by_branches b ON b.incarnation = e.incarnation \
                 WHERE b.repo = $1 AND b.name = $2 AND e.seq > $3 ORDER BY e.seq LIMIT $4",
                &[&self.repo, &name, &int(after), &int(limit as u64)],
            )?;
            Ok((known, rows))
        })?;
        if !known {
            return Err(Error::UnknownBranch(name.to_owned()));
        }
        rows.iter()
            .map(|r| {
                let seq: i64 = r.get(0);
                let what = format!("event {seq} of {name}");
                Ok((
                    uint(seq),
                    event_from(&what, r.get(1), &r.get::<_, String>(2))?,
                ))
            })
            .collect()
    }

    fn event_count(&self, name: &str) -> Result<u64, Error> {
        let row = self.query(|c| {
            c.query_opt(
                "SELECT (SELECT COALESCE(MAX(seq), 0) FROM by_events e \
                 WHERE e.incarnation = b.incarnation) FROM by_branches b \
                 WHERE b.repo = $1 AND b.name = $2",
                &[&self.repo, &name],
            )
        })?;
        row.map(|r| uint(r.get(0)))
            .ok_or_else(|| Error::UnknownBranch(name.to_owned()))
    }

    fn feed_since(&self, after: u64, limit: usize) -> Result<Vec<FeedRow>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT id, branch, at_ms, activity FROM by_events WHERE repo = $1 AND id > $2 \
                 ORDER BY id LIMIT $3",
                &[&self.repo, &int(after), &int(limit as u64)],
            )
        })?;
        rows.iter()
            .map(|r| {
                let id: i64 = r.get(0);
                Ok(FeedRow {
                    id: uint(id),
                    branch: r.get(1),
                    event: event_from(
                        &format!("feed entry {id}"),
                        r.get(2),
                        &r.get::<_, String>(3),
                    )?,
                })
            })
            .collect()
    }

    fn head(&self) -> Result<u64, Error> {
        self.query(|c| {
            c.query_one(
                "SELECT COALESCE(MAX(id), 0) FROM by_events WHERE repo = $1",
                &[&self.repo],
            )
        })
        .map(|r| uint(r.get(0)))
    }
}

fn artifact_row_from(row: &Row) -> Result<ArtifactRow, Error> {
    let ancestry: String = row.get(6);
    let labels: String = row.get(9);
    Ok(ArtifactRow {
        artifact: ArtifactRef {
            id: row.get(0),
            digest: row.get(1),
            size: uint(row.get(2)),
            name: row.get(3),
            media_type: row.get(4),
            publisher_branch: row.get(5),
            turn: uint(row.get(7)),
            created_at: uint(row.get::<_, i64>(8)) / 1000,
            labels: decode("artifact labels", &labels)?,
        },
        ancestry: decode("artifact ancestry", &ancestry)?,
    })
}

const ARTIFACT_COLUMNS: &str =
    "id, digest, size, name, media_type, publisher, ancestry, turn, created_ms, labels";

fn scratch_row_from(row: &Row) -> Result<ScratchRow, Error> {
    let ancestry: String = row.get(2);
    Ok(ScratchRow {
        area: ScratchArea {
            name: row.get(0),
            owner_branch: row.get(1),
            created_at: uint(row.get::<_, i64>(3)) / 1000,
        },
        ancestry: decode("scratch ancestry", &ancestry)?,
    })
}

impl Postgres {
    fn is_running(&self, tx: &mut Transaction<'_>, name: &str) -> R<bool> {
        match self.stored_record(tx, name)? {
            Some(record) => Ok(record.info.status == BranchStatus::Running),
            None => Ok(false),
        }
    }
}

impl StorageBackend for Postgres {
    fn create_artifact(&self, new: &NewArtifact) -> Result<ArtifactRow, Error> {
        self.tx(true, |tx| {
            let now = int(now_ms());
            let ancestry = encode("ancestry", &new.ancestry)?;
            let labels = encode("labels", &new.labels)?;
            let seq: i64 = tx
                .query_one(
                    "INSERT INTO by_artifacts (repo, id, digest, size, name, media_type, \
                     publisher, ancestry, turn, created_ms, labels) \
                     VALUES ($1, '', $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING seq",
                    &[
                        &self.repo,
                        &new.digest,
                        &int(new.size),
                        &new.name,
                        &new.media_type,
                        &new.publisher_branch,
                        &ancestry,
                        &int(new.turn),
                        &now,
                        &labels,
                    ],
                )
                .map_err(db("artifact"))?
                .get(0);
            let id = format!("art{seq}");
            tx.execute(
                "UPDATE by_artifacts SET id = $1 WHERE repo = $2 AND seq = $3",
                &[&id, &self.repo, &seq],
            )
            .map_err(db("artifact"))?;
            Ok(ArtifactRow {
                artifact: ArtifactRef {
                    id,
                    digest: new.digest.clone(),
                    size: new.size,
                    name: new.name.clone(),
                    media_type: new.media_type.clone(),
                    publisher_branch: new.publisher_branch.clone(),
                    turn: new.turn,
                    created_at: uint(now) / 1000,
                    labels: new.labels.clone(),
                },
                ancestry: new.ancestry.clone(),
            })
        })
    }

    fn artifact(&self, id: &str) -> Result<Option<ArtifactRow>, Error> {
        self.query(|c| {
            c.query_opt(
                &format!("SELECT {ARTIFACT_COLUMNS} FROM by_artifacts WHERE repo = $1 AND id = $2"),
                &[&self.repo, &id],
            )
        })?
        .map(|r| artifact_row_from(&r))
        .transpose()
    }

    fn artifacts(&self) -> Result<Vec<ArtifactRow>, Error> {
        let rows = self.query(|c| {
            c.query(
                &format!(
                    "SELECT {ARTIFACT_COLUMNS} FROM by_artifacts WHERE repo = $1 ORDER BY seq"
                ),
                &[&self.repo],
            )
        })?;
        rows.iter().map(artifact_row_from).collect()
    }

    fn artifact_shares(&self, id: &str) -> Result<Vec<String>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT branch FROM by_artifact_shares WHERE repo = $1 AND id = $2",
                &[&self.repo, &id],
            )
        })?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    fn share_artifact(&self, id: &str, branch: &str) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let exists = tx
                .query_opt(
                    "SELECT 1 FROM by_artifacts WHERE repo = $1 AND id = $2",
                    &[&self.repo, &id],
                )
                .map_err(db("artifact"))?
                .is_some();
            if exists {
                tx.execute(
                    "INSERT INTO by_artifact_shares (repo, id, branch) VALUES ($1, $2, $3) \
                     ON CONFLICT DO NOTHING",
                    &[&self.repo, &id, &branch],
                )
                .map_err(db("artifact share"))?;
            }
            Ok(exists)
        })
    }

    fn delete_artifact(&self, id: &str) -> Result<(), Error> {
        self.tx(true, |tx| {
            tx.execute(
                "DELETE FROM by_artifacts WHERE repo = $1 AND id = $2",
                &[&self.repo, &id],
            )
            .map_err(db("artifact"))?;
            tx.execute(
                "DELETE FROM by_artifact_shares WHERE repo = $1 AND id = $2",
                &[&self.repo, &id],
            )
            .map_err(db("artifact share"))?;
            Ok(())
        })
    }

    fn digest_refcount(&self, digest: &str) -> Result<u64, Error> {
        let row = self.query(|c| {
            c.query_one(
                "SELECT COUNT(*) FROM by_artifacts WHERE repo = $1 AND digest = $2",
                &[&self.repo, &digest],
            )
        })?;
        Ok(uint(row.get::<_, i64>(0)))
    }

    fn create_scratch(&self, name: &str, owner: &str, ancestry: &[String]) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let ancestry = encode("ancestry", &ancestry)?;
            let inserted = tx
                .execute(
                    "INSERT INTO by_scratch_areas (repo, name, owner, ancestry, created_ms) \
                     VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
                    &[&self.repo, &name, &owner, &ancestry, &int(now_ms())],
                )
                .map_err(db("scratch"))?;
            Ok(inserted == 1)
        })
    }

    fn scratch(&self, name: &str) -> Result<Option<ScratchRow>, Error> {
        self.query(|c| {
            c.query_opt(
                "SELECT name, owner, ancestry, created_ms FROM by_scratch_areas \
                 WHERE repo = $1 AND name = $2",
                &[&self.repo, &name],
            )
        })?
        .map(|r| scratch_row_from(&r))
        .transpose()
    }

    fn scratch_list(&self) -> Result<Vec<ScratchRow>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT name, owner, ancestry, created_ms FROM by_scratch_areas \
                 WHERE repo = $1 ORDER BY created_ms",
                &[&self.repo],
            )
        })?;
        rows.iter().map(scratch_row_from).collect()
    }

    fn scratch_shares(&self, name: &str) -> Result<Vec<String>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT branch FROM by_scratch_shares WHERE repo = $1 AND name = $2",
                &[&self.repo, &name],
            )
        })?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    fn share_scratch(&self, name: &str, branch: &str) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let exists = tx
                .query_opt(
                    "SELECT 1 FROM by_scratch_areas WHERE repo = $1 AND name = $2",
                    &[&self.repo, &name],
                )
                .map_err(db("scratch"))?
                .is_some();
            if exists {
                tx.execute(
                    "INSERT INTO by_scratch_shares (repo, name, branch) VALUES ($1, $2, $3) \
                     ON CONFLICT DO NOTHING",
                    &[&self.repo, &name, &branch],
                )
                .map_err(db("scratch share"))?;
            }
            Ok(exists)
        })
    }

    fn delete_scratch(&self, name: &str) -> Result<(), Error> {
        self.tx(true, |tx| {
            for table in ["by_scratch_areas", "by_scratch_shares", "by_scratch_locks"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE repo = $1 AND name = $2"),
                    &[&self.repo, &name],
                )
                .map_err(db("scratch"))?;
            }
            Ok(())
        })
    }

    fn scratch_lock(&self, name: &str, branch: &str) -> Result<Option<LockOutcome>, Error> {
        self.tx(true, |tx| {
            let known = tx
                .query_opt(
                    "SELECT 1 FROM by_scratch_areas WHERE repo = $1 AND name = $2",
                    &[&self.repo, &name],
                )
                .map_err(db("scratch"))?
                .is_some();
            if !known {
                return Ok(None);
            }
            let current = tx
                .query_opt(
                    "SELECT holder, acquired_ms FROM by_scratch_locks \
                     WHERE repo = $1 AND name = $2",
                    &[&self.repo, &name],
                )
                .map_err(db("scratch lock"))?
                .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1)));
            let grant = |tx: &mut Transaction<'_>| -> R<LockOutcome> {
                let now = int(now_ms());
                tx.execute(
                    "INSERT INTO by_scratch_locks (repo, name, holder, acquired_ms) \
                     VALUES ($1, $2, $3, $4) ON CONFLICT (repo, name) \
                     DO UPDATE SET holder = excluded.holder, acquired_ms = excluded.acquired_ms",
                    &[&self.repo, &name, &branch, &now],
                )
                .map_err(db("scratch lock"))?;
                Ok(LockOutcome::Granted(ScratchLock {
                    name: name.to_owned(),
                    holder_branch: branch.to_owned(),
                    acquired_at: uint(now) / 1000,
                }))
            };
            match current {
                None => Ok(Some(grant(tx)?)),
                Some((holder, _)) if holder == branch => Ok(Some(grant(tx)?)),
                Some((holder, _)) if !self.is_running(tx, &holder)? => Ok(Some(grant(tx)?)),
                Some((holder, acquired_ms)) => Ok(Some(LockOutcome::Held(ScratchLock {
                    name: name.to_owned(),
                    holder_branch: holder,
                    acquired_at: uint(acquired_ms) / 1000,
                }))),
            }
        })
    }

    fn scratch_unlock(&self, name: &str, branch: &str) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let changed = tx
                .execute(
                    "DELETE FROM by_scratch_locks WHERE repo = $1 AND name = $2 AND holder = $3",
                    &[&self.repo, &name, &branch],
                )
                .map_err(db("scratch lock"))?;
            Ok(changed == 1)
        })
    }

    fn scratch_lock_state(&self, name: &str) -> Result<Option<ScratchLock>, Error> {
        self.query(|c| {
            c.query_opt(
                "SELECT holder, acquired_ms FROM by_scratch_locks WHERE repo = $1 AND name = $2",
                &[&self.repo, &name],
            )
        })
        .map(|opt| {
            opt.map(|r| ScratchLock {
                name: name.to_owned(),
                holder_branch: r.get(0),
                acquired_at: uint(r.get::<_, i64>(1)) / 1000,
            })
        })
    }
}
