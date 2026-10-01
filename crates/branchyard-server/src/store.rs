//! Where operations persist, and the durable queue they are dispatched
//! through.
//!
//! [`OperationStore`] keeps four things in one database, so that each
//! change to them is one transaction:
//!
//! - operation records, with a unique index on the caller's idempotency
//!   key, so a retry anywhere maps to the same operation;
//! - the dispatch queue: one row per accepted, unfinished operation, with
//!   the serializable description of its work, claimed by a worker under a
//!   lease and a fence;
//! - branch locks, so two operations never change one branch at once, on
//!   this server or another sharing the database;
//! - webhook cursors.
//!
//! Admission ([`OperationStore::admit`]) writes the record, its idempotency
//! binding, its branch locks and its queue row in one transaction, before
//! the server answers `202 Accepted`: a committed admission is a durable
//! enqueue, and a failed one leaves nothing behind. Finishing
//! ([`OperationStore::finish`]) writes the outcome, deletes the queue row
//! and releases the locks in one transaction, only while the worker's
//! claim still holds.
//!
//! [`SqliteStore`] is what a server with a data directory uses, and
//! [`MemoryStore`] is the same on an in-memory database. With the
//! `postgres` feature, [`PostgresStore`] keeps them in PostgreSQL, where
//! several servers may share them: claims use `FOR UPDATE SKIP LOCKED`.
//! The queue is plain tables; PGMQ could replace the queue table later
//! without changing the transaction's shape. [`FileStore`], the JSON-lines
//! file earlier versions used, is only read, to import it once.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use branchyard_client::api::Operation;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Principal, DEFAULT_TENANT};

/// An idempotency key as the server scopes it: per authenticated caller and
/// per request route, with a fingerprint of the request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Idempotency {
    /// The token's configured name.
    pub caller: String,
    pub key: String,
    /// Method, route and canonical body, hashed.
    pub fingerprint: String,
}

/// An operation and what the server needs to deduplicate and lock it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredOperation {
    pub operation: Operation,
    #[serde(default)]
    pub idempotency: Option<Idempotency>,
    /// Branch names no other operation may change while this one runs.
    #[serde(default)]
    pub locks: Vec<String>,
    /// The tenant of the principal that submitted this operation. Absent
    /// (default) on a record from before tenants existed, which reads as
    /// [`crate::config::DEFAULT_TENANT`] everywhere this is used: not part
    /// of the wire `Operation`, since it is for the server's own isolation
    /// and quota bookkeeping, not something a caller needs echoed back.
    /// Admission counts a tenant's queued and running operations by this
    /// field of the durable record, so `max_running` holds across servers
    /// sharing the store and across restarts.
    #[serde(default)]
    pub tenant: String,
    /// The principal that admitted this operation, as its credential
    /// verified at admission: the worker that runs it, on any server or a
    /// `by worker` process with no credentials of its own, acts with this
    /// principal's tenant, scopes and repositories, never its own. Absent
    /// on a record from before tenants existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<Principal>,
    /// Branches of `operation.repo` this operation will create, as planned
    /// at admission: counted against its tenant's `max_branches` while it
    /// is queued or running, before they exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub creates: Vec<String>,
}

impl StoredOperation {
    /// The tenant this operation belongs to: a record from before tenants
    /// existed belongs to [`DEFAULT_TENANT`].
    pub fn tenant(&self) -> &str {
        match self.tenant.is_empty() {
            true => DEFAULT_TENANT,
            false => &self.tenant,
        }
    }
}

/// A tenant's ceilings that admission checks inside its transaction,
/// against the operation store's own durable rows: what makes them hold
/// across every server sharing the store and across restarts.
#[derive(Default)]
pub struct AdmissionQuota {
    /// Queued and running operations of the tenant at once.
    pub max_running: Option<usize>,
    /// Branches the tenant has, or will have once its queued and running
    /// operations create theirs, across its repositories.
    pub max_branches: Option<usize>,
    /// With `max_branches`: the branches (repository, name) that exist in
    /// the tenant's repositories now. Called inside the admission's
    /// transaction, after the tenant's unfinished operations were read, so
    /// an operation that finishes in between is counted by one or the
    /// other.
    pub existing_branches: Option<ExistingBranches>,
}

/// Reads the branches that exist in a tenant's repositories.
pub type ExistingBranches = Box<dyn Fn() -> io::Result<BTreeSet<(String, String)>> + Send + Sync>;

impl AdmissionQuota {
    fn is_empty(&self) -> bool {
        self.max_running.is_none() && self.max_branches.is_none()
    }

    /// The refusal, if admitting `operation` next to the tenant's
    /// `unfinished` operations would exceed a ceiling.
    fn check(
        &self,
        operation: &StoredOperation,
        unfinished: &[StoredOperation],
    ) -> io::Result<Option<Admission>> {
        if let Some(max) = self.max_running {
            if unfinished.len() >= max {
                return Ok(Some(Admission::Quota {
                    limit: "max_running",
                    max,
                    reserved: unfinished.len(),
                }));
            }
        }
        let Some(max) = self.max_branches else {
            return Ok(None);
        };
        if operation.creates.is_empty() {
            return Ok(None);
        }
        let mut branches: BTreeSet<(String, String)> = unfinished
            .iter()
            .flat_map(|o| {
                o.creates
                    .iter()
                    .map(|b| (o.operation.repo.clone(), b.clone()))
            })
            .collect();
        if let Some(existing) = &self.existing_branches {
            branches.extend(existing()?);
        }
        let reserved = branches.len();
        let after = operation
            .creates
            .iter()
            .filter(|b| !branches.contains(&(operation.operation.repo.clone(), (*b).clone())))
            .count()
            + reserved;
        Ok((after > max).then_some(Admission::Quota {
            limit: "max_branches",
            max,
            reserved,
        }))
    }
}

/// A process that claims queued operations: named like the engine names a
/// lease's holder, so a claim whose process is gone from this host is
/// taken over at once, and any other when its lease expires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worker {
    /// Unique per registry.
    pub id: String,
    /// Host and boot.
    pub host: String,
    pub pid: u32,
    /// The process's start time.
    pub start: String,
}

impl Worker {
    /// This process, under a fresh ID.
    pub fn current() -> Worker {
        let (host, pid, start) = branchyard::process_identity();
        Worker {
            id: format!("w_{}", &branchyard_client::new_key()[..20]),
            host,
            pid,
            start,
        }
    }
}

/// A queued operation claimed by a worker. `fence` is the claim's attempt
/// number: every later write for the claim names it, and is refused once
/// another worker has claimed the operation since.
#[derive(Clone, Debug, PartialEq)]
pub struct Claim {
    pub operation: StoredOperation,
    /// The serializable description of the work (`crate::work::Work`).
    pub work: Value,
    pub fence: i64,
}

/// A worker as its last [`OperationStore::beat`] recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveWorker {
    pub id: String,
    pub host: String,
    pub labels: Vec<String>,
    pub repos: Vec<String>,
    /// Milliseconds since its last beat.
    pub seen_ms_ago: u64,
}

/// Why a queued operation requiring `requires` of `repo` cannot be claimed
/// by any of `workers`, if none of them serving the repository carries
/// every label; `None` when one can (it is only busy).
pub fn unclaimable(requires: &[String], repo: &str, workers: &[LiveWorker]) -> Option<String> {
    if requires.is_empty() {
        return None;
    }
    let serving: Vec<&LiveWorker> = workers
        .iter()
        .filter(|w| w.repos.iter().any(|r| r == repo))
        .collect();
    if serving
        .iter()
        .any(|w| requires.iter().all(|l| w.labels.contains(l)))
    {
        return None;
    }
    let live = match serving.is_empty() {
        true => format!("no live worker serves {repo}"),
        false => format!(
            "live workers serving {repo}: {}",
            serving
                .iter()
                .map(|w| format!("{} on {} [{}]", w.id, w.host, w.labels.join(", ")))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    };
    Some(format!(
        "no live worker carries the labels it requires ({}); {live}",
        requires.join(", ")
    ))
}

/// Whether `label` may name a worker label: 1 to 63 of lowercase letters,
/// digits, `.`, `_` and `-`, starting with a letter or digit.
pub fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// What [`OperationStore::admit`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum Admission {
    /// Recorded and enqueued, with its locks taken, in one transaction.
    Admitted,
    /// The caller's key already names this operation; nothing was written.
    Replayed(Box<StoredOperation>),
    /// A branch is held; nothing was written.
    Busy { branch: String, holder: String },
    /// The tenant is at a ceiling of its [`AdmissionQuota`]; nothing was
    /// written.
    Quota {
        limit: &'static str,
        max: usize,
        reserved: usize,
    },
}

/// Durable operation records, dispatch queue and branch locks.
///
/// Every method that changes something commits before it returns, and
/// the commit would survive a crash: the server answers `202 Accepted`
/// only after [`OperationStore::admit`], which is what makes an idempotent
/// retry safe (invariant 4).
pub trait OperationStore: Send + Sync {
    /// Every operation as last saved, oldest first.
    fn load(&self) -> io::Result<Vec<StoredOperation>>;
    /// One operation.
    fn get(&self, id: &str) -> io::Result<Option<StoredOperation>>;
    /// The operation `caller` created with `key`.
    fn by_key(&self, caller: &str, key: &str) -> io::Result<Option<StoredOperation>>;
    /// Replace an operation's record. Only for records without a queue row
    /// (an unfinished record left by a version without the queue); a queued
    /// operation changes through [`OperationStore::start`] and
    /// [`OperationStore::finish`].
    fn save(&self, operation: &StoredOperation) -> io::Result<()>;
    /// Unfinished operations with no queue row: left by a version of the
    /// server that ran operations in memory.
    fn orphans(&self) -> io::Result<Vec<StoredOperation>>;

