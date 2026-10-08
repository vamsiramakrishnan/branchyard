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

use branchyard_support::LockExt as _;
use std::fmt;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use postgres::error::SqlState;
use postgres::{Client, NoTls, Row, Transaction};
use serde_json::Value;

use crate::graph::{Dependency, GraphBackend, GraphCommit};
use crate::state::{
    pick_port, Acquired, Backend, Begun, FeedRow, Fence, LeaseRow, Owner, PoolBackend, PortBackend,
    ProcessRow, Record, ReservationRow, SandboxBackend, SandboxKind, SandboxRow, SlotRow,
    SlotState, SteerRow, StepRow,
};
use crate::storage::{
    ArtifactRef, ArtifactRow, Identity, LegacyBinder, LegacyBranch, LockOutcome, NewArtifact,
    NewScratch, ScratchArea, ScratchLock, ScratchRow, Share, StorageBackend,
};
use crate::store_codec::{
    deadline, decode, encode, from_db, from_db_opt, from_db_u16, from_db_u32, parse_text, to_db,
    to_db_opt, to_db_usize,
};
use crate::{Activity, BranchStatus, Error, Message, RecordedEvent, SteerState};
use branchyard_support::time::now_ms;

/// 2: grants bound to incarnations (see `crate::storage::LegacyBinder`).
const SCHEMA: i64 = 2;
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
    parent_incarnation BIGINT,
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
    publisher_incarnation BIGINT,
    ancestry_incarnations TEXT NOT NULL DEFAULT '[]',
    UNIQUE (repo, id)
);
CREATE TABLE IF NOT EXISTS by_artifact_shares (
    repo TEXT NOT NULL,
    id TEXT NOT NULL,
    branch TEXT NOT NULL,
    incarnation BIGINT,
    PRIMARY KEY (repo, id, branch)
);
CREATE TABLE IF NOT EXISTS by_scratch_areas (
    repo TEXT NOT NULL,
    name TEXT NOT NULL,
    owner TEXT NOT NULL,
    ancestry TEXT NOT NULL,
    created_ms BIGINT NOT NULL,
    owner_incarnation BIGINT,
    ancestry_incarnations TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (repo, name)
);
CREATE TABLE IF NOT EXISTS by_scratch_shares (
    repo TEXT NOT NULL,
    name TEXT NOT NULL,
    branch TEXT NOT NULL,
    incarnation BIGINT,
    PRIMARY KEY (repo, name, branch)
);
CREATE TABLE IF NOT EXISTS by_scratch_locks (
    repo TEXT NOT NULL,
    name TEXT NOT NULL,
    holder TEXT NOT NULL,
    acquired_ms BIGINT NOT NULL,
    holder_incarnation BIGINT,
    PRIMARY KEY (repo, name)
);
CREATE TABLE IF NOT EXISTS by_messages (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repo TEXT NOT NULL,
    from_branch TEXT NOT NULL,
    to_branch TEXT NOT NULL,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    in_reply_to BIGINT,
    at_ms BIGINT NOT NULL,
    delivered_ms BIGINT,
    steer_id BIGINT,
    delivered_steer BIGINT,
    awaiting_until_ms BIGINT
);
CREATE INDEX IF NOT EXISTS by_messages_to ON by_messages (repo, to_branch, id);
CREATE INDEX IF NOT EXISTS by_messages_steer ON by_messages (steer_id);
CREATE INDEX IF NOT EXISTS by_messages_from ON by_messages (repo, from_branch, kind);
CREATE INDEX IF NOT EXISTS by_messages_reply ON by_messages (repo, in_reply_to);
CREATE TABLE IF NOT EXISTS by_graph_revisions (
    repo TEXT NOT NULL,
    parent TEXT NOT NULL,
    revision BIGINT NOT NULL,
    PRIMARY KEY (repo, parent)
);
CREATE TABLE IF NOT EXISTS by_graph_edges (
    repo TEXT NOT NULL,
    parent TEXT NOT NULL,
    dependent TEXT NOT NULL,
    prerequisite TEXT NOT NULL,
    after TEXT NOT NULL,
    PRIMARY KEY (repo, dependent, prerequisite)
);
CREATE INDEX IF NOT EXISTS by_graph_edges_prerequisite ON by_graph_edges (repo, prerequisite);
CREATE INDEX IF NOT EXISTS by_graph_edges_parent ON by_graph_edges (repo, parent);
CREATE TABLE IF NOT EXISTS by_ports (
    port INTEGER PRIMARY KEY,
    repo TEXT NOT NULL,
    branch TEXT NOT NULL,
    reserved_ms BIGINT NOT NULL,
    UNIQUE (repo, branch)
);
CREATE TABLE IF NOT EXISTS by_sandboxes (
    repo TEXT NOT NULL,
    branch TEXT NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    incarnation BIGINT NOT NULL,
    provider TEXT NOT NULL,
    turn BIGINT,
    detail TEXT NOT NULL,
    used_ms BIGINT NOT NULL,
    PRIMARY KEY (repo, branch, kind, name)
);
CREATE INDEX IF NOT EXISTS by_sandboxes_provider ON by_sandboxes (repo, kind, provider, used_ms);
CREATE TABLE IF NOT EXISTS by_outcomes (
    repo TEXT NOT NULL,
    id TEXT NOT NULL,
    repository TEXT NOT NULL,
    branch TEXT NOT NULL,
    kind TEXT NOT NULL,
    harness TEXT NOT NULL,
    model TEXT,
    effort TEXT,
    outcome TEXT NOT NULL,
    score DOUBLE PRECISION,
    cost_usd DOUBLE PRECISION,
    duration_ms BIGINT,
    turns BIGINT NOT NULL,
    routed BOOLEAN NOT NULL,
    recorded_ms BIGINT NOT NULL,
    PRIMARY KEY (repo, id)
);
CREATE INDEX IF NOT EXISTS by_outcomes_kind ON by_outcomes (repo, kind, recorded_ms);
";

/// Tables added after [`TABLES`], each made on its own when the catalog
/// says it is missing: a name (a table or index), and the statement that
/// makes it. An opener that finds them all reads only the catalog, so it
/// takes no lock another engine's transaction could wait behind; one that
/// finds one missing makes it alone, under the schema's advisory lock,
/// after checking again.
const STEPS: &[(&str, &str)] = &[
    (
        "by_knowledge",
        "CREATE TABLE IF NOT EXISTS by_knowledge (
            id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            repo TEXT NOT NULL,
            scope_path TEXT,
            scope_kind TEXT,
            text TEXT NOT NULL,
            source TEXT NOT NULL,
            status TEXT NOT NULL,
            created_ms BIGINT NOT NULL,
            adopted_by TEXT,
            decided_ms BIGINT,
            note TEXT
        )",
    ),
    (
        "by_knowledge_repo",
        "CREATE INDEX IF NOT EXISTS by_knowledge_repo ON by_knowledge (repo, id)",
    ),
    (
        "by_pool_slots",
        "CREATE TABLE IF NOT EXISTS by_pool_slots (
            repo TEXT NOT NULL,
            id TEXT NOT NULL,
            place TEXT NOT NULL,
            recipe TEXT NOT NULL,
            state TEXT NOT NULL,
            base TEXT NOT NULL,
            path TEXT NOT NULL,
            detail TEXT NOT NULL,
            host TEXT NOT NULL,
            pid BIGINT NOT NULL,
            pid_start TEXT NOT NULL,
            branch TEXT,
            created_ms BIGINT NOT NULL,
            changed_ms BIGINT NOT NULL,
            PRIMARY KEY (repo, id)
        )",
    ),
    (
        "by_model_usage",
        "CREATE TABLE IF NOT EXISTS by_model_usage (
            repo TEXT NOT NULL,
            id TEXT NOT NULL,
            at_ms BIGINT NOT NULL,
            branch TEXT NOT NULL,
            turn BIGINT NOT NULL,
            subject TEXT NOT NULL,
            model TEXT NOT NULL,
            api TEXT NOT NULL,
            backend TEXT NOT NULL,
            input BIGINT NOT NULL,
            output BIGINT NOT NULL,
            cache_read BIGINT NOT NULL,
            cache_write BIGINT NOT NULL,
            cache_write_1h BIGINT NOT NULL,
            cost_usd DOUBLE PRECISION,
            latency_ms BIGINT NOT NULL,
            status BIGINT NOT NULL,
            streamed BOOLEAN NOT NULL,
            PRIMARY KEY (repo, id)
        )",
    ),
    (
        "by_pool_slots_place",
        "CREATE INDEX IF NOT EXISTS by_pool_slots_place ON by_pool_slots (repo, place, created_ms)",
    ),
    (
        "by_model_usage_at",
        "CREATE INDEX IF NOT EXISTS by_model_usage_at ON by_model_usage (repo, at_ms)",
    ),
    (
        "by_effects",
        "CREATE TABLE IF NOT EXISTS by_effects (
            repo TEXT NOT NULL,
            id TEXT NOT NULL,
            branch TEXT NOT NULL,
            state TEXT NOT NULL,
            created_ms BIGINT NOT NULL,
            entry TEXT NOT NULL,
            PRIMARY KEY (repo, id)
        )",
    ),
    (
        "by_effects_branch",
        "CREATE INDEX IF NOT EXISTS by_effects_branch ON by_effects (repo, branch, created_ms)",
    ),
    (
        "by_effect_events",
        "CREATE TABLE IF NOT EXISTS by_effect_events (
            seq BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            repo TEXT NOT NULL,
            id TEXT NOT NULL,
            at_ms BIGINT NOT NULL,
            change TEXT NOT NULL
        )",
    ),
    (
        "by_effect_events_id",
        "CREATE INDEX IF NOT EXISTS by_effect_events_id ON by_effect_events (repo, id, seq)",
    ),
    (
        "by_approval_asks",
        "CREATE TABLE IF NOT EXISTS by_approval_asks (
            repo TEXT NOT NULL,
            id TEXT NOT NULL,
            branch TEXT NOT NULL,
            created_ms BIGINT NOT NULL,
            answered BOOLEAN NOT NULL,
            ask TEXT NOT NULL,
            PRIMARY KEY (repo, id)
        )",
    ),
    (
        "by_approval_asks_pending",
        "CREATE INDEX IF NOT EXISTS by_approval_asks_pending ON by_approval_asks \
         (repo, answered, created_ms)",
    ),
];

fn steer_row(r: &Row) -> Result<SteerRow, Error> {
    Ok(SteerRow {
        id: from_db("id", r.get::<_, i64>(0))?,
        branch: r.get(1),
        turn: from_db("turn", r.get::<_, i64>(2))?,
        by: r.get(3),
        text: r.get(4),
        requested_ms: from_db("requested_ms", r.get::<_, i64>(5))?,
        state: SteerState::from_columns(&r.get::<_, String>(6), r.get(7))?,
        message: from_db_opt("message", r.get::<_, Option<i64>>(8))?,
        message_delivered: r.get::<_, Option<bool>>(9).unwrap_or(false),
    })
}