    /// In one transaction: the operation's record and idempotency binding,
    /// its tenant's `quota` checked against the tenant's queued and running
    /// operations, its branch locks, and its queue row carrying `work`.
    /// Nothing is written when the key already names an operation, when
    /// the tenant is at a ceiling, when a branch is held, or when any write
    /// fails. Admissions of one tenant with a quota take turns.
    fn admit(
        &self,
        operation: &StoredOperation,
        work: &Value,
        quota: &AdmissionQuota,
    ) -> io::Result<Admission>;
    /// The tenant's queued and running operations.
    fn unfinished(&self, tenant: &str) -> io::Result<Vec<StoredOperation>>;
    /// Claim the oldest queued operation of one of `repos` that no live
    /// claim holds and whose required labels are all among `labels`, for
    /// `lease`. A claim whose lease expired, or whose process is gone from
    /// this host, is claimed again under a new fence.
    fn claim(
        &self,
        worker: &Worker,
        repos: &[String],
        labels: &[String],
        lease: Duration,
    ) -> io::Result<Option<Claim>>;
    /// Record that `worker`, carrying `labels`, serves `repos` now.
    fn beat(&self, worker: &Worker, labels: &[String], repos: &[String]) -> io::Result<()>;
    /// Workers that beat within `within`.
    fn workers(&self, within: Duration) -> io::Result<Vec<LiveWorker>>;
    /// Forget `worker`, which is stopping.
    fn leave(&self, worker: &Worker) -> io::Result<()>;
    /// Extend a claim's lease; false when the claim was lost.
    fn renew(&self, worker: &Worker, id: &str, fence: i64, lease: Duration) -> io::Result<bool>;
    /// Record `operation` (now running) and extend the lease, if the claim
    /// still holds; false otherwise, and nothing is written.
    fn start(
        &self,
        worker: &Worker,
        fence: i64,
        operation: &StoredOperation,
        lease: Duration,
    ) -> io::Result<bool>;
    /// Record `operation`'s outcome, delete its queue row and release its
    /// branch locks, in one transaction, if the claim still holds; false
    /// otherwise, and nothing is written.
    fn finish(&self, worker: &Worker, fence: i64, operation: &StoredOperation) -> io::Result<bool>;
    /// Give a claim back unstarted, for another worker to claim at once.
    fn release(&self, worker: &Worker, id: &str, fence: i64) -> io::Result<()>;
    /// Queued operations of `repos`, claimed or not.
    fn pending(&self, repos: &[String]) -> io::Result<usize>;

    /// Hold `branch` of `repo` for a short synchronous change, under
    /// `token`, until [`OperationStore::unhold`] or `ttl` passes. The
    /// holder's name when the branch is already held.
    fn hold(
        &self,
        repo: &str,
        branch: &str,
        holder: &str,
        token: &str,
        ttl: Duration,
    ) -> io::Result<Option<String>>;
    fn unhold(&self, repo: &str, branch: &str, token: &str) -> io::Result<()>;
    /// Only for a store no other process uses: every claim and hold was
    /// this process's predecessor's, so release them all.
    fn reset(&self) -> io::Result<()>;

    /// A webhook's last delivered feed position (`repo:webhook_id`); `None`
    /// before its first delivery.
    fn load_webhook_cursor(&self, id: &str) -> io::Result<Option<u64>>;
    /// Advance a webhook's cursor. Only ever moves forward; the caller
    /// guarantees that.
    fn save_webhook_cursor(&self, id: &str, cursor: u64) -> io::Result<()>;
}

fn ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn parse_op(id: &str, body: &str, place: &str) -> io::Result<StoredOperation> {
    serde_json::from_str(body).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{place}operation {id}: {e}"),
        )
    })
}

/// Branch names to lock, each once, in a fixed order so two admissions
/// never wait on each other's locks in opposite orders.
fn lock_order(locks: &[String]) -> Vec<String> {
    let mut locks = locks.to_vec();
    locks.sort();
    locks.dedup();
    locks
}

/// Operations as JSON lines in one append-only file, each save fsynced.
/// The latest line for an ID wins. Compacted when opened.
///
/// What earlier versions of the server kept; [`SqliteStore::open`] imports
/// it once. It has no queue and no locks, so it is not an
/// [`OperationStore`].
pub struct FileStore {
    path: PathBuf,
    file: Mutex<File>,
}

impl FileStore {
    pub fn open(path: impl Into<PathBuf>) -> io::Result<FileStore> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let latest = read_latest(&path)?;
        // Rewrite atomically with one line per operation, dropping any torn
        // final line.
        let temp = path.with_extension("jsonl.tmp");
        {
            let mut out = File::create(&temp)?;
            for op in &latest {
                serde_json::to_writer(&mut out, op)?;
                out.write_all(b"\n")?;
            }
            out.sync_all()?;
        }
        fs::rename(&temp, &path)?;
        if let Some(parent) = path.parent() {
            // Make the rename durable.
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(FileStore {
            path,
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every operation as last saved, oldest first.
    pub fn load(&self) -> io::Result<Vec<StoredOperation>> {
        read_latest(&self.path)
    }

    /// Append one operation.
    pub fn save(&self, operation: &StoredOperation) -> io::Result<()> {
        let mut line = serde_json::to_vec(operation)?;
        line.push(b'\n');
        let mut file = self.file.lock().unwrap_or_else(|p| p.into_inner());
        file.write_all(&line)?;
        file.sync_data()
    }
}

fn read_latest(path: &Path) -> io::Result<Vec<StoredOperation>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut order = Vec::new();
    let mut latest: HashMap<String, StoredOperation> = HashMap::new();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut number = 0;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        number += 1;
        if line.last() != Some(&b'\n') {
            eprintln!(
                "branchyard-server: ignoring a torn final line in {}",
                path.display()
            );
            break;
        }
        let op: StoredOperation = serde_json::from_slice(&line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} line {number}: {e}", path.display()),
            )
        })?;
        let id = op.operation.id.clone();
        if latest.insert(id.clone(), op).is_none() {
            order.push(id);
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|id| latest.remove(&id))
        .collect())
}

/// Operations, queue and locks in SQLite at `DATA-DIR/state.db`, in
/// write-ahead-log mode, each change committed with `synchronous=FULL`
/// before it returns. Changes that read before they write run in
/// `BEGIN IMMEDIATE` transactions, so writers take turns: the single-writer
/// equivalent of PostgreSQL's row locks.
///
/// This is the same embedded database the engine keeps per repository, in
/// its own file: the registry spans every served repository and lives in
/// the server's data directory, not in any one of them.
pub struct SqliteStore {
    path: PathBuf,
    conn: Mutex<rusqlite::Connection>,
}

fn sql(error: rusqlite::Error) -> io::Error {
    io::Error::other(error)
}

const SQLITE_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS operations (
        id TEXT PRIMARY KEY,
        seq INTEGER NOT NULL,
        body TEXT NOT NULL
    );
    CREATE UNIQUE INDEX IF NOT EXISTS operations_idempotency ON operations (
        json_extract(body, '$.idempotency.caller'),
        json_extract(body, '$.idempotency.key')
    );
    CREATE TABLE IF NOT EXISTS operation_queue (
        id TEXT PRIMARY KEY,
        seq INTEGER NOT NULL,
        repo TEXT NOT NULL,
        work TEXT NOT NULL,
        attempt INTEGER NOT NULL DEFAULT 0,
        worker TEXT,
        host TEXT,
        pid INTEGER,
        start TEXT,
        lease_until INTEGER
    );
    CREATE INDEX IF NOT EXISTS operation_queue_seq ON operation_queue (seq);
    CREATE TABLE IF NOT EXISTS branch_locks (
        repo TEXT NOT NULL,
        branch TEXT NOT NULL,
        holder TEXT NOT NULL,
        token TEXT NOT NULL,
        expires_at INTEGER,
        PRIMARY KEY (repo, branch)
    );
    CREATE INDEX IF NOT EXISTS branch_locks_token ON branch_locks (token);
    CREATE TABLE IF NOT EXISTS webhook_cursors (
        id TEXT PRIMARY KEY,
        cursor INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS workers (
        id TEXT PRIMARY KEY,
        host TEXT NOT NULL,
        pid INTEGER NOT NULL,
        labels TEXT NOT NULL,
        repos TEXT NOT NULL,
        seen INTEGER NOT NULL
    );";

/// Columns added since the table was first created.
fn sqlite_migrate(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let has: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('operation_queue') WHERE name = 'requires'",
        [],
        |r| r.get(0),
    )?;
    if has == 0 {
        conn.execute_batch(
            "ALTER TABLE operation_queue ADD COLUMN requires TEXT NOT NULL DEFAULT '[]'",
        )?;
    }
    Ok(())
}

/// Switch to write-ahead logging. A file still in rollback mode that
/// another connection is switching too answers busy at once, without
/// waiting on the busy timeout, so it is asked again for as long.
fn sqlite_wal(conn: &rusqlite::Connection) -> rusqlite::Result<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)) {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            mode => return mode,
        }
    }
}

fn sqlite_now() -> i64 {
    crate::ops::now_ms() as i64
}

type Conn = rusqlite::Connection;

fn sqlite_op(conn: &Conn, sql_text: &str, param: &str) -> io::Result<Option<StoredOperation>> {
    let row: Option<(String, String)> = conn
        .query_row(sql_text, [param], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .map_err(sql)?;
    row.map(|(id, body)| parse_op(&id, &body, "")).transpose()
}

fn sqlite_by_key(conn: &Conn, caller: &str, key: &str) -> io::Result<Option<StoredOperation>> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT id, body FROM operations \
             WHERE json_extract(body, '$.idempotency.caller') = ?1 \
               AND json_extract(body, '$.idempotency.key') = ?2",
            [caller, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql)?;
    row.map(|(id, body)| parse_op(&id, &body, "")).transpose()
}

impl SqliteStore {
    /// Open or create the database at `path`. If `legacy` names an
    /// `operations.jsonl` left by an earlier version, its operations are
    /// imported once and the file is renamed to `operations.jsonl.imported`.
    pub fn open(path: impl Into<PathBuf>, legacy: Option<&Path>) -> io::Result<SqliteStore> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut conn = rusqlite::Connection::open(&path).map_err(sql)?;
        conn.busy_timeout(Duration::from_secs(30)).map_err(sql)?;
        let mode = sqlite_wal(&conn).map_err(sql)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(io::Error::other(format!(
                "{} could not use write-ahead logging (journal mode {mode})",
                path.display()
            )));
        }
        conn.execute_batch("PRAGMA synchronous = FULL;")
            .map_err(sql)?;
        // Servers opening one file at once take turns: the migration
        // reads a column's absence, then adds it.
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql)?;
        tx.execute_batch(SQLITE_SCHEMA).map_err(sql)?;
        sqlite_migrate(&tx).map_err(sql)?;
        tx.commit().map_err(sql)?;
        let store = SqliteStore {
            path,
            conn: Mutex::new(conn),
        };
        if let Some(legacy) = legacy.filter(|p| p.is_file()) {
            for op in read_latest(legacy)? {
                store.insert(&op, false)?;
            }
            let imported = legacy.with_extension("jsonl.imported");
            fs::rename(legacy, &imported)?;
        }
        Ok(store)
    }

    /// An in-memory database: for tests and embedding.
    pub fn memory() -> SqliteStore {
        let conn = rusqlite::Connection::open_in_memory().expect("an in-memory database");
        conn.execute_batch(SQLITE_SCHEMA)
            .expect("the schema on an in-memory database");
        sqlite_migrate(&conn).expect("the schema on an in-memory database");
        SqliteStore {
            path: PathBuf::from(":memory:"),
            conn: Mutex::new(conn),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Conn> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Run `f` in a `BEGIN IMMEDIATE` transaction, committed when it
    /// returns `Ok((value, true))` and rolled back otherwise.
    fn immediate<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> io::Result<(T, bool)>,
    ) -> io::Result<T> {
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql)?;
        let (value, commit) = f(&tx)?;
        match commit {
            true => tx.commit().map_err(sql)?,
            false => tx.rollback().map_err(sql)?,
        }
        Ok(value)
    }

    /// Save `operation`; `replace` keeps an existing one otherwise.
    fn insert(&self, operation: &StoredOperation, replace: bool) -> io::Result<()> {
        sqlite_insert(&self.conn(), operation, replace).map(|_| ())
    }
}

/// Insert or replace an operation's record; the number of rows written.
fn sqlite_insert(conn: &Conn, operation: &StoredOperation, replace: bool) -> io::Result<usize> {
    let body = serde_json::to_string(operation)?;
    let conflict = match replace {
        true => "ON CONFLICT (id) DO UPDATE SET body = excluded.body",
        false => "ON CONFLICT DO NOTHING",
    };
    conn.execute(
        &format!(
            "INSERT INTO operations (id, seq, body) \
             VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM operations), ?2) {conflict}"
        ),
        rusqlite::params![operation.operation.id, body],
    )
    .map_err(sql)
}

/// The tenant's operations with a queue row: queued or running. A record
/// without a tenant (from before tenants existed) is the default tenant's.
fn sqlite_unfinished(conn: &Conn, tenant: &str) -> io::Result<Vec<StoredOperation>> {
    let mut statement = conn
        .prepare(
            "SELECT o.id, o.body FROM operation_queue q JOIN operations o ON o.id = q.id \
             WHERE COALESCE(NULLIF(json_extract(o.body, '$.tenant'), ''), ?2) = ?1 \
             ORDER BY q.seq",
        )
        .map_err(sql)?;
    let rows: Vec<(String, String)> = statement
        .query_map([tenant, DEFAULT_TENANT], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    rows.iter()
        .map(|(id, body)| parse_op(id, body, ""))
        .collect()
}

/// Release claims held by processes gone from this host.
fn sqlite_reap(conn: &Conn, worker: &Worker, now: i64) -> io::Result<()> {
    let mut statement = conn
        .prepare(
            "SELECT id, attempt, pid, start FROM operation_queue \
             WHERE host = ?1 AND worker IS NOT NULL AND worker <> ?2 AND lease_until > ?3",
        )
        .map_err(sql)?;
    let rows: Vec<(String, i64, i64, String)> = statement
        .query_map(rusqlite::params![worker.host, worker.id, now], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    for (id, attempt, pid, start) in rows {
        if branchyard::process_gone(&worker.host, pid as u32, &start) {
            conn.execute(
                "UPDATE operation_queue SET worker = NULL, lease_until = NULL \
                 WHERE id = ?1 AND attempt = ?2",
                rusqlite::params![id, attempt],
            )
            .map_err(sql)?;
        }
    }
    Ok(())
}

impl OperationStore for SqliteStore {
    fn load(&self) -> io::Result<Vec<StoredOperation>> {
        let conn = self.conn();
        let mut statement = conn
            .prepare("SELECT id, body FROM operations ORDER BY seq")
            .map_err(sql)?;
        let rows = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(sql)?;
        let place = format!("{} ", self.path.display());
        let mut ops = Vec::new();
        for row in rows {
            let (id, body) = row.map_err(sql)?;
            ops.push(parse_op(&id, &body, &place)?);
        }
        Ok(ops)
    }

    fn get(&self, id: &str) -> io::Result<Option<StoredOperation>> {
        sqlite_op(
            &self.conn(),
            "SELECT id, body FROM operations WHERE id = ?1",
            id,
        )
    }

    fn by_key(&self, caller: &str, key: &str) -> io::Result<Option<StoredOperation>> {
        sqlite_by_key(&self.conn(), caller, key)
    }

    fn save(&self, operation: &StoredOperation) -> io::Result<()> {
        self.insert(operation, true)
    }

    fn orphans(&self) -> io::Result<Vec<StoredOperation>> {
        let conn = self.conn();
        let mut statement = conn
            .prepare(
                "SELECT id, body FROM operations \
                 WHERE json_extract(body, '$.operation.state') IN ('queued', 'running') \
                   AND id NOT IN (SELECT id FROM operation_queue) ORDER BY seq",
            )
            .map_err(sql)?;
        let rows: Vec<(String, String)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)?;
        rows.iter()
            .map(|(id, body)| parse_op(id, body, ""))
            .collect()
    }

    fn admit(
        &self,
        operation: &StoredOperation,
        work: &Value,
        quota: &AdmissionQuota,
    ) -> io::Result<Admission> {
        let work = serde_json::to_string(work)?;
        let op = &operation.operation;
        // `BEGIN IMMEDIATE` makes every admission take its turn, so the
        // tenant's count below cannot change before this one commits.
        self.immediate(|tx| {
            if let Some(idem) = &operation.idempotency {
                if let Some(existing) = sqlite_by_key(tx, &idem.caller, &idem.key)? {
                    return Ok((Admission::Replayed(Box::new(existing)), false));
                }
            }
            if !quota.is_empty() {
                let unfinished = sqlite_unfinished(tx, operation.tenant())?;
                if let Some(refused) = quota.check(operation, &unfinished)? {
                    return Ok((refused, false));
                }
            }
            let now = sqlite_now();
            for branch in lock_order(&operation.locks) {
                tx.execute(
                    "DELETE FROM branch_locks WHERE repo = ?1 AND branch = ?2 \
                     AND expires_at IS NOT NULL AND expires_at <= ?3",
                    rusqlite::params![op.repo, branch, now],
                )
                .map_err(sql)?;
                let holder: Option<String> = tx
                    .query_row(
                        "SELECT holder FROM branch_locks WHERE repo = ?1 AND branch = ?2",
                        [&op.repo, &branch],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sql)?;
                if let Some(holder) = holder {
                    return Ok((Admission::Busy { branch, holder }, false));
                }
                tx.execute(
                    "INSERT INTO branch_locks (repo, branch, holder, token) VALUES (?1, ?2, ?3, ?3)",
                    rusqlite::params![op.repo, branch, op.id],
                )
                .map_err(sql)?;
            }
            sqlite_insert(tx, operation, false)?;
            tx.execute(
                "INSERT INTO operation_queue (id, seq, repo, work, requires) \
                 VALUES (?1, (SELECT seq FROM operations WHERE id = ?1), ?2, ?3, ?4)",
                rusqlite::params![op.id, op.repo, work, serde_json::to_string(&op.requires)?],
            )
            .map_err(sql)?;
            Ok((Admission::Admitted, true))
        })
    }

    fn unfinished(&self, tenant: &str) -> io::Result<Vec<StoredOperation>> {
        sqlite_unfinished(&self.conn(), tenant)
    }

    fn claim(
        &self,
        worker: &Worker,
        repos: &[String],
        labels: &[String],
        lease: Duration,
    ) -> io::Result<Option<Claim>> {
        let repos = serde_json::to_string(repos)?;
        let labels = serde_json::to_string(labels)?;
        self.immediate(|tx| {
            let now = sqlite_now();
            sqlite_reap(tx, worker, now)?;
            let next: Option<(String, i64, String)> = tx
                .query_row(
                    "SELECT id, attempt, work FROM operation_queue \
                     WHERE repo IN (SELECT value FROM json_each(?1)) \
                       AND (lease_until IS NULL OR lease_until <= ?2) \
                       AND NOT EXISTS (SELECT 1 FROM json_each(requires) r \
                           WHERE r.value NOT IN (SELECT value FROM json_each(?3))) \
                     ORDER BY seq LIMIT 1",
                    rusqlite::params![repos, now, labels],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(sql)?;
            let Some((id, attempt, work)) = next else {
                return Ok((None, false));
            };
            let fence = attempt + 1;
            tx.execute(
                "UPDATE operation_queue SET attempt = ?2, worker = ?3, host = ?4, pid = ?5, \
                 start = ?6, lease_until = ?7 WHERE id = ?1",
                rusqlite::params![
                    id,
                    fence,
                    worker.id,
                    worker.host,
                    worker.pid,
                    worker.start,
                    now + ms(lease)
                ],
            )
            .map_err(sql)?;
            let operation = sqlite_op(tx, "SELECT id, body FROM operations WHERE id = ?1", &id)?
                .ok_or_else(|| io::Error::other(format!("queued operation {id} has no record")))?;
            let work = serde_json::from_str(&work)?;
            Ok((
                Some(Claim {
                    operation,
                    work,
                    fence,
                }),
                true,
            ))
        })
    }

    fn beat(&self, worker: &Worker, labels: &[String], repos: &[String]) -> io::Result<()> {
        self.conn()
            .execute(
                "INSERT INTO workers (id, host, pid, labels, repos, seen) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (id) DO UPDATE SET \
                 labels = excluded.labels, repos = excluded.repos, seen = excluded.seen",
                rusqlite::params![
                    worker.id,
                    worker.host,
                    worker.pid,
                    serde_json::to_string(labels)?,
                    serde_json::to_string(repos)?,
                    sqlite_now()
                ],
            )
            .map_err(sql)?;
        Ok(())
    }

    fn workers(&self, within: Duration) -> io::Result<Vec<LiveWorker>> {
        let now = sqlite_now();
        let conn = self.conn();
        let mut statement = conn
            .prepare(
                "SELECT id, host, labels, repos, seen FROM workers WHERE seen > ?1 ORDER BY id",
            )
            .map_err(sql)?;
        let rows = statement
            .query_map([now - ms(within)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(sql)?;
        let mut live = Vec::new();
        for row in rows {
            let (id, host, labels, repos, seen) = row.map_err(sql)?;
            live.push(LiveWorker {
                id,
                host,
                labels: serde_json::from_str(&labels)?,
                repos: serde_json::from_str(&repos)?,
                seen_ms_ago: (now - seen).max(0) as u64,
            });
        }
        Ok(live)
    }

    fn leave(&self, worker: &Worker) -> io::Result<()> {
        self.conn()
            .execute("DELETE FROM workers WHERE id = ?1", [&worker.id])
            .map_err(sql)?;
        Ok(())
    }

    fn renew(&self, worker: &Worker, id: &str, fence: i64, lease: Duration) -> io::Result<bool> {
        let changed = self
            .conn()
            .execute(
                "UPDATE operation_queue SET lease_until = ?4 \
                 WHERE id = ?1 AND attempt = ?2 AND worker = ?3",
                rusqlite::params![id, fence, worker.id, sqlite_now() + ms(lease)],
            )
            .map_err(sql)?;
        Ok(changed == 1)
    }

    fn start(
        &self,
        worker: &Worker,
        fence: i64,
        operation: &StoredOperation,
        lease: Duration,
    ) -> io::Result<bool> {
        self.immediate(|tx| {
            let held = tx
                .execute(
                    "UPDATE operation_queue SET lease_until = ?4 \
                     WHERE id = ?1 AND attempt = ?2 AND worker = ?3",
                    rusqlite::params![
                        operation.operation.id,
                        fence,
                        worker.id,
                        sqlite_now() + ms(lease)
                    ],
                )
                .map_err(sql)?;
            if held != 1 {
                return Ok((false, false));
            }
            sqlite_insert(tx, operation, true)?;
            Ok((true, true))
        })
    }

    fn finish(&self, worker: &Worker, fence: i64, operation: &StoredOperation) -> io::Result<bool> {
        let id = &operation.operation.id;
        self.immediate(|tx| {
            let held = tx
                .execute(
                    "DELETE FROM operation_queue WHERE id = ?1 AND attempt = ?2 AND worker = ?3",
                    rusqlite::params![id, fence, worker.id],
                )
                .map_err(sql)?;
            if held != 1 {
                return Ok((false, false));
            }
            sqlite_insert(tx, operation, true)?;
            tx.execute("DELETE FROM branch_locks WHERE token = ?1", [id])
                .map_err(sql)?;
            Ok((true, true))
        })
    }

    fn release(&self, worker: &Worker, id: &str, fence: i64) -> io::Result<()> {
        self.conn()
            .execute(
                "UPDATE operation_queue SET worker = NULL, lease_until = NULL \
                 WHERE id = ?1 AND attempt = ?2 AND worker = ?3",
                rusqlite::params![id, fence, worker.id],
            )
            .map_err(sql)?;
        Ok(())
    }

    fn pending(&self, repos: &[String]) -> io::Result<usize> {
        let repos = serde_json::to_string(repos)?;
        let count: i64 = self
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM operation_queue \
                 WHERE repo IN (SELECT value FROM json_each(?1))",
                [repos],
                |r| r.get(0),
            )
            .map_err(sql)?;
        Ok(count as usize)
    }

    fn hold(
        &self,
        repo: &str,
        branch: &str,
        holder: &str,
        token: &str,
        ttl: Duration,
    ) -> io::Result<Option<String>> {
        self.immediate(|tx| {
            let now = sqlite_now();
            tx.execute(
                "DELETE FROM branch_locks WHERE repo = ?1 AND branch = ?2 \
                 AND expires_at IS NOT NULL AND expires_at <= ?3",
                rusqlite::params![repo, branch, now],
            )
            .map_err(sql)?;
            let existing: Option<String> = tx
                .query_row(
                    "SELECT holder FROM branch_locks WHERE repo = ?1 AND branch = ?2",
                    [repo, branch],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            if existing.is_some() {
                return Ok((existing, false));
            }
            tx.execute(
                "INSERT INTO branch_locks (repo, branch, holder, token, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![repo, branch, holder, token, now + ms(ttl)],
            )
            .map_err(sql)?;
            Ok((None, true))
        })
    }

    fn unhold(&self, repo: &str, branch: &str, token: &str) -> io::Result<()> {
        self.conn()
            .execute(
                "DELETE FROM branch_locks WHERE repo = ?1 AND branch = ?2 AND token = ?3",
                [repo, branch, token],
            )
            .map_err(sql)?;
        Ok(())
    }

    fn reset(&self) -> io::Result<()> {
        self.conn()
            .execute_batch(
                "BEGIN IMMEDIATE;
                 UPDATE operation_queue SET worker = NULL, lease_until = NULL;
                 DELETE FROM branch_locks WHERE expires_at IS NOT NULL;
                 COMMIT;",
            )
            .map_err(sql)
    }

    fn load_webhook_cursor(&self, id: &str) -> io::Result<Option<u64>> {
        let found: Option<i64> = self
            .conn()
            .query_row(
                "SELECT cursor FROM webhook_cursors WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql)?;
        Ok(found.map(|c| c as u64))
    }

    fn save_webhook_cursor(&self, id: &str, cursor: u64) -> io::Result<()> {
        self.conn()
            .execute(
                "INSERT INTO webhook_cursors (id, cursor) VALUES (?1, ?2) \
                 ON CONFLICT (id) DO UPDATE SET cursor = excluded.cursor",
                rusqlite::params![id, cursor as i64],
            )
            .map_err(sql)?;
        Ok(())
    }
}

/// Operations, queue and locks in a PostgreSQL database, in the
/// connection's `search_path` schema, each change committed with
/// `synchronous_commit = on` before it returns.
///
/// Several servers may share one schema. Claims take the oldest claimable
/// row with `FOR UPDATE SKIP LOCKED`, so two workers never claim one row;
/// leases are measured by the database's clock, so servers' clocks need
/// not agree. Two admissions with one idempotency key, or for one branch,
/// meet at a unique index and the second waits for the first to commit.
///
/// Every call runs on a thread of its own, since the registry is called
/// from the server's asynchronous handlers and the client blocks.
#[cfg(feature = "postgres")]
pub struct PostgresStore {
    url: String,
    conn: Mutex<Option<postgres::Client>>,
}

/// The registry's schema, one object per step: its name (a relation, or
/// `table.column` for a column added since the table was first created)
/// and the statement that makes it.
///
/// [`PostgresStore::open`] runs only the steps whose object is missing,
/// each in a transaction of its own. A statement here locks its table even
/// when the object exists already (`CREATE INDEX IF NOT EXISTS` takes a
/// `SHARE` lock, `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` an `ACCESS
/// EXCLUSIVE` one), and a server opening the registry while another works
/// on it must not hold one table's lock while it waits for another's: the
/// other's `start` and `finish` write the queue, then the operation, and
/// the two would wait for each other.
#[cfg(feature = "postgres")]
const PG_SCHEMA: &[(&str, &str)] = &[
    (
        "by_operations",
        "CREATE TABLE IF NOT EXISTS by_operations (
            id TEXT PRIMARY KEY,
            seq BIGINT GENERATED ALWAYS AS IDENTITY,
            body TEXT NOT NULL
        )",
    ),
    (
        "by_operations_idempotency",
        "CREATE UNIQUE INDEX IF NOT EXISTS by_operations_idempotency ON by_operations (
            ((body::jsonb) #>> '{idempotency,caller}'),
            ((body::jsonb) #>> '{idempotency,key}')
        )",
    ),
    (
        "by_operation_queue",
        "CREATE TABLE IF NOT EXISTS by_operation_queue (
            id TEXT PRIMARY KEY REFERENCES by_operations (id),
            seq BIGINT GENERATED ALWAYS AS IDENTITY,
            repo TEXT NOT NULL,
            work TEXT NOT NULL,
            attempt BIGINT NOT NULL DEFAULT 0,
            worker TEXT,
            host TEXT,
            pid BIGINT,
            start TEXT,
            lease_until TIMESTAMPTZ
        )",
    ),
    (
        "by_operation_queue_seq",
        "CREATE INDEX IF NOT EXISTS by_operation_queue_seq ON by_operation_queue (seq)",
    ),
    (
        "by_branch_locks",
        "CREATE TABLE IF NOT EXISTS by_branch_locks (
            repo TEXT NOT NULL,
            branch TEXT NOT NULL,
            holder TEXT NOT NULL,
            token TEXT NOT NULL,
            expires_at TIMESTAMPTZ,
            PRIMARY KEY (repo, branch)
        )",
    ),
    (
        "by_branch_locks_token",
        "CREATE INDEX IF NOT EXISTS by_branch_locks_token ON by_branch_locks (token)",
    ),
    (
        "by_webhook_cursors",
        "CREATE TABLE IF NOT EXISTS by_webhook_cursors (
            id TEXT PRIMARY KEY,
            cursor BIGINT NOT NULL
        )",
    ),
    (
        "by_operation_queue.requires",
        "ALTER TABLE by_operation_queue \
         ADD COLUMN IF NOT EXISTS requires TEXT[] NOT NULL DEFAULT '{}'",
    ),
    (
        "by_workers",
        "CREATE TABLE IF NOT EXISTS by_workers (
            id TEXT PRIMARY KEY,
            host TEXT NOT NULL,
            pid BIGINT NOT NULL,
            labels TEXT[] NOT NULL,
            repos TEXT[] NOT NULL,
            seen TIMESTAMPTZ NOT NULL
        )",
    ),
];

/// Which of `names` (see [`PG_SCHEMA`]) are missing, read from the
/// catalogs on the connection's search path without locking any table.
#[cfg(feature = "postgres")]
fn pg_missing(
    c: &mut impl postgres::GenericClient,
    names: &[&str],
) -> Result<Vec<String>, postgres::Error> {
    let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    let rows = c.query(
        "SELECT n FROM unnest($1::text[]) WITH ORDINALITY AS t (n, i) \
         WHERE CASE WHEN strpos(n, '.') = 0 THEN to_regclass(n) IS NULL \
             ELSE NOT EXISTS (SELECT 1 FROM pg_attribute \
                 WHERE attrelid = to_regclass(split_part(n, '.', 1)) \
                   AND attname = split_part(n, '.', 2) AND NOT attisdropped) END \
         ORDER BY i",
        &[&names],
    )?;
    Ok(rows.iter().map(|row| row.get(0)).collect())
}

#[cfg(feature = "postgres")]
const PG_BY_KEY: &str = "SELECT id, body FROM by_operations \
     WHERE (body::jsonb) #>> '{idempotency,caller}' = $1 \
       AND (body::jsonb) #>> '{idempotency,key}' = $2";

/// The tenant's operations with a queue row (`$1`), a record without a
/// tenant being the default tenant's (`$2`).
#[cfg(feature = "postgres")]
const PG_UNFINISHED: &str = "SELECT o.id, o.body FROM by_operation_queue q \
     JOIN by_operations o ON o.id = q.id \
     WHERE COALESCE(NULLIF((o.body::jsonb) ->> 'tenant', ''), $2) = $1 ORDER BY q.seq";

/// Why a PostgreSQL admission wrote nothing.
#[cfg(feature = "postgres")]
enum Refused {
    /// The key is bound already.
    Replayed,
    /// A branch is held.
    Busy(String),
    /// A quota's refusal.
    Admission(Admission),
    Io(io::Error),
}

#[cfg(feature = "postgres")]
fn pg_ops(rows: &[postgres::Row]) -> io::Result<Vec<StoredOperation>> {
    rows.iter()
        .map(|row| {
            let (id, body): (String, String) = (row.get(0), row.get(1));
            parse_op(&id, &body, "")
        })
        .collect()
}

#[cfg(feature = "postgres")]
fn pg_op(rows: &[postgres::Row]) -> io::Result<Option<StoredOperation>> {
    rows.first()
        .map(|row| {
            let (id, body): (String, String) = (row.get(0), row.get(1));
            parse_op(&id, &body, "")
        })
        .transpose()
}

#[cfg(feature = "postgres")]
impl PostgresStore {
    /// Connect and create the tables if they are missing.
    pub fn open(url: &str) -> io::Result<PostgresStore> {
        let store = PostgresStore {
            url: url.to_owned(),
            conn: Mutex::new(None),
        };
        store.with(|client| {
            let names: Vec<&str> = PG_SCHEMA.iter().map(|(name, _)| *name).collect();
            // A schema already made, the usual case, is left alone: no
            // statement that would lock a table another server is using.
            let missing = pg_missing(client, &names)?;
            for (name, statement) in PG_SCHEMA {
                if !missing.iter().any(|m| m == name) {
                    continue;
                }
                // One opener at a time makes or migrates the schema; each
                // step locks at most one existing table, so it only ever
                // waits behind another server's transaction, never with it.
                let mut tx = client.transaction()?;
                tx.execute("SELECT pg_advisory_xact_lock(7390184326)", &[])?;
                if !pg_missing(&mut tx, &[name])?.is_empty() {
                    tx.batch_execute(statement)?;
                }
                tx.commit()?;
            }
            Ok(())
        })?;
        Ok(store)
    }

    /// Run `f` with a connection, on another thread, reconnecting after
    /// the connection closed.
    fn with<T: Send>(
        &self,
        f: impl FnOnce(&mut postgres::Client) -> Result<T, postgres::Error> + Send,
    ) -> io::Result<T> {
        let mut guard = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let conn: &mut Option<postgres::Client> = &mut guard;
        let url = &self.url;
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    if conn.as_ref().is_none_or(postgres::Client::is_closed) {
                        // Dropped here, on this thread, off the runtime.
                        *conn = Some(postgres::Client::connect(url, postgres::NoTls)?);
                    }
                    f(conn.as_mut().expect("connected above"))
                })
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
        .map_err(|e| match e.as_db_error() {
            Some(db) => io::Error::other(format!("operation registry: {}", db.message())),
            None => io::Error::other(format!("operation registry: {e}")),
        })
    }
}