/// The columns [`steer_row`] reads, from `by_steers` as `s`. Steer ids are
/// unique across the database, so a message links to one without a repo.
const STEER_COLUMNS: &str = "s.id, s.branch, s.turn, s.requested_by, s.text, s.at_ms, s.state, \
     s.reason, (SELECT m.id FROM by_messages m WHERE m.steer_id = s.id), \
     (SELECT m.delivered_ms IS NOT NULL FROM by_messages m WHERE m.steer_id = s.id)";

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
///
/// `url::Url` does the work when it can, but a password is not always
/// something it can see: a multi-host URL (`h1:5432,h2:5432`) does not parse,
/// and a password with a raw `/`, `#` or `?` (as deploy/compose.yaml
/// produces from a base64 secret) either fails to parse or parses with the
/// password folded into the host or path. Those fall back to textual masking,
/// which over-redacts rather than leaks.
pub(crate) fn redact(url: &str) -> String {
    // More than one `@` means the parser may have ended the password early
    // (`by:a@b/c@db`); only the textual pass masks up to the last one.
    if at_signs(url) <= 1 {
        if let Ok(mut parsed) = url::Url::parse(url) {
            if parsed.password().is_some() {
                if let Ok(()) = parsed.set_password(Some("***")) {
                    return parsed.to_string();
                }
            } else if !parsed.username().is_empty() || at_signs(url) == 0 {
                return url.to_owned();
            }
            // No password, no username, yet an `@`: `by:12/pw@db` parses as
            // host `by`, port 12, path `/pw@db`.
        }
    }
    redact_text(url)
}

/// How many `@` the text after `://` holds.
fn at_signs(url: &str) -> usize {
    url.split_once("://")
        .map_or(0, |(_, rest)| rest.matches('@').count())
}