#[cfg(feature = "postgres")]
impl Drop for PostgresStore {
    /// The client blocks to close its connection, which a Tokio runtime's
    /// thread may not.
    fn drop(&mut self) {
        let conn = self.conn.get_mut().unwrap_or_else(|p| p.into_inner());
        if let Some(client) = conn.take() {
            std::thread::scope(|scope| {
                scope.spawn(move || drop(client));
            });
        }
    }
}

#[cfg(feature = "postgres")]
fn pg_lease(lease: Duration) -> i64 {
    ms(lease)
}

#[cfg(feature = "postgres")]
impl OperationStore for PostgresStore {
    fn load(&self) -> io::Result<Vec<StoredOperation>> {
        let rows =
            self.with(|c| c.query("SELECT id, body FROM by_operations ORDER BY seq", &[]))?;
        rows.iter()
            .map(|row| {
                let (id, body): (String, String) = (row.get(0), row.get(1));
                parse_op(&id, &body, "")
            })
            .collect()
    }

    fn get(&self, id: &str) -> io::Result<Option<StoredOperation>> {
        let id = id.to_owned();
        let rows = self
            .with(move |c| c.query("SELECT id, body FROM by_operations WHERE id = $1", &[&id]))?;
        pg_op(&rows)
    }

    fn by_key(&self, caller: &str, key: &str) -> io::Result<Option<StoredOperation>> {
        let (caller, key) = (caller.to_owned(), key.to_owned());
        let rows = self.with(move |c| c.query(PG_BY_KEY, &[&caller, &key]))?;
        pg_op(&rows)
    }

    fn save(&self, operation: &StoredOperation) -> io::Result<()> {
        let body = serde_json::to_string(operation)?;
        let id = operation.operation.id.clone();
        self.with(move |c| {
            c.execute(
                "INSERT INTO by_operations (id, body) VALUES ($1, $2) \
                 ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body",
                &[&id, &body],
            )
        })?;
        Ok(())
    }

    fn orphans(&self) -> io::Result<Vec<StoredOperation>> {
        let rows = self.with(|c| {
            c.query(
                "SELECT o.id, o.body FROM by_operations o \
                 WHERE (o.body::jsonb) #>> '{operation,state}' IN ('queued', 'running') \
                   AND NOT EXISTS (SELECT 1 FROM by_operation_queue q WHERE q.id = o.id) \
                 ORDER BY o.seq",
                &[],
            )
        })?;
        rows.iter()
            .map(|row| {
                let (id, body): (String, String) = (row.get(0), row.get(1));
                parse_op(&id, &body, "")
            })
            .collect()
    }

    fn admit(
        &self,
        operation: &StoredOperation,
        work: &Value,
        quota: &AdmissionQuota,
    ) -> io::Result<Admission> {
        let body = serde_json::to_string(operation)?;
        let work = serde_json::to_string(work)?;
        let op = &operation.operation;
        let (id, repo) = (op.id.clone(), op.repo.clone());
        let idem = operation.idempotency.clone();
        let locks = lock_order(&operation.locks);
        let tenant = operation.tenant().to_owned();
        let requires = op.requires.clone();
        let admitted = self.with(move |c| {
            let mut tx = c.transaction()?;
            if !quota.is_empty() {
                // Admissions of one tenant with a quota take turns, on
                // every server sharing the schema: the count below then
                // cannot change before this one commits or rolls back.
                // Taken before any row, so it never waits behind one.
                tx.execute(
                    "SELECT pg_advisory_xact_lock(hashtextextended('branchyard tenant ' || $1, 0))",
                    &[&tenant],
                )?;
            }
            // The idempotency binding first: a second admission with this
            // key waits here for the first to commit, then replays it.
            let inserted = tx.execute(
                "INSERT INTO by_operations (id, body) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                &[&id, &body],
            )?;
            if inserted == 0 {
                tx.rollback()?;
                return Ok(Err(Refused::Replayed));
            }
            if !quota.is_empty() {
                let rows = tx.query(PG_UNFINISHED, &[&tenant, &DEFAULT_TENANT])?;
                let unfinished = match pg_ops(&rows) {
                    Ok(ops) => ops,
                    Err(e) => {
                        tx.rollback()?;
                        return Ok(Err(Refused::Io(e)));
                    }
                };
                match quota.check(operation, &unfinished) {
                    Ok(None) => {}
                    Ok(Some(refused)) => {
                        tx.rollback()?;
                        return Ok(Err(Refused::Admission(refused)));
                    }
                    Err(e) => {
                        tx.rollback()?;
                        return Ok(Err(Refused::Io(e)));
                    }
                }
            }
            for branch in &locks {
                tx.execute(
                    "DELETE FROM by_branch_locks WHERE repo = $1 AND branch = $2 \
                     AND expires_at IS NOT NULL AND expires_at <= clock_timestamp()",
                    &[&repo, branch],
                )?;
                let taken = tx.execute(
                    "INSERT INTO by_branch_locks (repo, branch, holder, token) \
                     VALUES ($1, $2, $3, $3) ON CONFLICT DO NOTHING",
                    &[&repo, branch, &id],
                )?;
                if taken == 0 {
                    tx.rollback()?;
                    return Ok(Err(Refused::Busy(branch.clone())));
                }
            }
            tx.execute(
                "INSERT INTO by_operation_queue (id, repo, work, requires) \
                 VALUES ($1, $2, $3, $4)",
                &[&id, &repo, &work, &requires],
            )?;
            tx.commit()?;
            Ok(Ok(()))
        })?;
        match admitted {
            Ok(()) => Ok(Admission::Admitted),
            Err(Refused::Admission(refused)) => Ok(refused),
            Err(Refused::Io(e)) => Err(e),
            Err(Refused::Replayed) => {
                let existing = match &idem {
                    Some(idem) => self.by_key(&idem.caller, &idem.key)?,
                    None => None,
                };
                existing
                    .map(|e| Admission::Replayed(Box::new(e)))
                    .ok_or_else(|| {
                        io::Error::other(format!("operation {} was already recorded", op.id))
                    })
            }
            Err(Refused::Busy(branch)) => {
                let (repo, name) = (op.repo.clone(), branch.clone());
                let rows = self.with(move |c| {
                    c.query(
                        "SELECT holder FROM by_branch_locks WHERE repo = $1 AND branch = $2",
                        &[&repo, &name],
                    )
                })?;
                let holder = rows
                    .first()
                    .map(|r| r.get::<_, String>(0))
                    .unwrap_or_else(|| "another operation".to_owned());
                Ok(Admission::Busy { branch, holder })
            }
        }
    }

    fn unfinished(&self, tenant: &str) -> io::Result<Vec<StoredOperation>> {
        let tenant = tenant.to_owned();
        let rows = self.with(move |c| c.query(PG_UNFINISHED, &[&tenant, &DEFAULT_TENANT]))?;
        pg_ops(&rows)
    }

    fn claim(
        &self,
        worker: &Worker,
        repos: &[String],
        labels: &[String],
        lease: Duration,
    ) -> io::Result<Option<Claim>> {
        let worker = worker.clone();
        let repos = repos.to_vec();
        let labels = labels.to_vec();
        let lease = pg_lease(lease);
        let claimed = self.with(move |c| {
            // Claims whose process is gone from this host need not wait for
            // their lease.
            let local = c.query(
                "SELECT id, attempt, pid, start FROM by_operation_queue \
                 WHERE host = $1 AND worker IS NOT NULL AND worker <> $2 \
                   AND lease_until > clock_timestamp()",
                &[&worker.host, &worker.id],
            )?;
            for row in &local {
                let (id, attempt, pid, start): (String, i64, i64, String) =
                    (row.get(0), row.get(1), row.get(2), row.get(3));
                if branchyard::process_gone(&worker.host, pid as u32, &start) {
                    c.execute(
                        "UPDATE by_operation_queue SET worker = NULL, lease_until = NULL \
                         WHERE id = $1 AND attempt = $2",
                        &[&id, &attempt],
                    )?;
                }
            }
            let pid = i64::from(worker.pid);
            let rows = c.query(
                "UPDATE by_operation_queue q SET attempt = q.attempt + 1, worker = $2, \
                     host = $3, pid = $4, start = $5, \
                     lease_until = clock_timestamp() + $6::float8 * interval '1 millisecond' \
                 WHERE q.id = (SELECT id FROM by_operation_queue \
                     WHERE repo = ANY($1) \
                       AND (lease_until IS NULL OR lease_until <= clock_timestamp()) \
                       AND requires <@ $7::text[] \
                     ORDER BY seq FOR UPDATE SKIP LOCKED LIMIT 1) \
                 RETURNING q.id, q.attempt, q.work, \
                     (SELECT body FROM by_operations o WHERE o.id = q.id)",
                &[
                    &repos,
                    &worker.id,
                    &worker.host,
                    &pid,
                    &worker.start,
                    &(lease as f64),
                    &labels,
                ],
            )?;
            Ok(rows.first().map(|row| {
                let (id, fence, work, body): (String, i64, String, String) =
                    (row.get(0), row.get(1), row.get(2), row.get(3));
                (id, fence, work, body)
            }))
        })?;
        let Some((id, fence, work, body)) = claimed else {
            return Ok(None);
        };
        Ok(Some(Claim {
            operation: parse_op(&id, &body, "")?,
            work: serde_json::from_str(&work)?,
            fence,
        }))
    }