/// Mask what follows the first `:` of the userinfo, which ends at the last
/// `@` of the text; no `://`, no `@` or no `:` in the userinfo leaves `url`.
fn redact_text(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let Some(at) = rest.rfind('@') else {
        return url.to_owned();
    };
    match rest[..at].split_once(':') {
        Some((name, _)) => format!("{scheme}://{name}:***{}", &rest[at..]),
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

impl From<crate::store_codec::CodecError> for Fail {
    fn from(error: crate::store_codec::CodecError) -> Self {
        Fail::Error(error.into())
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
        let conn = self.conn.get_mut_recovering("conn");
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
                Some(Ok(1)) => upgrade_to_identities(tx),
                Some(other) => Err(Fail::Error(Error::State(format!(
                    "the database has Branchyard schema {other:?}; this version understands \
                     {SCHEMA}"
                )))),
            }
        })?;
        store.make_missing(STEPS)?;
        Ok(store)
    }

    /// Make each of `steps` the catalog says is missing, one at a time; see
    /// [`STEPS`].
    fn make_missing(&self, steps: &[(&str, &str)]) -> Result<(), Error> {
        let names: Vec<String> = steps.iter().map(|(name, _)| (*name).to_owned()).collect();
        let missing: Vec<String> = self
            .query(|client| {
                client.query(
                    "SELECT n FROM unnest($1::text[]) WITH ORDINALITY AS t (n, i) \
                     WHERE to_regclass(n) IS NULL ORDER BY i",
                    &[&names],
                )
            })?
            .iter()
            .map(|row| row.get(0))
            .collect();
        for (name, statement) in steps {
            if !missing.iter().any(|m| m == name) {
                continue;
            }
            self.tx(true, |tx| {
                tx.execute("SELECT pg_advisory_xact_lock(7390184325)", &[])
                    .map_err(db("schema"))?;
                let absent: bool = tx
                    .query_one("SELECT to_regclass($1::text) IS NULL", &[name])
                    .map_err(db("schema"))?
                    .get(0);
                if absent {
                    tx.batch_execute(statement).map_err(db("schema"))?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Option<Client>> {
        self.conn.lock_recovering("conn")
    }

    /// The connection, reconnecting after it closed.
    #[allow(clippy::expect_used)] // ratchet: branchyard
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
                if incarnation == fence.incarnation
                    && from_db("generation", generation)? == fence.generation =>
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
            record.removed = current.removed;
        }
        let text = encode(name, &record)?;
        let created = to_db("created_ms", record.created_ms)?;
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
        // Bind the parent's incarnation once, if it is older: the first
        // write naming a parent is the child's creation, while its parent
        // is alive (see `crate::storage::Lineage`).
        if let Some(parent) = &record.info.parent {
            tx.execute(
                "UPDATE by_branches b SET parent_incarnation = (SELECT p.incarnation \
                 FROM by_branches p WHERE p.repo = $1 AND p.name = $3 AND p.record IS NOT NULL \
                 AND p.incarnation < b.incarnation) \
                 WHERE b.repo = $1 AND b.name = $2 AND b.parent_incarnation IS NULL",
                &[&self.repo, name, parent],
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
                &to_db("at_ms", event.at_ms)?,
                &activity,
            ],
        )
        .map_err(db("append"))?;
        Ok(from_db("seq", seq)?)
    }

    /// Take `record`'s lease for a new turn and write it, unless a lease
    /// on this incarnation is held: then that lease.
    fn grant(
        &self,
        tx: &mut Transaction<'_>,
        record: &Record,
        incarnation: i64,
        owner: &Owner,
        ttl: Duration,
    ) -> R<Result<Fence, LeaseRow>> {
        let name = &record.info.name;
        let current = self.lease_row(tx, name)?;
        if let Some(row) = &current {
            if row.owner.is_some() && row.incarnation == incarnation {
                return Ok(Err(row.clone()));
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
                &to_db("generation", generation)?,
                &owner.id,
                &owner.host,
                &i64::from(owner.pid),
                &owner.start,
                &to_db("now_ms", now)?,
                &to_db("expires_ms", deadline(now, ttl))?,
            ],
        )
        .map_err(db("acquire"))?;
        self.put(tx, record)?;
        tx.execute(
            "DELETE FROM by_reservations WHERE repo = $1 AND name = $2",
            &[&self.repo, name],
        )
        .map_err(db("acquire"))?;
        Ok(Ok(Fence {
            branch: name.clone(),
            incarnation,
            generation,
            turn: generation,
        }))
    }

    /// Whether a live lease on `name`'s current incarnation is held.
    fn held(&self, tx: &mut Transaction<'_>, name: &str) -> R<bool> {
        let incarnation = self.incarnation(tx, name)?;
        Ok(self
            .lease_row(tx, name)?
            .is_some_and(|row| row.owner.is_some() && Some(row.incarnation) == incarnation))
    }

    fn graph_revision_in(&self, tx: &mut Transaction<'_>, parent: &str) -> R<u64> {
        let revision = tx
            .query_opt(
                "SELECT revision FROM by_graph_revisions WHERE repo = $1 AND parent = $2",
                &[&self.repo, &parent],
            )
            .map_err(db("graph revision"))?
            .map_or(0, |r| r.get::<_, i64>(0));
        Ok(from_db("revision", revision)?)
    }

    fn edges(&self, filter: &str, value: &str) -> Result<Vec<Dependency>, Error> {
        let sql = format!(
            "SELECT dependent, prerequisite, after FROM by_graph_edges \
             WHERE repo = $1 AND {filter} = $2 ORDER BY dependent, prerequisite"
        );
        let rows = self.query(|client| client.query(&sql, &[&self.repo, &value]))?;
        rows.iter()
            .map(|r| {
                Ok(Dependency {
                    dependent: r.get(0),
                    prerequisite: r.get(1),
                    after: parse_text("after", &r.get::<_, String>(2))?,
                })
            })
            .collect()
    }

    fn lease_row(&self, tx: &mut Transaction<'_>, name: &str) -> R<Option<LeaseRow>> {
        Ok(tx
            .query_opt(
                "SELECT branch, incarnation, generation, turn, owner, host, pid, pid_start, \
                 expires_ms, deadline_ms FROM by_leases WHERE repo = $1 AND branch = $2",
                &[&self.repo, &name],
            )
            .map_err(db("lease"))?
            .map(|r| lease_from(&r))
            .transpose()?)
    }
}

fn lease_from(r: &Row) -> Result<LeaseRow, Error> {
    Ok(LeaseRow {
        branch: r.get(0),
        incarnation: r.get(1),
        generation: from_db("generation", r.get(2))?,
        turn: from_db("turn", r.get(3))?,
        owner: r.get(4),
        host: r.get(5),
        pid: from_db_u32("pid", r.get::<_, i64>(6))?,
        start: r.get(7),
        expires_ms: from_db("expires_ms", r.get(8))?,
        deadline_ms: from_db_opt("deadline_ms", r.get::<_, Option<i64>>(9))?,
    })
}

fn event_from(what: &str, at_ms: i64, activity: &str) -> Result<RecordedEvent, Error> {
    Ok(RecordedEvent {
        at_ms: from_db("at_ms", at_ms)?,
        activity: decode(what, activity)?,
    })
}

impl Backend for Postgres {
    fn reserve(&self, name: &str, owner: &Owner) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let now = to_db("now_ms", now_ms())?;
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
        rows.iter()
            .map(|r| {
                Ok(ReservationRow {
                    name: r.get(0),
                    owner: r.get(1),
                    host: r.get(2),
                    pid: from_db_u32("pid", r.get::<_, i64>(3))?,
                    start: r.get(4),
                    reserved_ms: from_db("reserved_ms", r.get(5))?,
                })
            })
            .collect()
    }

    fn reclaim(&self, row: &ReservationRow) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let removed = tx
                .execute(
                    "DELETE FROM by_reservations \
                     WHERE repo = $1 AND name = $2 AND owner = $3 AND reserved_ms = $4",
                    &[
                        &self.repo,
                        &row.name,
                        &row.owner,
                        &to_db("reserved_ms", row.reserved_ms)?,
                    ],
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
            // Its spend stays in its parent's ledger.
            if let Some(removed) = self.stored_record(tx, name)? {
                let owner = match removed.info.parent.as_deref() {
                    Some(parent) => self.stored_record(tx, parent)?,
                    None => None,
                };
                if let Some(mut owner) = owner {
                    let mut read = |n: &str| self.stored_record(tx, n).ok().flatten();
                    if crate::state::note_removed(&mut owner, &removed, &mut read) {
                        let owner_name = owner.info.name.clone();
                        let text = encode(&owner_name, &owner)?;
                        tx.execute(
                            "UPDATE by_branches SET record = $3 WHERE repo = $1 AND name = $2",
                            &[&self.repo, &owner_name, &text],
                        )
                        .map_err(db("delete"))?;
                    }
                }
            }
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
            // Its own dependencies and graph go; what depends on it keeps
            // the row, and is blocked for want of it.
            for sql in [
                "DELETE FROM by_graph_edges WHERE repo = $1 AND dependent = $2",
                "DELETE FROM by_graph_revisions WHERE repo = $1 AND parent = $2",
                "DELETE FROM by_ports WHERE repo = $1 AND branch = $2",
                "DELETE FROM by_sandboxes WHERE repo = $1 AND branch = $2",
            ] {
                tx.execute(sql, &[&self.repo, &name])
                    .map_err(db("delete"))?;
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
            match self.grant(tx, record, incarnation, owner, ttl)? {
                Ok(fence) => Ok(Acquired::Granted(fence)),
                Err(row) => Ok(Acquired::Held(row)),
            }
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
                    &to_db("expires_ms", deadline(now_ms(), ttl))?,
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
        rows.iter().map(lease_from).collect()
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
                        &to_db("generation", lease.generation)?,
                        &owner.id,
                        &owner.host,
                        &i64::from(owner.pid),
                        &owner.start,
                        &to_db("now_ms", now)?,
                        &to_db("expires_ms", deadline(now, ttl))?,
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
                &[
                    &self.repo,
                    &fence.branch,
                    &to_db_opt("deadline_ms", deadline_ms)?,
                ],
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
            self.check(tx, fence)?;
            let row = tx
                .query_opt(
                    "SELECT intent, outcome FROM by_steps \
                     WHERE incarnation = $1 AND turn = $2 AND step = $3",
                    &[&fence.incarnation, &to_db("turn", turn)?, &step],
                )
                .map_err(db("step"))?
                .map(|r| (r.get::<_, String>(0), r.get::<_, Option<String>>(1)));
            match row {
                Some((_, Some(outcome))) => Ok(Begun::Done(decode(step, &outcome)?)),
                Some((intent, None)) => Ok(Begun::Pending(decode(step, &intent)?)),
                None => {
                    let now = to_db("now_ms", now_ms())?;
                    tx.execute(
                        "INSERT INTO by_steps (incarnation, turn, step, branch, generation, \
                         intent, started_ms) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                        &[
                            &fence.incarnation,
                            &to_db("turn", turn)?,
                            &step,
                            &fence.branch,
                            &to_db("generation", fence.generation)?,
                            &encode(step, intent)?,
                            &now,
                        ],
                    )
                    .map_err(db("step"))?;
                    for id in deliver {
                        tx.execute(
                            "UPDATE by_messages SET delivered_ms = $3 \
                             WHERE repo = $1 AND id = $2 AND delivered_ms IS NULL",
                            &[&self.repo, &to_db("id", *id)?, &now],
                        )
                        .map_err(db("message"))?;
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
            self.check(tx, fence)?;
            let changed = tx
                .execute(
                    "UPDATE by_steps SET outcome = $4, finished_ms = $5, generation = $6 \
                     WHERE incarnation = $1 AND turn = $2 AND step = $3",
                    &[
                        &fence.incarnation,
                        &to_db("turn", turn)?,
                        &step,
                        &encode(step, outcome)?,
                        &to_db("now_ms", now_ms())?,
                        &to_db("generation", fence.generation)?,
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
            self.check(tx, fence)?;
            for id in deliver {
                tx.execute(
                    "UPDATE by_messages SET delivered_ms = NULL WHERE repo = $1 AND id = $2 \
                     AND delivered_steer IS NULL AND delivered_ms = (SELECT started_ms \
                     FROM by_steps WHERE incarnation = $3 AND turn = $4 AND step = $5 \
                     AND outcome IS NULL)",
                    &[
                        &self.repo,
                        &to_db("id", *id)?,
                        &fence.incarnation,
                        &to_db("turn", turn)?,
                        &step,
                    ],
                )
                .map_err(db("message"))?;
            }
            tx.execute(
                "DELETE FROM by_steps WHERE incarnation = $1 AND turn = $2 AND step = $3 \
                 AND outcome IS NULL",
                &[&fence.incarnation, &to_db("turn", turn)?, &step],
            )
            .map_err(db("step"))?;
            Ok(())
        })
    }

    fn steps(&self, name: &str, turn: u64) -> Result<Vec<StepRow>, Error> {
        let turn_db = to_db("turn", turn)?;
        let rows = self.query(|c| {
            c.query(
                "SELECT s.step, s.intent, s.outcome FROM by_steps s \
                 JOIN by_branches b ON b.incarnation = s.incarnation \
                 WHERE b.repo = $1 AND b.name = $2 AND s.turn = $3 \
                 ORDER BY s.started_ms, s.step",
                &[&self.repo, &name, &turn_db],
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
                    &to_db("turn", fence.turn)?,
                    &i64::from(process.pid),
                    &fence.branch,
                    &i64::from(process.pgid),
                    &process.start,
                    &process.host,
                    &to_db("generation", fence.generation)?,
                    &to_db("now_ms", now_ms())?,
                ],
            )
            .map_err(db("process"))?;
            Ok(())
        })
    }

    fn processes(&self, name: &str, turn: u64) -> Result<Vec<ProcessRow>, Error> {
        let turn_db = to_db("turn", turn)?;
        let rows = self.query(|c| {
            c.query(
                "SELECT p.pid, p.pgid, p.start, p.host FROM by_processes p \
                 JOIN by_branches b ON b.incarnation = p.incarnation \
                 WHERE b.repo = $1 AND b.name = $2 AND p.turn = $3 ORDER BY p.recorded_ms",
                &[&self.repo, &name, &turn_db],
            )
        })?;
        rows.iter()
            .map(|r| {
                Ok(ProcessRow {
                    pid: from_db_u32("pid", r.get::<_, i64>(0))?,
                    pgid: from_db_u32("pgid", r.get::<_, i64>(1))?,
                    start: r.get(2),
                    host: r.get(3),
                })
            })
            .collect()
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
                    &to_db("turn", lease.turn)?,
                    &name,
                    &by,
                    &to_db("now_ms", now_ms())?,
                    &subtree,
                ],
            )
            .map_err(db("cancel"))?;
            Ok(true)
        })
    }

    fn cancel_requested(&self, fence: &Fence) -> Result<Option<String>, Error> {
        let turn_db = to_db("turn", fence.turn)?;
        Ok(self
            .query(|c| {
                c.query_opt(
                    "SELECT requested_by FROM by_cancels WHERE incarnation = $1 AND turn = $2",
                    &[&fence.incarnation, &turn_db],
                )
            })?
            .map(|r| r.get(0)))
    }

    fn request_steer(
        &self,
        name: &str,
        by: &str,
        text: &str,
        message: Option<u64>,
    ) -> Result<Option<u64>, Error> {
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
                        &to_db("turn", lease.turn)?,
                        &name,
                        &by,
                        &text,
                        &to_db("now_ms", now_ms())?,
                    ],
                )
                .map_err(db("steer"))?;
            let id: i64 = row.get(0);
            if let Some(message) = message {
                let linked = tx
                    .execute(
                        "UPDATE by_messages SET steer_id = $3 \
                         WHERE repo = $1 AND id = $2 AND delivered_ms IS NULL",
                        &[&self.repo, &to_db("message", message)?, &id],
                    )
                    .map_err(db("message"))?;
                if linked == 0 {
                    // Rolls the steer back with the transaction.
                    return Err(Error::Denied(format!(
                        "message #{message} is unknown or already delivered"
                    ))
                    .into());
                }
            }
            Ok(Some(from_db("id", id)?))
        })
    }

    fn pending_steers(&self, fence: &Fence) -> Result<Vec<SteerRow>, Error> {
        let turn_db = to_db("turn", fence.turn)?;
        self.query(|c| {
            c.query(
                &format!(
                    "SELECT {STEER_COLUMNS} FROM by_steers s WHERE s.incarnation = $1 \
                         AND s.turn = $2 AND s.state = 'pending' ORDER BY s.id"
                ),
                &[&fence.incarnation, &turn_db],
            )
        })?
        .iter()
        .map(steer_row)
        .collect()
    }

    fn settle_steer(
        &self,
        fence: &Fence,
        id: u64,
        state: &SteerState,
    ) -> Result<Option<u64>, Error> {
        let (name, reason) = state.columns();
        self.tx(true, |tx| {
            self.check(tx, fence)?;
            let settled = tx
                .execute(
                    "UPDATE by_steers SET state = $4, reason = $5 \
                     WHERE id = $1 AND incarnation = $2 AND turn = $3",
                    &[
                        &to_db("id", id)?,
                        &fence.incarnation,
                        &to_db("turn", fence.turn)?,
                        &name,
                        &reason,
                    ],
                )
                .map_err(db("steer"))?;
            if settled == 0 {
                return Ok(None);
            }
            match state {
                SteerState::Pending => Ok(None),
                SteerState::Written | SteerState::Accepted => Ok(tx
                    .query_opt(
                        "UPDATE by_messages SET delivered_ms = $2, delivered_steer = $1 \
                         WHERE steer_id = $1 AND delivered_ms IS NULL RETURNING id",
                        &[&to_db("id", id)?, &to_db("now_ms", now_ms())?],
                    )
                    .map_err(db("message"))?
                    .map(|r| from_db("id", r.get::<_, i64>(0)))
                    .transpose()?),
                SteerState::Refused { .. } => {
                    tx.execute(
                        "UPDATE by_messages SET steer_id = NULL, delivered_steer = NULL, \
                         delivered_ms = CASE WHEN delivered_steer = $1 THEN NULL \
                         ELSE delivered_ms END WHERE steer_id = $1",
                        &[&to_db("id", id)?],
                    )
                    .map_err(db("message"))?;
                    Ok(None)
                }
            }
        })
    }

    fn steer(&self, name: &str, id: u64) -> Result<Option<SteerRow>, Error> {
        let repo = self.repo.clone();
        let name = name.to_owned();
        let id_db = to_db("id", id)?;
        self.query(move |c| {
            c.query_opt(
                &format!(
                    "SELECT {STEER_COLUMNS} FROM by_steers s JOIN by_branches b \
                         ON b.incarnation = s.incarnation \
                         WHERE b.repo = $1 AND b.name = $2 AND s.id = $3"
                ),
                &[&repo, &name, &id_db],
            )
        })?
        .as_ref()
        .map(steer_row)
        .transpose()
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
        let after_db = to_db("after", after)?;
        let limit_db = to_db_usize("limit", limit)?;
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
                &[&self.repo, &name, &after_db, &limit_db],
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
                    from_db("seq", seq)?,
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
        let seq = row
            .map(|r| r.get::<_, i64>(0))
            .ok_or_else(|| Error::UnknownBranch(name.to_owned()))?;
        Ok(from_db("seq", seq)?)
    }

    fn feed_since(&self, after: u64, limit: usize) -> Result<Vec<FeedRow>, Error> {
        let after_db = to_db("after", after)?;
        let limit_db = to_db_usize("limit", limit)?;
        let rows = self.query(|c| {
            c.query(
                "SELECT id, branch, at_ms, activity FROM by_events WHERE repo = $1 AND id > $2 \
                 ORDER BY id LIMIT $3",
                &[&self.repo, &after_db, &limit_db],
            )
        })?;
        rows.iter()
            .map(|r| {
                let id: i64 = r.get(0);
                Ok(FeedRow {
                    id: from_db("id", id)?,
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
        .and_then(|r| Ok(from_db("id", r.get::<_, i64>(0))?))
    }

    fn send_message(&self, message: &Message) -> Result<Message, Error> {
        self.tx(true, |tx| {
            let at_ms = now_ms();
            let row = tx
                .query_one(
                    "INSERT INTO by_messages \
                     (repo, from_branch, to_branch, kind, text, in_reply_to, at_ms, \
                      delivered_ms) VALUES ($1, $2, $3, $4, $5, $6, $7, NULL) RETURNING id",
                    &[
                        &self.repo,
                        &message.from,
                        &message.to,
                        &message.kind.as_str(),
                        &message.text,
                        &to_db_opt("in_reply_to", message.in_reply_to)?,
                        &to_db("at_ms", at_ms)?,
                    ],
                )
                .map_err(db("message"))?;
            Ok(Message {
                id: from_db("id", row.get(0))?,
                at_ms,
                delivered: false,
                ..message.clone()
            })
        })
    }

    fn message(&self, id: u64) -> Result<Option<Message>, Error> {
        let id_db = to_db("id", id)?;
        let row = self.query(|c| {
            c.query_opt(
                "SELECT id, from_branch, to_branch, kind, text, in_reply_to, at_ms, \
                 delivered_ms FROM by_messages WHERE repo = $1 AND id = $2",
                &[&self.repo, &id_db],
            )
        })?;
        row.map(|r| message_row(&r)).transpose()
    }

    fn inbox(&self, to: &str) -> Result<Vec<Message>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT id, from_branch, to_branch, kind, text, in_reply_to, at_ms, \
                 delivered_ms FROM by_messages WHERE repo = $1 AND to_branch = $2 ORDER BY id",
                &[&self.repo, &to],
            )
        })?;
        rows.iter().map(message_row).collect()
    }

    fn mark_delivered(&self, ids: &[u64]) -> Result<(), Error> {
        if ids.is_empty() {
            return Ok(());
        }
        self.tx(true, |tx| {
            let now = to_db("now_ms", now_ms())?;
            for id in ids {
                tx.execute(
                    "UPDATE by_messages SET delivered_ms = $3 \
                     WHERE repo = $1 AND id = $2 AND delivered_ms IS NULL",
                    &[&self.repo, &to_db("id", *id)?, &now],
                )
                .map_err(db("message"))?;
            }
            Ok(())
        })
    }

    fn answer_to(&self, question_id: u64) -> Result<Option<Message>, Error> {
        let question_id_db = to_db("question_id", question_id)?;
        let row = self.query(|c| {
            c.query_opt(
                "SELECT id, from_branch, to_branch, kind, text, in_reply_to, at_ms, \
                 delivered_ms FROM by_messages WHERE repo = $1 AND in_reply_to = $2 \
                 ORDER BY id LIMIT 1",
                &[&self.repo, &question_id_db],
            )
        })?;
        row.map(|r| message_row(&r)).transpose()
    }

    fn message_steer(&self, id: u64) -> Result<Option<u64>, Error> {
        let id_db = to_db("id", id)?;
        let row = self.query(|c| {
            c.query_opt(
                "SELECT steer_id FROM by_messages WHERE repo = $1 AND id = $2",
                &[&self.repo, &id_db],
            )
        })?;
        Ok(from_db_opt(
            "steer_id",
            row.and_then(|r| r.get::<_, Option<i64>>(0)),
        )?)
    }

    fn set_awaiting(&self, id: u64, until_ms: Option<u64>) -> Result<(), Error> {
        self.tx(true, |tx| {
            tx.execute(
                "UPDATE by_messages SET awaiting_until_ms = $3 WHERE repo = $1 AND id = $2",
                &[
                    &self.repo,
                    &to_db("id", id)?,
                    &to_db_opt("until_ms", until_ms)?,
                ],
            )
            .map_err(db("message"))?;
            Ok(())
        })
    }

    fn awaiting_answer(&self, from: &str, now_ms: u64) -> Result<bool, Error> {
        let now_ms_db = to_db("now_ms", now_ms)?;
        let row = self.query(|c| {
            c.query_one(
                "SELECT EXISTS (SELECT 1 FROM by_messages q \
                 WHERE q.repo = $1 AND q.from_branch = $2 AND q.kind = 'question' \
                 AND q.awaiting_until_ms > $3 \
                 AND NOT EXISTS (SELECT 1 FROM by_messages a \
                 WHERE a.repo = $1 AND a.in_reply_to = q.id))",
                &[&self.repo, &from, &now_ms_db],
            )
        })?;
        Ok(row.get(0))
    }
}