    fn beat(&self, worker: &Worker, labels: &[String], repos: &[String]) -> io::Result<()> {
        let worker = worker.clone();
        let (labels, repos) = (labels.to_vec(), repos.to_vec());
        self.with(move |c| {
            c.execute(
                "INSERT INTO by_workers (id, host, pid, labels, repos, seen) \
                 VALUES ($1, $2, $3, $4, $5, clock_timestamp()) ON CONFLICT (id) DO UPDATE SET \
                 labels = EXCLUDED.labels, repos = EXCLUDED.repos, seen = EXCLUDED.seen",
                &[
                    &worker.id,
                    &worker.host,
                    &i64::from(worker.pid),
                    &labels,
                    &repos,
                ],
            )
        })?;
        Ok(())
    }

    fn workers(&self, within: Duration) -> io::Result<Vec<LiveWorker>> {
        let within = ms(within) as f64;
        let rows = self.with(move |c| {
            c.query(
                "SELECT id, host, labels, repos, \
                     (EXTRACT(EPOCH FROM clock_timestamp() - seen) * 1000)::float8 \
                 FROM by_workers \
                 WHERE seen > clock_timestamp() - $1::float8 * interval '1 millisecond' \
                 ORDER BY id",
                &[&within],
            )
        })?;
        Ok(rows
            .iter()
            .map(|row| LiveWorker {
                id: row.get(0),
                host: row.get(1),
                labels: row.get(2),
                repos: row.get(3),
                seen_ms_ago: row.get::<_, f64>(4).max(0.0) as u64,
            })
            .collect())
    }

    fn leave(&self, worker: &Worker) -> io::Result<()> {
        let id = worker.id.clone();
        self.with(move |c| c.execute("DELETE FROM by_workers WHERE id = $1", &[&id]))?;
        Ok(())
    }

    fn renew(&self, worker: &Worker, id: &str, fence: i64, lease: Duration) -> io::Result<bool> {
        let (id, worker) = (id.to_owned(), worker.id.clone());
        let lease = pg_lease(lease) as f64;
        let changed = self.with(move |c| {
            c.execute(
                "UPDATE by_operation_queue \
                 SET lease_until = clock_timestamp() + $4::float8 * interval '1 millisecond' \
                 WHERE id = $1 AND attempt = $2 AND worker = $3",
                &[&id, &fence, &worker, &lease],
            )
        })?;
        Ok(changed == 1)
    }

    fn start(
        &self,
        worker: &Worker,
        fence: i64,
        operation: &StoredOperation,
        lease: Duration,
    ) -> io::Result<bool> {
        let body = serde_json::to_string(operation)?;
        let (id, worker) = (operation.operation.id.clone(), worker.id.clone());
        let lease = pg_lease(lease) as f64;
        self.with(move |c| {
            let mut tx = c.transaction()?;
            let held = tx.execute(
                "UPDATE by_operation_queue \
                 SET lease_until = clock_timestamp() + $4::float8 * interval '1 millisecond' \
                 WHERE id = $1 AND attempt = $2 AND worker = $3",
                &[&id, &fence, &worker, &lease],
            )?;
            if held != 1 {
                tx.rollback()?;
                return Ok(false);
            }
            tx.execute(
                "UPDATE by_operations SET body = $2 WHERE id = $1",
                &[&id, &body],
            )?;
            tx.commit()?;
            Ok(true)
        })
    }

    fn finish(&self, worker: &Worker, fence: i64, operation: &StoredOperation) -> io::Result<bool> {
        let body = serde_json::to_string(operation)?;
        let (id, worker) = (operation.operation.id.clone(), worker.id.clone());
        self.with(move |c| {
            let mut tx = c.transaction()?;
            let held = tx.execute(
                "DELETE FROM by_operation_queue WHERE id = $1 AND attempt = $2 AND worker = $3",
                &[&id, &fence, &worker],
            )?;
            if held != 1 {
                tx.rollback()?;
                return Ok(false);
            }
            tx.execute(
                "UPDATE by_operations SET body = $2 WHERE id = $1",
                &[&id, &body],
            )?;
            tx.execute("DELETE FROM by_branch_locks WHERE token = $1", &[&id])?;
            tx.commit()?;
            Ok(true)
        })
    }

    fn release(&self, worker: &Worker, id: &str, fence: i64) -> io::Result<()> {
        let (id, worker) = (id.to_owned(), worker.id.clone());
        self.with(move |c| {
            c.execute(
                "UPDATE by_operation_queue SET worker = NULL, lease_until = NULL \
                 WHERE id = $1 AND attempt = $2 AND worker = $3",
                &[&id, &fence, &worker],
            )
        })?;
        Ok(())
    }

    fn pending(&self, repos: &[String]) -> io::Result<usize> {
        let repos = repos.to_vec();
        let count: i64 = self.with(move |c| {
            c.query_one(
                "SELECT COUNT(*) FROM by_operation_queue WHERE repo = ANY($1)",
                &[&repos],
            )
            .map(|row| row.get(0))
        })?;
        Ok(count as usize)
    }

    fn hold(
        &self,
        repo: &str,
        branch: &str,
        holder: &str,
        token: &str,
        ttl: Duration,
    ) -> io::Result<Option<String>> {
        let (repo, branch) = (repo.to_owned(), branch.to_owned());
        let (holder, token) = (holder.to_owned(), token.to_owned());
        let ttl = pg_lease(ttl) as f64;
        self.with(move |c| {
            let mut tx = c.transaction()?;
            tx.execute(
                "DELETE FROM by_branch_locks WHERE repo = $1 AND branch = $2 \
                 AND expires_at IS NOT NULL AND expires_at <= clock_timestamp()",
                &[&repo, &branch],
            )?;
            let taken = tx.execute(
                "INSERT INTO by_branch_locks (repo, branch, holder, token, expires_at) \
                 VALUES ($1, $2, $3, $4, clock_timestamp() + $5::float8 * interval '1 millisecond') \
                 ON CONFLICT DO NOTHING",
                &[&repo, &branch, &holder, &token, &ttl],
            )?;
            if taken == 1 {
                tx.commit()?;
                return Ok(None);
            }
            let rows = tx.query(
                "SELECT holder FROM by_branch_locks WHERE repo = $1 AND branch = $2",
                &[&repo, &branch],
            )?;
            tx.rollback()?;
            Ok(Some(
                rows.first()
                    .map(|r| r.get::<_, String>(0))
                    .unwrap_or_else(|| "another operation".to_owned()),
            ))
        })
    }

    fn unhold(&self, repo: &str, branch: &str, token: &str) -> io::Result<()> {
        let (repo, branch, token) = (repo.to_owned(), branch.to_owned(), token.to_owned());
        self.with(move |c| {
            c.execute(
                "DELETE FROM by_branch_locks WHERE repo = $1 AND branch = $2 AND token = $3",
                &[&repo, &branch, &token],
            )
        })?;
        Ok(())
    }

    fn reset(&self) -> io::Result<()> {
        Err(io::Error::other(
            "a PostgreSQL registry may be shared by several servers; its claims expire instead",
        ))
    }

    fn load_webhook_cursor(&self, id: &str) -> io::Result<Option<u64>> {
        let id = id.to_owned();
        let rows = self.with(move |c| {
            c.query(
                "SELECT cursor FROM by_webhook_cursors WHERE id = $1",
                &[&id],
            )
        })?;
        Ok(rows.first().map(|row| {
            let cursor: i64 = row.get(0);
            cursor as u64
        }))
    }

    fn save_webhook_cursor(&self, id: &str, cursor: u64) -> io::Result<()> {
        let id = id.to_owned();
        let cursor = cursor as i64;
        self.with(move |c| {
            c.execute(
                "INSERT INTO by_webhook_cursors (id, cursor) VALUES ($1, $2) \
                 ON CONFLICT (id) DO UPDATE SET cursor = EXCLUDED.cursor",
                &[&id, &cursor],
            )
        })?;
        Ok(())
    }
}

/// The worker-label conformance every [`OperationStore`] passes: a worker
/// claims only operations whose required labels it all carries; one
/// requiring nothing is anyone's; two workers racing for labeled work
/// claim each operation once, and a worker without the label none of it;
/// beats are seen by [`OperationStore::workers`], and a leaving worker is
/// not. `repo` names a repository no other test of the store uses. Panics
/// on a violation. Run on SQLite here and on PostgreSQL by
/// `tests/postgres.rs`.
#[doc(hidden)]
pub fn check_labels(store: &dyn OperationStore, repo: &str) {
    use branchyard_client::api::{OperationKind, OperationState};
    const LEASE: Duration = Duration::from_secs(30);
    let stored = |id: &str, requires: &[&str]| StoredOperation {
        operation: Operation {
            id: id.into(),
            repo: repo.into(),
            kind: OperationKind::Task,
            state: OperationState::Queued,
            branches: Vec::new(),
            cursor: 0,
            end_cursor: None,
            created_at_ms: 1,
            finished_at_ms: None,
            result: None,
            error: None,
            requires: requires.iter().map(|l| l.to_string()).collect(),
            waiting: None,
        },
        idempotency: None,
        locks: Vec::new(),
        tenant: String::new(),
        principal: None,
        creates: Vec::new(),
    };
    let admit = |id: &str, requires: &[&str]| {
        let admitted = store
            .admit(
                &stored(&format!("{repo}-{id}"), requires),
                &serde_json::json!({}),
                &AdmissionQuota::default(),
            )
            .unwrap();
        assert_eq!(admitted, Admission::Admitted);
    };
    let worker = |id: &str| Worker {
        id: format!("{repo}-{id}"),
        ..Worker::current()
    };
    let labels = |list: &[&str]| list.iter().map(|l| l.to_string()).collect::<Vec<_>>();
    let repos = vec![repo.to_owned()];
    let claimed = |claim: Option<Claim>| claim.map(|c| c.operation.operation.id);
    admit("gpu", &["gpu"]);
    admit("any", &[]);
    admit("both", &["gpu", "linux"]);
    // Oldest first among what each may claim.
    let plain = worker("plain");
    assert_eq!(
        claimed(store.claim(&plain, &repos, &[], LEASE).unwrap()),
        Some(format!("{repo}-any"))
    );
    assert_eq!(
        claimed(
            store
                .claim(&plain, &repos, &labels(&["linux"]), LEASE)
                .unwrap()
        ),
        None
    );
    let gpu = worker("gpu");
    assert_eq!(
        claimed(store.claim(&gpu, &repos, &labels(&["gpu"]), LEASE).unwrap()),
        Some(format!("{repo}-gpu"))
    );
    assert_eq!(
        claimed(store.claim(&gpu, &repos, &labels(&["gpu"]), LEASE).unwrap()),
        None
    );
    let both = worker("both");
    assert_eq!(
        claimed(
            store
                .claim(&both, &repos, &labels(&["linux", "x", "gpu"]), LEASE)
                .unwrap()
        ),
        Some(format!("{repo}-both"))
    );

    // Two labeled workers race for eight labeled operations while an
    // unlabeled one tries too.
    for n in 0..8 {
        admit(&format!("race-{n}"), &["gpu"]);
    }
    let racers = [worker("racer-a"), worker("racer-b")];
    let outsider = worker("outsider");
    let (mut won, outside) = std::thread::scope(|scope| {
        let handles: Vec<_> = racers
            .iter()
            .map(|w| {
                let repos = repos.clone();
                scope.spawn(move || {
                    let mut got = Vec::new();
                    while let Some(claim) =
                        store.claim(w, &repos, &labels(&["gpu"]), LEASE).unwrap()
                    {
                        got.push(claim.operation.operation.id);
                    }
                    got
                })
            })
            .collect();
        let outside = scope.spawn(|| {
            let mut got = Vec::new();
            for _ in 0..20 {
                if let Some(claim) = store.claim(&outsider, &repos, &[], LEASE).unwrap() {
                    got.push(claim.operation.operation.id);
                }
            }
            got
        });
        let won: Vec<String> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        (won, outside.join().unwrap())
    });
    assert!(outside.is_empty(), "{outside:?}");
    won.sort();
    let mut expected: Vec<String> = (0..8).map(|n| format!("{repo}-race-{n}")).collect();
    expected.sort();
    assert_eq!(won, expected, "each operation claimed exactly once");

    // Beats.
    store.beat(&gpu, &labels(&["gpu"]), &repos).unwrap();
    store.beat(&plain, &[], &repos).unwrap();
    let live = store.workers(Duration::from_secs(60)).unwrap();
    let mine: Vec<&LiveWorker> = live.iter().filter(|w| w.id.starts_with(repo)).collect();
    assert_eq!(mine.len(), 2, "{live:?}");
    let seen = mine.iter().find(|w| w.id == gpu.id).unwrap();
    assert_eq!(seen.labels, ["gpu"]);
    assert_eq!(seen.repos, repos);
    assert!(seen.seen_ms_ago < 60_000);
    let reason = unclaimable(&labels(&["gpu", "linux"]), repo, &live).unwrap();
    assert!(
        reason.contains("gpu, linux") && reason.contains(&gpu.id),
        "{reason}"
    );
    assert_eq!(unclaimable(&labels(&["gpu"]), repo, &live), None);
    store.leave(&gpu).unwrap();
    let live = store.workers(Duration::from_secs(60)).unwrap();
    assert!(!live.iter().any(|w| w.id == gpu.id));
    assert!(unclaimable(&labels(&["gpu"]), repo, &live).is_some());
}