/// Reads one `by_messages` row.
fn message_row(r: &Row) -> Result<Message, Error> {
    let kind: String = r.get(3);
    Ok(Message {
        id: from_db("id", r.get(0))?,
        from: r.get(1),
        to: r.get(2),
        kind: kind.parse()?,
        text: r.get(4),
        in_reply_to: from_db_opt("in_reply_to", r.get::<_, Option<i64>>(5))?,
        at_ms: from_db("at_ms", r.get(6))?,
        delivered: r.get::<_, Option<i64>>(7).is_some(),
    })
}

/// Upgrade a schema 1 database: add the identity columns and bind every
/// name-only grant of every repository once, by [`LegacyBinder`]'s rule, in
/// the transaction (and under the advisory lock) that checked the schema.
fn upgrade_to_identities(tx: &mut Transaction<'_>) -> R<()> {
    let e = || db("upgrade to schema 2");
    for sql in [
        "ALTER TABLE by_branches ADD COLUMN IF NOT EXISTS parent_incarnation BIGINT",
        "ALTER TABLE by_artifacts ADD COLUMN IF NOT EXISTS publisher_incarnation BIGINT",
        "ALTER TABLE by_artifacts ADD COLUMN IF NOT EXISTS ancestry_incarnations TEXT NOT NULL \
         DEFAULT '[]'",
        "ALTER TABLE by_artifact_shares ADD COLUMN IF NOT EXISTS incarnation BIGINT",
        "ALTER TABLE by_scratch_areas ADD COLUMN IF NOT EXISTS owner_incarnation BIGINT",
        "ALTER TABLE by_scratch_areas ADD COLUMN IF NOT EXISTS ancestry_incarnations TEXT \
         NOT NULL DEFAULT '[]'",
        "ALTER TABLE by_scratch_shares ADD COLUMN IF NOT EXISTS incarnation BIGINT",
        "ALTER TABLE by_scratch_locks ADD COLUMN IF NOT EXISTS holder_incarnation BIGINT",
    ] {
        tx.execute(sql, &[]).map_err(e())?;
    }
    let mut repos: std::collections::BTreeMap<String, Vec<LegacyBranch>> = Default::default();
    for row in tx
        .query(
            "SELECT repo, incarnation, name, created_ms, record FROM by_branches \
             WHERE record IS NOT NULL",
            &[],
        )
        .map_err(e())?
    {
        let name: String = row.get(2);
        let text: String = row.get(4);
        let record: Record = decode(&format!("record {name}"), &text)?;
        repos.entry(row.get(0)).or_default().push(LegacyBranch {
            name,
            incarnation: row.get(1),
            created_ms: from_db("created_ms", row.get(3))?,
            parent: record.info.parent,
        });
    }
    let empty = Vec::new();
    let binder_for = |repo: &str| LegacyBinder::new(repos.get(repo).unwrap_or(&empty));
    for branches in repos.values() {
        let binder = LegacyBinder::new(branches);
        for branch in branches {
            tx.execute(
                "UPDATE by_branches SET parent_incarnation = $2 WHERE incarnation = $1",
                &[&branch.incarnation, &binder.parent(branch)],
            )
            .map_err(e())?;
        }
    }
    for row in tx
        .query(
            "SELECT repo, seq, publisher, ancestry, created_ms FROM by_artifacts",
            &[],
        )
        .map_err(e())?
    {
        let binder = binder_for(row.get(0));
        let seq: i64 = row.get(1);
        let publisher: String = row.get(2);
        let ancestry: Vec<String> = decode("artifact ancestry", row.get(3))?;
        let created = from_db("created_ms", row.get(4))?;
        let ancestry = encode("ancestry", &binder.bind_all(&ancestry, created))?;
        tx.execute(
            "UPDATE by_artifacts SET publisher_incarnation = $2, ancestry_incarnations = $3 \
             WHERE seq = $1",
            &[&seq, &binder.bind(&publisher, Some(created)), &ancestry],
        )
        .map_err(e())?;
    }
    for row in tx
        .query(
            "SELECT repo, name, owner, ancestry, created_ms FROM by_scratch_areas",
            &[],
        )
        .map_err(e())?
    {
        let repo: String = row.get(0);
        let binder = binder_for(&repo);
        let name: String = row.get(1);
        let owner: String = row.get(2);
        let ancestry: Vec<String> = decode("scratch ancestry", row.get(3))?;
        let created = from_db("created_ms", row.get(4))?;
        let ancestry = encode("ancestry", &binder.bind_all(&ancestry, created))?;
        tx.execute(
            "UPDATE by_scratch_areas SET owner_incarnation = $3, ancestry_incarnations = $4 \
             WHERE repo = $1 AND name = $2",
            &[&repo, &name, &binder.bind(&owner, Some(created)), &ancestry],
        )
        .map_err(e())?;
    }
    for (table, key) in [("by_artifact_shares", "id"), ("by_scratch_shares", "name")] {
        for row in tx
            .query(&format!("SELECT repo, {key}, branch FROM {table}"), &[])
            .map_err(e())?
        {
            let repo: String = row.get(0);
            let id: String = row.get(1);
            let branch: String = row.get(2);
            let bound = binder_for(&repo).bind(&branch, None);
            tx.execute(
                &format!(
                    "UPDATE {table} SET incarnation = $4 \
                     WHERE repo = $1 AND {key} = $2 AND branch = $3"
                ),
                &[&repo, &id, &branch, &bound],
            )
            .map_err(e())?;
        }
    }
    for row in tx
        .query(
            "SELECT repo, name, holder, acquired_ms FROM by_scratch_locks",
            &[],
        )
        .map_err(e())?
    {
        let repo: String = row.get(0);
        let name: String = row.get(1);
        let holder: String = row.get(2);
        let bound = binder_for(&repo).bind(&holder, Some(from_db("acquired_ms", row.get(3))?));
        tx.execute(
            "UPDATE by_scratch_locks SET holder_incarnation = $3 WHERE repo = $1 AND name = $2",
            &[&repo, &name, &bound],
        )
        .map_err(e())?;
    }
    tx.execute(
        "UPDATE by_meta SET value = $1 WHERE key = 'schema'",
        &[&SCHEMA.to_string()],
    )
    .map_err(e())?;
    Ok(())
}