/// Operations in memory only, for tests and embedding: a [`SqliteStore`]
/// on an in-memory database.
pub struct MemoryStore(SqliteStore);

impl Default for MemoryStore {
    fn default() -> MemoryStore {
        MemoryStore(SqliteStore::memory())
    }
}

impl std::ops::Deref for MemoryStore {
    type Target = SqliteStore;
    fn deref(&self) -> &SqliteStore {
        &self.0
    }
}

macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        $(fn $name(&self, $($arg: $ty),*) -> $ret { self.0.$name($($arg),*) })*
    };
}

impl OperationStore for MemoryStore {
    forward! {
        load() -> io::Result<Vec<StoredOperation>>;
        get(id: &str) -> io::Result<Option<StoredOperation>>;
        by_key(caller: &str, key: &str) -> io::Result<Option<StoredOperation>>;
        save(operation: &StoredOperation) -> io::Result<()>;
        orphans() -> io::Result<Vec<StoredOperation>>;
        admit(operation: &StoredOperation, work: &Value, quota: &AdmissionQuota)
            -> io::Result<Admission>;
        unfinished(tenant: &str) -> io::Result<Vec<StoredOperation>>;
        claim(worker: &Worker, repos: &[String], labels: &[String], lease: Duration)
            -> io::Result<Option<Claim>>;
        beat(worker: &Worker, labels: &[String], repos: &[String]) -> io::Result<()>;
        workers(within: Duration) -> io::Result<Vec<LiveWorker>>;
        leave(worker: &Worker) -> io::Result<()>;
        renew(worker: &Worker, id: &str, fence: i64, lease: Duration) -> io::Result<bool>;
        start(worker: &Worker, fence: i64, operation: &StoredOperation, lease: Duration)
            -> io::Result<bool>;
        finish(worker: &Worker, fence: i64, operation: &StoredOperation) -> io::Result<bool>;
        release(worker: &Worker, id: &str, fence: i64) -> io::Result<()>;
        pending(repos: &[String]) -> io::Result<usize>;
        hold(repo: &str, branch: &str, holder: &str, token: &str, ttl: Duration)
            -> io::Result<Option<String>>;
        unhold(repo: &str, branch: &str, token: &str) -> io::Result<()>;
        reset() -> io::Result<()>;
        load_webhook_cursor(id: &str) -> io::Result<Option<u64>>;
        save_webhook_cursor(id: &str, cursor: u64) -> io::Result<()>;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_client::api::{OperationKind, OperationState};

    fn op(id: &str, state: OperationState) -> StoredOperation {
        StoredOperation {
            operation: Operation {
                id: id.into(),
                repo: "r".into(),
                kind: OperationKind::Task,
                state,
                branches: vec!["b".into()],
                cursor: 0,
                end_cursor: None,
                created_at_ms: 1,
                finished_at_ms: None,
                result: None,
                error: None,
                requires: Vec::new(),
                waiting: None,
            },
            idempotency: None,
            locks: Vec::new(),
            tenant: String::new(),
            principal: None,
            creates: Vec::new(),
        }
    }

    /// A fresh directory, removed when the returned guard is dropped.
    fn temp(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("branchyard-{name}-"))
            .tempdir()
            .unwrap()
    }