fn artifact_row_from(row: &Row) -> Result<ArtifactRow, Error> {
    let ancestry: String = row.get(6);
    let labels: String = row.get(9);
    let ancestry_incarnations: String = row.get(11);
    Ok(ArtifactRow {
        artifact: ArtifactRef {
            id: row.get(0),
            digest: row.get(1),
            size: from_db("size", row.get(2))?,
            name: row.get(3),
            media_type: row.get(4),
            publisher_branch: row.get(5),
            turn: from_db("turn", row.get(7))?,
            created_at: from_db("created_at", row.get::<_, i64>(8))? / 1000,
            labels: decode("artifact labels", &labels)?,
        },
        ancestry: decode("artifact ancestry", &ancestry)?,
        publisher_incarnation: row.get(10),
        ancestry_incarnations: decode("artifact ancestry", &ancestry_incarnations)?,
    })
}

const ARTIFACT_COLUMNS: &str = "id, digest, size, name, media_type, publisher, ancestry, turn, \
     created_ms, labels, publisher_incarnation, ancestry_incarnations";

const SCRATCH_COLUMNS: &str =
    "name, owner, ancestry, created_ms, owner_incarnation, ancestry_incarnations";

fn scratch_row_from(row: &Row) -> Result<ScratchRow, Error> {
    let ancestry: String = row.get(2);
    let ancestry_incarnations: String = row.get(5);
    Ok(ScratchRow {
        area: ScratchArea {
            name: row.get(0),
            owner_branch: row.get(1),
            created_at: from_db("created_at", row.get::<_, i64>(3))? / 1000,
        },
        ancestry: decode("scratch ancestry", &ancestry)?,
        owner_incarnation: row.get(4),
        ancestry_incarnations: decode("scratch ancestry", &ancestry_incarnations)?,
    })
}

fn share_from(row: &Row) -> Share {
    Share {
        branch: row.get(0),
        incarnation: row.get(1),
    }
}

impl Postgres {
    /// Whether the branch at `incarnation` says `running`; a removed one
    /// does not.
    fn is_running(&self, tx: &mut Transaction<'_>, incarnation: i64) -> R<bool> {
        let text: Option<String> = tx
            .query_opt(
                "SELECT record FROM by_branches WHERE repo = $1 AND incarnation = $2",
                &[&self.repo, &incarnation],
            )
            .map_err(db("read"))?
            .and_then(|r| r.get(0));
        match text {
            Some(text) => {
                let record: Record = decode("record", &text)?;
                Ok(record.info.status == BranchStatus::Running)
            }
            None => Ok(false),
        }
    }
}

impl PortBackend for Postgres {
    fn reserve_port(
        &self,
        branch: &str,
        start: u16,
        usable: &(dyn Fn(u16) -> bool + Sync),
    ) -> Result<u16, Error> {
        self.tx(true, |tx| {
            let held = tx
                .query_opt(
                    "SELECT port FROM by_ports WHERE repo = $1 AND branch = $2",
                    &[&self.repo, &branch],
                )
                .map_err(db("port"))?;
            if let Some(row) = held {
                return Ok(from_db_u16("port", i64::from(row.get::<_, i32>(0)))?);
            }
            // Ports are unique across the database, whichever repository
            // holds them: its repositories may share a host.
            let taken = tx
                .query("SELECT port FROM by_ports", &[])
                .map_err(db("port"))?
                .iter()
                .map(|r| from_db_u16("port", i64::from(r.get::<_, i32>(0))))
                .collect::<Result<_, _>>()?;
            let port = pick_port(start, &taken, usable)?;
            tx.execute(
                "INSERT INTO by_ports (port, repo, branch, reserved_ms) VALUES ($1, $2, $3, $4)",
                &[
                    &i32::from(port),
                    &self.repo,
                    &branch,
                    &to_db("now_ms", now_ms())?,
                ],
            )
            .map_err(|error| match error.code() {
                // Another reservation took it first: try again from the
                // start, which sees that one.
                Some(code) if *code == SqlState::UNIQUE_VIOLATION => Fail::Retry(error),
                _ => db("port")(error),
            })?;
            Ok(port)
        })
    }

    fn port(&self, branch: &str) -> Result<Option<u16>, Error> {
        let row = self.query(|client| {
            client.query_opt(
                "SELECT port FROM by_ports WHERE repo = $1 AND branch = $2",
                &[&self.repo, &branch],
            )
        })?;
        row.map(|r| from_db_u16("port", i64::from(r.get::<_, i32>(0))))
            .transpose()
            .map_err(Error::from)
    }
}

const SANDBOX_COLUMNS: &str = "branch, incarnation, kind, provider, name, turn, detail, used_ms";

fn sandbox_row(r: &Row) -> Result<SandboxRow, Error> {
    Ok(SandboxRow {
        branch: r.get(0),
        incarnation: r.get(1),
        kind: SandboxKind::parse(r.get(2))?,
        provider: r.get(3),
        name: r.get(4),
        turn: r
            .get::<_, Option<i64>>(5)
            .map(|t| from_db_u32("turn", t))
            .transpose()?,
        detail: r.get(6),
        used_ms: from_db("used_ms", r.get(7))?,
    })
}

impl SandboxBackend for Postgres {
    fn put_sandbox(&self, row: &SandboxRow) -> Result<(), Error> {
        self.tx(true, |tx| {
            tx.execute(
                "INSERT INTO by_sandboxes \
                 (repo, branch, incarnation, kind, provider, name, turn, detail, used_ms) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT (repo, branch, kind, name) DO UPDATE SET \
                 incarnation = EXCLUDED.incarnation, provider = EXCLUDED.provider, \
                 turn = EXCLUDED.turn, detail = EXCLUDED.detail, used_ms = EXCLUDED.used_ms",
                &[
                    &self.repo,
                    &row.branch,
                    &row.incarnation,
                    &row.kind.as_str(),
                    &row.provider,
                    &row.name,
                    &row.turn.map(i64::from),
                    &row.detail,
                    &to_db("used_ms", row.used_ms)?,
                ],
            )
            .map_err(db("sandbox"))?;
            Ok(())
        })
    }

    fn sandboxes(&self, branch: &str) -> Result<Vec<SandboxRow>, Error> {
        let rows = self.query(|client| {
            client.query(
                &format!(
                    "SELECT {SANDBOX_COLUMNS} FROM by_sandboxes WHERE repo = $1 AND branch = $2 \
                     ORDER BY used_ms, name"
                ),
                &[&self.repo, &branch],
            )
        })?;
        rows.iter().map(sandbox_row).collect()
    }

    fn sandboxes_of(&self, kind: SandboxKind, provider: &str) -> Result<Vec<SandboxRow>, Error> {
        let rows = self.query(|client| {
            client.query(
                &format!(
                    "SELECT {SANDBOX_COLUMNS} FROM by_sandboxes \
                     WHERE repo = $1 AND kind = $2 AND provider = $3 \
                     ORDER BY used_ms, branch, name"
                ),
                &[&self.repo, &kind.as_str(), &provider],
            )
        })?;
        rows.iter().map(sandbox_row).collect()
    }

    fn take_sandbox(
        &self,
        branch: &str,
        kind: SandboxKind,
        name: &str,
    ) -> Result<Option<SandboxRow>, Error> {
        let row = self.tx(true, |tx| {
            tx.query_opt(
                &format!(
                    "DELETE FROM by_sandboxes \
                     WHERE repo = $1 AND branch = $2 AND kind = $3 AND name = $4 \
                     RETURNING {SANDBOX_COLUMNS}"
                ),
                &[&self.repo, &branch, &kind.as_str(), &name],
            )
            .map_err(db("sandbox"))
        })?;
        row.as_ref().map(sandbox_row).transpose()
    }
}

impl GraphBackend for Postgres {
    fn graph_revision(&self, parent: &str) -> Result<u64, Error> {
        let row = self.query(|client| {
            client.query_opt(
                "SELECT revision FROM by_graph_revisions WHERE repo = $1 AND parent = $2",
                &[&self.repo, &parent],
            )
        })?;
        Ok(from_db("revision", row.map_or(0, |r| r.get::<_, i64>(0)))?)
    }

    fn dependencies(&self, parent: &str) -> Result<Vec<Dependency>, Error> {
        self.edges("parent", parent)
    }

    fn prerequisites(&self, dependent: &str) -> Result<Vec<Dependency>, Error> {
        self.edges("dependent", dependent)
    }

    fn dependents(&self, prerequisite: &str) -> Result<Vec<Dependency>, Error> {
        self.edges("prerequisite", prerequisite)
    }

    fn commit_graph(&self, commit: &GraphCommit) -> Result<u64, Error> {
        let parent = &commit.parent;
        self.tx(true, |tx| {
            let current = self.graph_revision_in(tx, parent)?;
            if let Some(expected) = commit.expected.filter(|e| *e != current) {
                return Err(Error::StaleRevision {
                    branch: parent.clone(),
                    expected,
                    actual: current,
                }
                .into());
            }
            let mut owner = self
                .stored_record(tx, parent)?
                .ok_or_else(|| Error::UnknownBranch(parent.clone()))?;
            for record in &commit.create {
                let name = &record.info.name;
                if self.incarnation(tx, name)?.is_some() {
                    return Err(Error::BranchExists(name.clone()).into());
                }
                self.put(tx, record)?;
                if !owner.info.children.contains(name) {
                    owner.info.children.push(name.clone());
                }
            }
            tx.execute(
                "UPDATE by_branches SET record = $3 WHERE repo = $1 AND name = $2",
                &[&self.repo, parent, &encode(parent, &owner)?],
            )
            .map_err(db("commit graph"))?;
            let created: Vec<&String> = commit.create.iter().map(|r| &r.info.name).collect();
            let touched: std::collections::BTreeSet<&String> = commit
                .add
                .iter()
                .map(|d| &d.dependent)
                .chain(commit.remove.iter().map(|d| &d.dependent))
                .filter(|name| !created.contains(name))
                .collect();
            for name in touched {
                let mut record = self
                    .stored_record(tx, name)?
                    .ok_or_else(|| Error::UnknownBranch(name.clone()))?;
                if !crate::graph::unstarted(&record.info.status) || self.held(tx, name)? {
                    return Err(Error::Denied(format!(
                        "{name} has already started, so its dependencies can no longer change"
                    ))
                    .into());
                }
                if record.info.status != BranchStatus::Waiting {
                    record.info.status = BranchStatus::Waiting;
                    self.put(tx, &record)?;
                    self.insert_event(
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
                        "DELETE FROM by_graph_edges WHERE repo = $1 AND parent = $2 \
                         AND dependent = $3 AND prerequisite = $4",
                        &[&self.repo, parent, &d.dependent, &d.prerequisite],
                    )
                    .map_err(db("commit graph"))?;
                if removed == 0 {
                    return Err(Error::Denied(format!(
                        "{} does not depend on {}",
                        d.dependent, d.prerequisite
                    ))
                    .into());
                }
            }
            for d in &commit.add {
                let inserted = tx
                    .execute(
                        "INSERT INTO by_graph_edges (repo, parent, dependent, prerequisite, \
                         after) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
                        &[
                            &self.repo,
                            parent,
                            &d.dependent,
                            &d.prerequisite,
                            &<&'static str>::from(d.after),
                        ],
                    )
                    .map_err(db("commit graph"))?;
                if inserted == 0 {
                    return Err(Error::Denied(format!(
                        "{} already depends on {}",
                        d.dependent, d.prerequisite
                    ))
                    .into());
                }
            }
            let next = current + 1;
            tx.execute(
                "INSERT INTO by_graph_revisions (repo, parent, revision) VALUES ($1, $2, $3) \
                 ON CONFLICT (repo, parent) DO UPDATE SET revision = $3",
                &[&self.repo, parent, &to_db("next", next)?],
            )
            .map_err(db("commit graph"))?;
            Ok(next)
        })
    }

    fn claim(&self, record: &Record, owner: &Owner, ttl: Duration) -> Result<Option<Fence>, Error> {
        let name = &record.info.name;
        self.tx(true, |tx| {
            let waiting = self
                .stored_record(tx, name)?
                .is_some_and(|stored| stored.info.status == BranchStatus::Waiting);
            let Some(incarnation) = self.incarnation(tx, name)? else {
                return Ok(None);
            };
            if !waiting {
                return Ok(None);
            }
            Ok(self.grant(tx, record, incarnation, owner, ttl)?.ok())
        })
    }

    fn settle_waiting(&self, record: &Record, event: &RecordedEvent) -> Result<bool, Error> {
        let name = &record.info.name;
        self.tx(true, |tx| {
            let waiting = self
                .stored_record(tx, name)?
                .is_some_and(|stored| stored.info.status == BranchStatus::Waiting);
            if !waiting || self.held(tx, name)? {
                return Ok(false);
            }
            self.put(tx, record)?;
            self.insert_event(tx, name, event)?;
            Ok(true)
        })
    }
}

impl StorageBackend for Postgres {
    fn identities(&self) -> Result<Vec<Identity>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT incarnation, name, parent_incarnation, record FROM by_branches \
                 WHERE repo = $1 AND record IS NOT NULL",
                &[&self.repo],
            )
        })?;
        rows.iter()
            .map(|r| {
                let name: String = r.get(1);
                let text: String = r.get(3);
                let record: Record = decode(&format!("record {name}"), &text)?;
                Ok(Identity {
                    name,
                    incarnation: r.get(0),
                    parent_incarnation: r.get(2),
                    parent: record.info.parent,
                })
            })
            .collect()
    }

    fn create_artifact(&self, new: &NewArtifact) -> Result<ArtifactRow, Error> {
        self.tx(true, |tx| {
            let now = to_db("now_ms", now_ms())?;
            let ancestry = encode("ancestry", &new.ancestry)?;
            let ancestry_incarnations = encode("ancestry", &new.ancestry_incarnations)?;
            let labels = encode("labels", &new.labels)?;
            let seq: i64 = tx
                .query_one(
                    "INSERT INTO by_artifacts (repo, id, digest, size, name, media_type, \
                     publisher, ancestry, turn, created_ms, labels, publisher_incarnation, \
                     ancestry_incarnations) \
                     VALUES ($1, '', $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING seq",
                    &[
                        &self.repo,
                        &new.digest,
                        &to_db("size", new.size)?,
                        &new.name,
                        &new.media_type,
                        &new.publisher_branch,
                        &ancestry,
                        &to_db("turn", new.turn)?,
                        &now,
                        &labels,
                        &new.publisher_incarnation,
                        &ancestry_incarnations,
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
                    created_at: from_db("now", now)? / 1000,
                    labels: new.labels.clone(),
                },
                ancestry: new.ancestry.clone(),
                publisher_incarnation: Some(new.publisher_incarnation),
                ancestry_incarnations: new.ancestry_incarnations.clone(),
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

    fn artifact_shares(&self, id: &str) -> Result<Vec<Share>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT branch, incarnation FROM by_artifact_shares WHERE repo = $1 AND id = $2",
                &[&self.repo, &id],
            )
        })?;
        Ok(rows.iter().map(share_from).collect())
    }

    fn share_artifact(&self, id: &str, branch: &str, incarnation: i64) -> Result<bool, Error> {
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
                    "INSERT INTO by_artifact_shares (repo, id, branch, incarnation) \
                     VALUES ($1, $2, $3, $4) ON CONFLICT (repo, id, branch) \
                     DO UPDATE SET incarnation = excluded.incarnation",
                    &[&self.repo, &id, &branch, &incarnation],
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
        Ok(from_db("refcount", row.get::<_, i64>(0))?)
    }

    fn create_scratch(&self, new: &NewScratch) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let ancestry = encode("ancestry", &new.ancestry)?;
            let ancestry_incarnations = encode("ancestry", &new.ancestry_incarnations)?;
            let inserted = tx
                .execute(
                    "INSERT INTO by_scratch_areas (repo, name, owner, ancestry, created_ms, \
                     owner_incarnation, ancestry_incarnations) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
                    &[
                        &self.repo,
                        &new.name,
                        &new.owner,
                        &ancestry,
                        &to_db("now_ms", now_ms())?,
                        &new.owner_incarnation,
                        &ancestry_incarnations,
                    ],
                )
                .map_err(db("scratch"))?;
            Ok(inserted == 1)
        })
    }

    fn scratch(&self, name: &str) -> Result<Option<ScratchRow>, Error> {
        self.query(|c| {
            c.query_opt(
                &format!(
                    "SELECT {SCRATCH_COLUMNS} FROM by_scratch_areas WHERE repo = $1 AND name = $2"
                ),
                &[&self.repo, &name],
            )
        })?
        .map(|r| scratch_row_from(&r))
        .transpose()
    }

    fn scratch_list(&self) -> Result<Vec<ScratchRow>, Error> {
        let rows = self.query(|c| {
            c.query(
                &format!(
                    "SELECT {SCRATCH_COLUMNS} FROM by_scratch_areas WHERE repo = $1 \
                     ORDER BY created_ms"
                ),
                &[&self.repo],
            )
        })?;
        rows.iter().map(scratch_row_from).collect()
    }

    fn scratch_shares(&self, name: &str) -> Result<Vec<Share>, Error> {
        let rows = self.query(|c| {
            c.query(
                "SELECT branch, incarnation FROM by_scratch_shares WHERE repo = $1 AND name = $2",
                &[&self.repo, &name],
            )
        })?;
        Ok(rows.iter().map(share_from).collect())
    }

    fn share_scratch(&self, name: &str, branch: &str, incarnation: i64) -> Result<bool, Error> {
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
                    "INSERT INTO by_scratch_shares (repo, name, branch, incarnation) \
                     VALUES ($1, $2, $3, $4) ON CONFLICT (repo, name, branch) \
                     DO UPDATE SET incarnation = excluded.incarnation",
                    &[&self.repo, &name, &branch, &incarnation],
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

    fn scratch_lock(
        &self,
        name: &str,
        branch: &str,
        incarnation: i64,
    ) -> Result<Option<LockOutcome>, Error> {
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
                    "SELECT holder, acquired_ms, holder_incarnation FROM by_scratch_locks \
                     WHERE repo = $1 AND name = $2",
                    &[&self.repo, &name],
                )
                .map_err(db("scratch lock"))?
                .map(|r| {
                    (
                        r.get::<_, String>(0),
                        r.get::<_, i64>(1),
                        r.get::<_, Option<i64>>(2),
                    )
                });
            let grant = |tx: &mut Transaction<'_>| -> R<LockOutcome> {
                let now = to_db("now_ms", now_ms())?;
                tx.execute(
                    "INSERT INTO by_scratch_locks (repo, name, holder, acquired_ms, \
                     holder_incarnation) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (repo, name) \
                     DO UPDATE SET holder = excluded.holder, acquired_ms = excluded.acquired_ms, \
                     holder_incarnation = excluded.holder_incarnation",
                    &[&self.repo, &name, &branch, &now, &incarnation],
                )
                .map_err(db("scratch lock"))?;
                Ok(LockOutcome::Granted(ScratchLock {
                    name: name.to_owned(),
                    holder_branch: branch.to_owned(),
                    acquired_at: from_db("now", now)? / 1000,
                }))
            };
            match current {
                None => Ok(Some(grant(tx)?)),
                Some((_, _, holder)) if holder == Some(incarnation) => Ok(Some(grant(tx)?)),
                // An unbound holder (see `LegacyBinder`) is gone.
                Some((_, _, None)) => Ok(Some(grant(tx)?)),
                Some((_, _, Some(holder))) if !self.is_running(tx, holder)? => Ok(Some(grant(tx)?)),
                Some((holder, acquired_ms, _)) => Ok(Some(LockOutcome::Held(ScratchLock {
                    name: name.to_owned(),
                    holder_branch: holder,
                    acquired_at: from_db("acquired_ms", acquired_ms)? / 1000,
                }))),
            }
        })
    }

    fn scratch_unlock(&self, name: &str, incarnation: i64) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let changed = tx
                .execute(
                    "DELETE FROM by_scratch_locks WHERE repo = $1 AND name = $2 \
                     AND holder_incarnation = $3",
                    &[&self.repo, &name, &incarnation],
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
        .and_then(|opt| {
            opt.map(|r| {
                Ok(ScratchLock {
                    name: name.to_owned(),
                    holder_branch: r.get(0),
                    acquired_at: from_db("acquired_ms", r.get::<_, i64>(1))? / 1000,
                })
            })
            .transpose()
        })
    }
}

const OUTCOME_COLUMNS: &str = "id, repository, branch, kind, harness, model, effort, outcome, \
     score, cost_usd, duration_ms, turns, routed, recorded_ms";

fn outcome_row(r: &Row) -> Result<crate::fleet::OutcomeRecord, Error> {
    Ok(crate::fleet::OutcomeRecord {
        id: r.get(0),
        repo: r.get(1),
        branch: r.get(2),
        kind: r
            .get::<_, String>(3)
            .parse()
            .map_err(|e| Error::State(format!("outcome kind: {e}")))?,
        harness: r.get(4),
        model: r.get(5),
        effort: r.get(6),
        outcome: crate::fleet::BranchOutcome::parse(r.get(7))?,
        score: r.get(8),
        cost_usd: r.get(9),
        duration_ms: from_db_opt("duration_ms", r.get::<_, Option<i64>>(10))?,
        turns: from_db_u32("turns", r.get::<_, i64>(11))?,
        routed: r.get(12),
        recorded_ms: from_db("recorded_ms", r.get(13))?,
    })
}