    #[test]
    fn the_latest_save_wins_and_survives_reopening() {
        let temp = temp("store");
        let dir = temp.path();
        let path = dir.join("operations.jsonl");
        let store = FileStore::open(&path).unwrap();
        store.save(&op("a", OperationState::Queued)).unwrap();
        store.save(&op("b", OperationState::Queued)).unwrap();
        store.save(&op("a", OperationState::Succeeded)).unwrap();
        drop(store);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{{\"operation\":").unwrap();
        drop(file);
        let store = FileStore::open(&path).unwrap();
        let ops = store.load().unwrap();
        let states: Vec<_> = ops
            .iter()
            .map(|o| (o.operation.id.as_str(), o.operation.state))
            .collect();
        assert_eq!(
            states,
            [
                ("a", OperationState::Succeeded),
                ("b", OperationState::Queued)
            ]
        );
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
    }

    #[test]
    fn webhook_cursors_round_trip_and_only_ever_move_as_told() {
        let temp = temp("webhook-cursor");
        let dir = temp.path();
        let store = SqliteStore::open(dir.join("state.db"), None).unwrap();
        assert_eq!(store.load_webhook_cursor("repo:hook").unwrap(), None);
        store.save_webhook_cursor("repo:hook", 5).unwrap();
        assert_eq!(store.load_webhook_cursor("repo:hook").unwrap(), Some(5));
        store.save_webhook_cursor("repo:hook", 12).unwrap();
        assert_eq!(store.load_webhook_cursor("repo:hook").unwrap(), Some(12));
        // A distinct id has its own cursor.
        assert_eq!(store.load_webhook_cursor("repo:other").unwrap(), None);
        drop(store);
        // Durable: reopening finds it again.
        let store = SqliteStore::open(dir.join("state.db"), None).unwrap();
        assert_eq!(store.load_webhook_cursor("repo:hook").unwrap(), Some(12));

        let memory = MemoryStore::default();
        assert_eq!(memory.load_webhook_cursor("h").unwrap(), None);
        memory.save_webhook_cursor("h", 3).unwrap();
        assert_eq!(memory.load_webhook_cursor("h").unwrap(), Some(3));
    }

    #[test]
    fn sqlite_imports_the_file_once_and_keeps_order_and_the_latest_save() {
        let temp = temp("sqlite-store");
        let dir = temp.path();
        let legacy = dir.join("operations.jsonl");
        let file = FileStore::open(&legacy).unwrap();
        file.save(&op("a", OperationState::Queued)).unwrap();
        file.save(&op("b", OperationState::Succeeded)).unwrap();
        drop(file);
        let db = dir.join("state.db");
        let store = SqliteStore::open(&db, Some(&legacy)).unwrap();
        assert!(!legacy.exists());
        assert!(dir.join("operations.jsonl.imported").is_file());
        // The imported queued operation has no queue row.
        let orphans = store.orphans().unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].operation.id, "a");
        store.save(&op("a", OperationState::Interrupted)).unwrap();
        store.save(&op("c", OperationState::Succeeded)).unwrap();
        drop(store);
        let store = SqliteStore::open(&db, Some(&legacy)).unwrap();
        let states: Vec<_> = store
            .load()
            .unwrap()
            .iter()
            .map(|o| (o.operation.id.clone(), o.operation.state))
            .collect();
        assert_eq!(
            states,
            [
                ("a".to_owned(), OperationState::Interrupted),
                ("b".to_owned(), OperationState::Succeeded),
                ("c".to_owned(), OperationState::Succeeded),
            ]
        );
        assert!(store.orphans().unwrap().is_empty());
    }

    fn keyed(id: &str, key: &str, locks: &[&str]) -> StoredOperation {
        let mut stored = op(id, OperationState::Queued);
        stored.idempotency = Some(Idempotency {
            caller: "c".into(),
            key: key.into(),
            fingerprint: "f".into(),
        });
        stored.locks = locks.iter().map(|s| s.to_string()).collect();
        stored
    }

    fn worker(id: &str) -> Worker {
        Worker {
            id: id.into(),
            ..Worker::current()
        }
    }

    const LEASE: Duration = Duration::from_secs(30);

    #[test]
    fn admission_binds_the_key_takes_locks_and_enqueues_together() {
        let store = MemoryStore::default();
        let work = serde_json::json!({ "kind": "test" });
        assert_eq!(
            store
                .admit(
                    &keyed("a", "k", &["x", "y"]),
                    &work,
                    &AdmissionQuota::default()
                )
                .unwrap(),
            Admission::Admitted
        );
        // The same key replays, whatever else differs.
        match store
            .admit(&keyed("b", "k", &[]), &work, &AdmissionQuota::default())
            .unwrap()
        {
            Admission::Replayed(existing) => assert_eq!(existing.operation.id, "a"),
            other => panic!("{other:?}"),
        }
        // A held branch refuses the whole admission.
        assert_eq!(
            store
                .admit(
                    &keyed("c", "k2", &["z", "y"]),
                    &work,
                    &AdmissionQuota::default()
                )
                .unwrap(),
            Admission::Busy {
                branch: "y".into(),
                holder: "a".into()
            }
        );
        assert!(store.get("c").unwrap().is_none());
        assert_eq!(store.hold("r", "z", "a removal", "t", LEASE).unwrap(), None);
        store.unhold("r", "z", "t").unwrap();
        assert_eq!(store.pending(&["r".into()]).unwrap(), 1);
        assert_eq!(store.pending(&["other".into()]).unwrap(), 0);

        // Claimed once; a second worker finds nothing claimable.
        let (one, two) = (worker("one"), worker("two"));
        let claim = store
            .claim(&one, &["r".into()], &[], LEASE)
            .unwrap()
            .unwrap();
        assert_eq!(
            (claim.operation.operation.id.as_str(), claim.fence),
            ("a", 1)
        );
        assert_eq!(claim.work, work);
        assert!(store
            .claim(&two, &["r".into()], &[], LEASE)
            .unwrap()
            .is_none());
        assert!(store.renew(&one, "a", 1, LEASE).unwrap());
        assert!(!store.renew(&two, "a", 1, LEASE).unwrap());
        let mut done = claim.operation.clone();
        done.operation.state = OperationState::Succeeded;
        assert!(!store.finish(&two, 1, &done).unwrap());
        assert!(store.finish(&one, 1, &done).unwrap());
        assert_eq!(store.pending(&["r".into()]).unwrap(), 0);
        assert_eq!(
            store.get("a").unwrap().unwrap().operation.state,
            OperationState::Succeeded
        );
        // Its locks went with it.
        assert_eq!(
            store
                .admit(
                    &keyed("c", "k2", &["x", "y"]),
                    &work,
                    &AdmissionQuota::default()
                )
                .unwrap(),
            Admission::Admitted
        );
    }

    #[test]
    fn workers_claim_only_what_their_labels_allow_on_sqlite() {
        check_labels(&MemoryStore::default(), "mem");
        let temp = temp("labels");
        let path = temp.path().join("state.db");
        check_labels(&SqliteStore::open(&path, None).unwrap(), "file");
        // An older queue without the column gains it, its rows requiring
        // nothing.
        let old = temp.path().join("old.db");
        let conn = rusqlite::Connection::open(&old).unwrap();
        conn.execute_batch(
            "CREATE TABLE operation_queue (id TEXT PRIMARY KEY, seq INTEGER NOT NULL, \
             repo TEXT NOT NULL, work TEXT NOT NULL, attempt INTEGER NOT NULL DEFAULT 0, \
             worker TEXT, host TEXT, pid INTEGER, start TEXT, lease_until INTEGER); \
             INSERT INTO operation_queue (id, seq, repo, work) VALUES ('x', 1, 'r', '{}');",
        )
        .unwrap();
        drop(conn);
        let store = SqliteStore::open(&old, None).unwrap();
        let requires: String = store
            .conn()
            .query_row("SELECT requires FROM operation_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(requires, "[]");
    }

    #[test]
    fn stores_opened_at_once_on_an_older_file_migrate_it_once() {
        let temp = temp("open-together");
        for (n, old) in [false, true].into_iter().enumerate() {
            let path = temp.path().join(format!("state-{n}.db"));
            if old {
                rusqlite::Connection::open(&path)
                    .unwrap()
                    .execute_batch(
                        "CREATE TABLE operation_queue (id TEXT PRIMARY KEY, \
                         seq INTEGER NOT NULL, repo TEXT NOT NULL, work TEXT NOT NULL, \
                         attempt INTEGER NOT NULL DEFAULT 0, worker TEXT, host TEXT, \
                         pid INTEGER, start TEXT, lease_until INTEGER); \
                         INSERT INTO operation_queue (id, seq, repo, work) \
                         VALUES ('x', 1, 'r', '{}');",
                    )
                    .unwrap();
            }
            let barrier = std::sync::Barrier::new(8);
            std::thread::scope(|scope| {
                let opens: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            SqliteStore::open(&path, None).map(drop)
                        })
                    })
                    .collect();
                for open in opens {
                    open.join()
                        .unwrap()
                        .map_err(|e| format!("old={old}: {e}"))
                        .unwrap();
                }
            });
            let store = SqliteStore::open(&path, None).unwrap();
            if old {
                let requires: String = store
                    .conn()
                    .query_row("SELECT requires FROM operation_queue", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(requires, "[]");
            }
        }
    }

    #[test]
    fn labels_are_checked_and_unclaimable_work_says_why() {
        for good in ["gpu", "linux", "x86_64", "a.b-c", "9"] {
            assert!(valid_label(good), "{good}");
        }
        for bad in ["", "GPU", "-x", "a b", "a,b", &"x".repeat(64)] {
            assert!(!valid_label(bad), "{bad}");
        }
        assert_eq!(unclaimable(&[], "r", &[]), None);
        let reason = unclaimable(&["gpu".into()], "r", &[]).unwrap();
        assert!(reason.contains("no live worker serves r"), "{reason}");
    }

    #[test]
    fn an_expired_claim_is_claimed_again_under_a_new_fence() {
        let store = MemoryStore::default();
        store
            .admit(
                &keyed("a", "k", &["x"]),
                &serde_json::json!({}),
                &AdmissionQuota::default(),
            )
            .unwrap();
        let (one, two) = (worker("one"), worker("two"));
        let repos = ["r".to_owned()];
        let first = store
            .claim(&one, &repos, &[], Duration::from_millis(1))
            .unwrap()
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let second = store.claim(&two, &repos, &[], LEASE).unwrap().unwrap();
        assert_eq!(second.fence, first.fence + 1);
        // The first worker is fenced out of every write.
        assert!(!store.renew(&one, "a", first.fence, LEASE).unwrap());
        assert!(!store
            .start(&one, first.fence, &first.operation, LEASE)
            .unwrap());
        assert!(!store.finish(&one, first.fence, &first.operation).unwrap());
        // Given back unstarted, another claim follows at once.
        store.release(&two, "a", second.fence).unwrap();
        let third = store.claim(&one, &repos, &[], LEASE).unwrap().unwrap();
        assert_eq!(third.fence, second.fence + 1);
        // A reset store (its only process restarted) frees every claim.
        store.reset().unwrap();
        assert!(store.claim(&two, &repos, &[], LEASE).unwrap().is_some());
    }

    #[test]
    fn a_failed_queue_write_rolls_the_admission_back() {
        let store = MemoryStore::default();
        store
            .conn()
            .execute_batch(
                "CREATE TRIGGER fail_enqueue BEFORE INSERT ON operation_queue \
                 BEGIN SELECT RAISE(ABORT, 'injected queue failure'); END;",
            )
            .unwrap();
        let error = store
            .admit(
                &keyed("a", "k", &["x"]),
                &serde_json::json!({}),
                &AdmissionQuota::default(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("injected"), "{error}");
        assert!(store.get("a").unwrap().is_none());
        assert!(store.by_key("c", "k").unwrap().is_none());
        assert_eq!(store.hold("r", "x", "a removal", "t", LEASE).unwrap(), None);
        assert_eq!(store.pending(&["r".into()]).unwrap(), 0);
    }

    fn in_tenant(id: &str, tenant: &str, locks: &[&str], creates: &[&str]) -> StoredOperation {
        let mut stored = keyed(id, &format!("key-{id}"), locks);
        stored.tenant = tenant.into();
        stored.creates = creates.iter().map(|s| s.to_string()).collect();
        stored
    }

    #[test]
    fn a_quota_refusal_writes_nothing_and_terminal_operations_stop_counting() {
        let store = MemoryStore::default();
        let work = serde_json::json!({});
        let quota = AdmissionQuota {
            max_running: Some(1),
            ..AdmissionQuota::default()
        };
        assert_eq!(
            store
                .admit(&in_tenant("a", "acme", &["x"], &[]), &work, &quota)
                .unwrap(),
            Admission::Admitted
        );
        assert_eq!(
            store
                .admit(&in_tenant("b", "acme", &["y"], &[]), &work, &quota)
                .unwrap(),
            Admission::Quota {
                limit: "max_running",
                max: 1,
                reserved: 1
            }
        );
        // Nothing of the refused admission was written: no record, no key
        // binding, no queue row, no lock.
        assert!(store.get("b").unwrap().is_none());
        assert!(store.by_key("c", "key-b").unwrap().is_none());
        assert_eq!(store.pending(&["r".into()]).unwrap(), 1);
        assert_eq!(store.hold("r", "y", "a removal", "t", LEASE).unwrap(), None);
        store.unhold("r", "y", "t").unwrap();
        // Another tenant, and a record from before tenants (the default
        // tenant's), count on their own.
        assert_eq!(
            store
                .admit(&in_tenant("c", "other", &[], &[]), &work, &quota)
                .unwrap(),
            Admission::Admitted
        );
        assert_eq!(store.unfinished("acme").unwrap().len(), 1);
        assert_eq!(store.unfinished("other").unwrap().len(), 1);
        assert!(store.unfinished(DEFAULT_TENANT).unwrap().is_empty());
        // Finished: it no longer counts.
        let one = worker("one");
        let claim = store
            .claim(&one, &["r".into()], &[], LEASE)
            .unwrap()
            .unwrap();
        assert_eq!(claim.operation.operation.id, "a");
        let mut done = claim.operation.clone();
        done.operation.state = OperationState::Succeeded;
        assert!(store.finish(&one, claim.fence, &done).unwrap());
        assert_eq!(
            store
                .admit(&in_tenant("b", "acme", &["y"], &[]), &work, &quota)
                .unwrap(),
            Admission::Admitted
        );
    }

    #[test]
    fn max_branches_counts_existing_and_planned_branches_once() {
        let store = MemoryStore::default();
        let work = serde_json::json!({});
        let quota = || AdmissionQuota {
            max_branches: Some(3),
            existing_branches: Some(Box::new(|| {
                Ok([("r".to_owned(), "old".to_owned())].into_iter().collect())
            })),
            ..AdmissionQuota::default()
        };
        assert_eq!(
            store
                .admit(&in_tenant("a", "acme", &[], &["one"]), &work, &quota())
                .unwrap(),
            Admission::Admitted
        );
        // Two more would make four: refused, with the three reserved.
        assert_eq!(
            store
                .admit(
                    &in_tenant("b", "acme", &[], &["two", "three"]),
                    &work,
                    &quota()
                )
                .unwrap(),
            Admission::Quota {
                limit: "max_branches",
                max: 3,
                reserved: 2
            }
        );
        assert!(store.get("b").unwrap().is_none());
        // A branch that already exists is not counted twice.
        assert_eq!(
            store
                .admit(
                    &in_tenant("c", "acme", &[], &["old", "two"]),
                    &work,
                    &quota()
                )
                .unwrap(),
            Admission::Admitted
        );
        // An operation that creates nothing is not refused.
        assert_eq!(
            store
                .admit(&in_tenant("d", "acme", &[], &[]), &work, &quota())
                .unwrap(),
            Admission::Admitted
        );
    }
}