impl crate::fleet::OutcomeBackend for Postgres {
    fn put_outcome(&self, row: &crate::fleet::OutcomeRecord) -> Result<(), Error> {
        // Statistics derived from branches, not a branch's state: committed
        // as event appends are, without a sync of its own.
        self.tx(false, |tx| {
            tx.execute(
                &format!(
                    "INSERT INTO by_outcomes (repo, {OUTCOME_COLUMNS}) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
                     ON CONFLICT (repo, id) DO UPDATE SET \
                     repository = EXCLUDED.repository, branch = EXCLUDED.branch, \
                     kind = EXCLUDED.kind, harness = EXCLUDED.harness, model = EXCLUDED.model, \
                     effort = EXCLUDED.effort, outcome = EXCLUDED.outcome, \
                     score = EXCLUDED.score, cost_usd = EXCLUDED.cost_usd, \
                     duration_ms = EXCLUDED.duration_ms, turns = EXCLUDED.turns, \
                     routed = EXCLUDED.routed, recorded_ms = EXCLUDED.recorded_ms"
                ),
                &[
                    &self.repo,
                    &row.id,
                    &row.repo,
                    &row.branch,
                    &row.kind.as_str(),
                    &row.harness,
                    &row.model,
                    &row.effort,
                    &row.outcome.as_str(),
                    &row.score,
                    &row.cost_usd,
                    &to_db_opt("duration_ms", row.duration_ms)?,
                    &i64::from(row.turns),
                    &row.routed,
                    &to_db("recorded_ms", row.recorded_ms)?,
                ],
            )
            .map_err(db("outcome"))?;
            Ok(())
        })
    }

    fn outcome(&self, id: &str) -> Result<Option<crate::fleet::OutcomeRecord>, Error> {
        let row = self.query(|client| {
            client.query_opt(
                &format!("SELECT {OUTCOME_COLUMNS} FROM by_outcomes WHERE repo = $1 AND id = $2"),
                &[&self.repo, &id],
            )
        })?;
        row.as_ref().map(outcome_row).transpose()
    }

    fn outcomes(
        &self,
        kind: Option<crate::fleet::TaskKind>,
    ) -> Result<Vec<crate::fleet::OutcomeRecord>, Error> {
        let kind = kind.map(|k| k.as_str().to_owned());
        let rows = self.query(|client| {
            client.query(
                &format!(
                    "SELECT {OUTCOME_COLUMNS} FROM by_outcomes \
                     WHERE repo = $1 AND ($2::TEXT IS NULL OR kind = $2) \
                     ORDER BY recorded_ms, id"
                ),
                &[&self.repo, &kind],
            )
        })?;
        rows.iter().map(outcome_row).collect()
    }
}

const USAGE_COLUMNS: &str = "id, at_ms, branch, turn, subject, model, api, backend, input, \
     output, cache_read, cache_write, cache_write_1h, cost_usd, latency_ms, status, streamed";

fn usage_row(r: &Row) -> Result<crate::models::UsageRecord, Error> {
    let api: String = r.get(6);
    Ok(crate::models::UsageRecord {
        id: r.get(0),
        at_ms: from_db("at_ms", r.get(1))?,
        branch: r.get(2),
        turn: from_db_u32("turn", r.get::<_, i64>(3))?,
        subject: r.get(4),
        model: r.get(5),
        api: parse_text("api", &api)?,
        backend: r.get(7),
        tokens: crate::models::Tokens {
            input: from_db("input", r.get(8))?,
            output: from_db("output", r.get(9))?,
            cache_read: from_db("cache_read", r.get(10))?,
            cache_write: from_db("cache_write", r.get(11))?,
            cache_write_1h: from_db("cache_write_1h", r.get(12))?,
        },
        cost_usd: r.get(13),
        latency_ms: from_db("latency_ms", r.get(14))?,
        status: from_db_u16("status", r.get::<_, i64>(15))?,
        streamed: r.get(16),
    })
}

impl crate::models::UsageBackend for Postgres {
    fn put_usage(&self, row: &crate::models::UsageRecord) -> Result<(), Error> {
        // Accounting beside the branches, committed as event appends are.
        self.tx(false, |tx| {
            tx.execute(
                &format!(
                    "INSERT INTO by_model_usage (repo, {USAGE_COLUMNS}) VALUES ($1, $2, $3, $4, \
                     $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18) \
                     ON CONFLICT (repo, id) DO NOTHING"
                ),
                &[
                    &self.repo,
                    &row.id,
                    &to_db("at_ms", row.at_ms)?,
                    &row.branch,
                    &i64::from(row.turn),
                    &row.subject,
                    &row.model,
                    &row.api.as_str(),
                    &row.backend,
                    &to_db("input", row.tokens.input)?,
                    &to_db("output", row.tokens.output)?,
                    &to_db("cache_read", row.tokens.cache_read)?,
                    &to_db("cache_write", row.tokens.cache_write)?,
                    &to_db("cache_write_1h", row.tokens.cache_write_1h)?,
                    &row.cost_usd,
                    &to_db("latency_ms", row.latency_ms)?,
                    &i64::from(row.status),
                    &row.streamed,
                ],
            )
            .map_err(db("model usage"))?;
            Ok(())
        })
    }

    fn usage_since(&self, since_ms: u64) -> Result<Vec<crate::models::UsageRecord>, Error> {
        let since_ms_db = to_db("since_ms", since_ms)?;
        let rows = self.query(|client| {
            client.query(
                &format!(
                    "SELECT {USAGE_COLUMNS} FROM by_model_usage WHERE repo = $1 AND at_ms >= $2 \
                     ORDER BY at_ms, id"
                ),
                &[&self.repo, &since_ms_db],
            )
        })?;
        rows.iter().map(usage_row).collect()
    }
}

impl crate::effects::EffectBackend for Postgres {
    fn open_effect(&self, entry: &crate::effects::EffectEntry) -> Result<(), Error> {
        use crate::effects::EffectChange;
        let text = encode("effect", entry)?;
        let change = encode(
            "effect",
            &EffectChange::Opened {
                entry: Box::new(entry.clone()),
            },
        )?;
        // Durable before the call it records is made.
        self.tx(true, |tx| {
            tx.execute(
                "INSERT INTO by_effects (repo, id, branch, state, created_ms, entry) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &self.repo,
                    &entry.id,
                    &entry.branch,
                    &entry.state.as_str(),
                    &to_db("created_ms", entry.created_ms)?,
                    &text,
                ],
            )
            .map_err(db("effect"))?;
            tx.execute(
                "INSERT INTO by_effect_events (repo, id, at_ms, change) VALUES ($1, $2, $3, $4)",
                &[
                    &self.repo,
                    &entry.id,
                    &to_db("created_ms", entry.created_ms)?,
                    &change,
                ],
            )
            .map_err(db("effect"))?;
            Ok(())
        })
    }

    fn move_effect(
        &self,
        id: &str,
        from: &[crate::effects::EffectState],
        change: &crate::effects::EffectMove,
        at_ms: u64,
    ) -> Result<Option<crate::effects::EffectEntry>, Error> {
        use crate::effects::{EffectChange, EffectEntry};
        let event = encode("effect", &EffectChange::Moved(Box::new(change.clone())))?;
        self.tx(true, |tx| {
            let text: Option<String> = tx
                .query_opt(
                    "SELECT entry FROM by_effects WHERE repo = $1 AND id = $2 FOR UPDATE",
                    &[&self.repo, &id],
                )
                .map_err(db("effect"))?
                .map(|r| r.get(0));
            let Some(text) = text else {
                return Ok(None);
            };
            let mut entry: EffectEntry = decode("effect", &text)?;
            if !from.is_empty() && !from.contains(&entry.state) {
                return Ok(None);
            }
            change.apply(&mut entry, at_ms);
            let updated = encode("effect", &entry)?;
            tx.execute(
                "UPDATE by_effects SET state = $3, entry = $4 WHERE repo = $1 AND id = $2",
                &[&self.repo, &id, &entry.state.as_str(), &updated],
            )
            .map_err(db("effect"))?;
            tx.execute(
                "INSERT INTO by_effect_events (repo, id, at_ms, change) VALUES ($1, $2, $3, $4)",
                &[&self.repo, &id, &to_db("at_ms", at_ms)?, &event],
            )
            .map_err(db("effect"))?;
            Ok(Some(entry))
        })
    }

    fn effect(&self, id: &str) -> Result<Option<crate::effects::EffectEntry>, Error> {
        let row = self.query(|client| {
            client.query_opt(
                "SELECT entry FROM by_effects WHERE repo = $1 AND id = $2",
                &[&self.repo, &id],
            )
        })?;
        row.map(|r| decode("effect", &r.get::<_, String>(0)))
            .transpose()
    }

    fn effects(&self, branch: Option<&str>) -> Result<Vec<crate::effects::EffectEntry>, Error> {
        let rows = self.query(|client| {
            client.query(
                "SELECT entry FROM by_effects WHERE repo = $1 \
                 AND ($2::text IS NULL OR branch = $2) ORDER BY created_ms, id",
                &[&self.repo, &branch],
            )
        })?;
        rows.iter()
            .map(|r| decode("effect", &r.get::<_, String>(0)))
            .collect()
    }

    fn effect_events(&self, id: &str) -> Result<Vec<crate::effects::EffectEvent>, Error> {
        let rows = self.query(|client| {
            client.query(
                "SELECT seq, at_ms, change FROM by_effect_events WHERE repo = $1 AND id = $2 \
                 ORDER BY seq",
                &[&self.repo, &id],
            )
        })?;
        rows.iter()
            .map(|r| {
                Ok(crate::effects::EffectEvent {
                    seq: from_db("seq", r.get(0))?,
                    id: id.to_owned(),
                    at_ms: from_db("at_ms", r.get(1))?,
                    change: decode("effect event", &r.get::<_, String>(2))?,
                })
            })
            .collect()
    }

    fn put_ask(&self, ask: &crate::effects::ApprovalAsk) -> Result<(), Error> {
        let text = encode("approval", ask)?;
        self.tx(true, |tx| {
            tx.execute(
                "INSERT INTO by_approval_asks (repo, id, branch, created_ms, answered, ask) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &self.repo,
                    &ask.id,
                    &ask.branch,
                    &to_db("created_ms", ask.created_ms)?,
                    &ask.answer.is_some(),
                    &text,
                ],
            )
            .map_err(db("approval"))?;
            Ok(())
        })
    }

    fn answer_ask(
        &self,
        id: &str,
        answer: &crate::effects::AskAnswer,
    ) -> Result<Option<(crate::effects::ApprovalAsk, bool)>, Error> {
        self.tx(true, |tx| {
            let text: Option<String> = tx
                .query_opt(
                    "SELECT ask FROM by_approval_asks WHERE repo = $1 AND id = $2 FOR UPDATE",
                    &[&self.repo, &id],
                )
                .map_err(db("approval"))?
                .map(|r| r.get(0));
            let Some(text) = text else {
                return Ok(None);
            };
            let mut ask: crate::effects::ApprovalAsk = decode("approval", &text)?;
            if ask.answer.is_some() {
                return Ok(Some((ask, false)));
            }
            ask.answer = Some(answer.clone());
            let updated = encode("approval", &ask)?;
            tx.execute(
                "UPDATE by_approval_asks SET answered = TRUE, ask = $3 \
                 WHERE repo = $1 AND id = $2",
                &[&self.repo, &id, &updated],
            )
            .map_err(db("approval"))?;
            Ok(Some((ask, true)))
        })
    }

    fn ask(&self, id: &str) -> Result<Option<crate::effects::ApprovalAsk>, Error> {
        let row = self.query(|client| {
            client.query_opt(
                "SELECT ask FROM by_approval_asks WHERE repo = $1 AND id = $2",
                &[&self.repo, &id],
            )
        })?;
        row.map(|r| decode("approval", &r.get::<_, String>(0)))
            .transpose()
    }

    fn asks(&self, pending_only: bool) -> Result<Vec<crate::effects::ApprovalAsk>, Error> {
        let rows = self.query(|client| {
            client.query(
                "SELECT ask FROM by_approval_asks WHERE repo = $1 AND (NOT $2 OR NOT answered) \
                 ORDER BY created_ms, id",
                &[&self.repo, &pending_only],
            )
        })?;
        rows.iter()
            .map(|r| decode("approval", &r.get::<_, String>(0)))
            .collect()
    }
}

const KNOWLEDGE_COLUMNS: &str = "id, scope_path, scope_kind, text, source, status, created_ms, \
     adopted_by, decided_ms, note";

fn knowledge_row(r: &Row) -> Result<crate::KnowledgeEntry, Error> {
    Ok(crate::KnowledgeEntry {
        id: from_db("id", r.get(0))?,
        scope: crate::KnowledgeScope {
            path: r.get(1),
            kind: r
                .get::<_, Option<String>>(2)
                .map(|k| k.parse())
                .transpose()
                .map_err(|e| Error::State(format!("knowledge kind: {e}")))?,
        },
        text: r.get(3),
        source: decode("knowledge source", &r.get::<_, String>(4))?,
        status: r
            .get::<_, String>(5)
            .parse()
            .map_err(|e| Error::State(format!("knowledge status: {e}")))?,
        created_ms: from_db("created_ms", r.get(6))?,
        adopted_by: r.get(7),
        decided_ms: from_db_opt("decided_ms", r.get::<_, Option<i64>>(8))?,
        note: r.get(9),
    })
}

impl crate::knowledge::KnowledgeBackend for Postgres {
    fn add_knowledge(&self, entry: &crate::KnowledgeEntry) -> Result<crate::KnowledgeEntry, Error> {
        let source = encode("knowledge source", &entry.source)?;
        let created = match entry.created_ms {
            0 => now_ms(),
            at => at,
        };
        let id: i64 = self.tx(true, |tx| {
            Ok(tx
                .query_one(
                    "INSERT INTO by_knowledge (repo, scope_path, scope_kind, text, source, status, \
                     created_ms, adopted_by, decided_ms, note) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING id",
                    &[
                        &self.repo,
                        &entry.scope.path,
                        &entry.scope.kind.map(|k| k.as_str()),
                        &entry.text,
                        &source,
                        &entry.status.as_str(),
                        &to_db("created", created)?,
                        &entry.adopted_by,
                        &to_db_opt("decided_ms", entry.decided_ms)?,
                        &entry.note,
                    ],
                )
                .map_err(db("knowledge"))?
                .get(0))
        })?;
        Ok(crate::KnowledgeEntry {
            id: from_db("id", id)?,
            created_ms: created,
            ..entry.clone()
        })
    }

    fn knowledge(&self, id: u64) -> Result<Option<crate::KnowledgeEntry>, Error> {
        let id_db = to_db("id", id)?;
        let row = self.query(|client| {
            client.query_opt(
                &format!(
                    "SELECT {KNOWLEDGE_COLUMNS} FROM by_knowledge WHERE repo = $1 AND id = $2"
                ),
                &[&self.repo, &id_db],
            )
        })?;
        row.as_ref().map(knowledge_row).transpose()
    }

    fn knowledge_entries(&self) -> Result<Vec<crate::KnowledgeEntry>, Error> {
        let rows = self.query(|client| {
            client.query(
                &format!(
                    "SELECT {KNOWLEDGE_COLUMNS} FROM by_knowledge WHERE repo = $1 ORDER BY id"
                ),
                &[&self.repo],
            )
        })?;
        rows.iter().map(knowledge_row).collect()
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
                    "UPDATE by_knowledge SET scope_path = $3, scope_kind = $4, text = $5, \
                     source = $6, status = $7, adopted_by = $8, decided_ms = $9, note = $10 \
                     WHERE repo = $1 AND id = $2 AND status = $11",
                    &[
                        &self.repo,
                        &to_db("id", entry.id)?,
                        &entry.scope.path,
                        &entry.scope.kind.map(|k| k.as_str()),
                        &entry.text,
                        &source,
                        &entry.status.as_str(),
                        &entry.adopted_by,
                        &to_db_opt("decided_ms", entry.decided_ms)?,
                        &entry.note,
                        &expected.as_str(),
                    ],
                )
                .map_err(db("knowledge"))?;
            Ok(changed == 1)
        })
    }

    fn remove_knowledge(&self, id: u64) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let changed = tx
                .execute(
                    "DELETE FROM by_knowledge WHERE repo = $1 AND id = $2",
                    &[&self.repo, &to_db("id", id)?],
                )
                .map_err(db("knowledge"))?;
            Ok(changed == 1)
        })
    }
}

const SLOT_COLUMNS: &str = "id, place, recipe, state, base, path, detail, host, pid, pid_start, \
     branch, created_ms, changed_ms";

fn slot_row(r: &Row) -> Result<SlotRow, Error> {
    Ok(SlotRow {
        id: r.get(0),
        place: r.get(1),
        recipe: r.get(2),
        state: SlotState::parse(r.get(3))?,
        base: r.get(4),
        path: r.get(5),
        detail: r.get(6),
        host: r.get(7),
        pid: from_db_u32("pid", r.get::<_, i64>(8))?,
        start: r.get(9),
        branch: r.get(10),
        created_ms: from_db("created_ms", r.get(11))?,
        changed_ms: from_db("changed_ms", r.get(12))?,
    })
}

impl PoolBackend for Postgres {
    fn insert_slot(&self, row: &SlotRow) -> Result<(), Error> {
        self.tx(true, |tx| {
            tx.execute(
                &format!(
                    "INSERT INTO by_pool_slots (repo, {SLOT_COLUMNS}) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)"
                ),
                &[
                    &self.repo,
                    &row.id,
                    &row.place,
                    &row.recipe,
                    &row.state.as_str(),
                    &row.base,
                    &row.path,
                    &row.detail,
                    &row.host,
                    &i64::from(row.pid),
                    &row.start,
                    &row.branch,
                    &to_db("created_ms", row.created_ms)?,
                    &to_db("changed_ms", row.changed_ms)?,
                ],
            )
            .map_err(db("pool slot"))?;
            Ok(())
        })
    }

    fn slots(&self, place: Option<&str>) -> Result<Vec<SlotRow>, Error> {
        let rows = self.query(|client| {
            client.query(
                &format!(
                    "SELECT {SLOT_COLUMNS} FROM by_pool_slots \
                     WHERE repo = $1 AND ($2::text IS NULL OR place = $2) \
                     ORDER BY created_ms, id"
                ),
                &[&self.repo, &place],
            )
        })?;
        rows.iter().map(slot_row).collect()
    }

    fn update_slot(&self, row: &SlotRow, expected: SlotState) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let changed = tx
                .execute(
                    "UPDATE by_pool_slots SET state = $3, base = $4, path = $5, detail = $6, \
                     host = $7, pid = $8, pid_start = $9, branch = $10, changed_ms = $11 \
                     WHERE repo = $1 AND id = $2 AND state = $12",
                    &[
                        &self.repo,
                        &row.id,
                        &row.state.as_str(),
                        &row.base,
                        &row.path,
                        &row.detail,
                        &row.host,
                        &i64::from(row.pid),
                        &row.start,
                        &row.branch,
                        &to_db("changed_ms", row.changed_ms)?,
                        &expected.as_str(),
                    ],
                )
                .map_err(db("pool slot"))?;
            Ok(changed == 1)
        })
    }

    fn delete_slot(&self, id: &str) -> Result<bool, Error> {
        self.tx(true, |tx| {
            let changed = tx
                .execute(
                    "DELETE FROM by_pool_slots WHERE repo = $1 AND id = $2",
                    &[&self.repo, &id],
                )
                .map_err(db("pool slot"))?;
            Ok(changed == 1)
        })
    }
}

#[cfg(test)]
mod redact_tests {
    use super::redact;

    #[test]
    fn a_password_is_hidden_and_a_url_without_one_is_left_alone() {
        assert_eq!(
            redact("postgres://by:s3cret@db.example:5432/yard"),
            "postgres://by:***@db.example:5432/yard"
        );
        assert_eq!(
            redact("postgres://by@db.example/yard"),
            "postgres://by@db.example/yard"
        );
        assert_eq!(redact("host=db user=by"), "host=db user=by");
    }

    #[test]
    fn a_multi_host_url_is_masked() {
        assert_eq!(
            redact("postgresql://by:pw@h1:5432,h2:5432/db"),
            "postgresql://by:***@h1:5432,h2:5432/db"
        );
    }

    #[test]
    fn a_password_with_a_raw_delimiter_is_masked() {
        for pw in [
            "a/b", "a#b", "a?b", "12/b", "12#b", "12?b", "a/b/c", "p@ss/w",
        ] {
            let url = format!("postgres://by:{pw}@db:5432/yard");
            let shown = redact(&url);
            assert_eq!(shown, "postgres://by:***@db:5432/yard", "{url}");
        }
        assert_eq!(
            redact("postgres://by:a/b@db/yard?sslmode=require"),
            "postgres://by:***@db/yard?sslmode=require"
        );
    }

    #[test]
    fn an_empty_host_url_is_masked() {
        assert_eq!(
            redact("postgres://by:pw@/db?host=/run/pg"),
            "postgres://by:***@/db?host=/run/pg"
        );
    }

    #[test]
    fn an_at_sign_after_the_authority_is_not_a_password() {
        assert_eq!(
            redact("postgres://by@db/yard?application_name=x"),
            "postgres://by@db/yard?application_name=x"
        );
    }
}
