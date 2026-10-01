//! Where triggers and their runs persist: the same database as the
//! operation registry, in tables of their own.
//!
//! Every change is one transaction, and every claim is a compare-and-set,
//! so two dispatchers on one database never both fire a schedule time or
//! a pending run:
//!
//! - [`TriggerStore::claim_schedule`] moves a trigger's `next_due_ms`
//!   from the value the dispatcher read to the next time, and records the
//!   run for the time it claimed (and any it missed), in one transaction
//!   that only succeeds while `next_due_ms` is still what was read.
//! - [`TriggerStore::claim_run`] takes a pending run whose claim expired
//!   (or that nobody claimed) under a new fence, its attempt number;
//!   [`TriggerStore::finish_run`] records its outcome only while that
//!   fence holds.
//! - A run's key (`event:<id>`, `schedule:<ms>`) is unique per trigger,
//!   so a redelivered webhook finds the run its first delivery recorded.
//!
//! Counting failures toward auto-pause happens in the transaction that
//! records a run's failure or its task's outcome.
//!
//! Times are the dispatchers' clocks ([`super::Clock`]), not the
//! database's: keep the hosts' clocks synchronized.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use branchyard_client::triggers::{RunOutcome, RunState, TriggerRun};
use rusqlite::OptionalExtension;

use super::StoredTrigger;

/// What [`TriggerStore::record`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum Recorded {
    Inserted,
    /// The trigger already has a run with this key.
    Existing(Box<TriggerRun>),
}

/// Whether recording an outcome paused the trigger.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Accounted {
    /// The write happened (the claim or the unsettled state still held).
    pub written: bool,
    /// Why the trigger was paused by it, if it was.
    pub paused: Option<String>,
}

/// Triggers and their runs.
pub trait TriggerStore: Send + Sync {
    /// Add a trigger; false when its tenant already has one of its name.
    fn create(&self, trigger: &StoredTrigger) -> io::Result<bool>;
    fn get(&self, id: &str) -> io::Result<Option<StoredTrigger>>;
    /// Triggers whose ID or name is `key`, in `tenant` or in any.
    fn find(&self, tenant: Option<&str>, key: &str) -> io::Result<Vec<StoredTrigger>>;
    /// Every trigger of `tenant`, or of every tenant, oldest first.
    fn list(&self, tenant: Option<&str>) -> io::Result<Vec<StoredTrigger>>;
    /// Set whether it is enabled, why not, its failure count and its next
    /// due time; false when it does not exist.
    fn set_state(
        &self,
        id: &str,
        enabled: bool,
        paused_reason: Option<&str>,
        failures: u32,
        next_due_ms: Option<u64>,
    ) -> io::Result<bool>;
    fn set_secret(&self, id: &str, secret: &str) -> io::Result<bool>;
    /// Remove it and its runs; false when it did not exist.
    fn remove(&self, id: &str) -> io::Result<bool>;

    /// Enabled schedules of `repos` due at `now_ms`.
    fn due(&self, repos: &[String], now_ms: u64) -> io::Result<Vec<StoredTrigger>>;
    /// Move `id`'s next due time from `expected` to `next`, and record
    /// `runs` (a pending one claimed by `claimer` until `until_ms`, under
    /// fence 1), in one transaction; false when another dispatcher moved
    /// it first, the trigger was disabled, or it is gone.
    fn claim_schedule(
        &self,
        id: &str,
        expected: u64,
        next: Option<u64>,
        runs: &[TriggerRun],
        claimer: &str,
        until_ms: u64,
    ) -> io::Result<bool>;
    /// Record a run unless the trigger has one with its key already. A
    /// pending run is left unclaimed, for any dispatcher.
    fn record(&self, run: &TriggerRun) -> io::Result<Recorded>;
    /// Claim the oldest pending run of `repos` that nobody holds, or whose
    /// claim expired by `now_ms`, until `until_ms`; with its fence.
    fn claim_run(
        &self,
        repos: &[String],
        claimer: &str,
        now_ms: u64,
        until_ms: u64,
    ) -> io::Result<Option<(TriggerRun, i64)>>;
    /// Hold a claimed, still pending run only until `until_ms` while
    /// `fence` holds: any dispatcher may claim it after.
    fn defer_run(&self, run_id: &str, fence: i64, until_ms: u64) -> io::Result<bool>;
    /// Give back every pending run's claim: only for a store no other
    /// process dispatches from (a data directory's SQLite, which one
    /// server holds), whose claims were its predecessor's.
    fn release_claims(&self) -> io::Result<()>;
    /// Record a claimed run's outcome while `fence` holds. A `failed` run
    /// counts toward `pause_after` failures in a row (0: never pause).
    fn finish_run(&self, run: &TriggerRun, fence: i64, pause_after: u32) -> io::Result<Accounted>;
    /// Fired runs of `repos` whose task has not been seen to end.
    fn unsettled(&self, repos: &[String]) -> io::Result<Vec<TriggerRun>>;
    /// Record how a fired run's task ended, once; a failure counts toward
    /// pausing, a success starts the count over.
    fn settle(&self, run_id: &str, outcome: &RunOutcome, pause_after: u32)
        -> io::Result<Accounted>;
    /// A trigger's runs, newest first.
    fn runs(&self, trigger_id: &str, limit: usize) -> io::Result<Vec<TriggerRun>>;
}

fn parse<T: serde::de::DeserializeOwned>(what: &str, id: &str, body: &str) -> io::Result<T> {
    serde_json::from_str(body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{what} {id}: {e}")))
}

fn body<T: serde::Serialize>(value: &T) -> io::Result<String> {
    serde_json::to_string(value).map_err(io::Error::other)
}

fn needs_settling(run: &TriggerRun) -> bool {
    run.state == RunState::Fired && run.outcome.is_none()
}

fn pause_reason(failures: u32, last: &str) -> String {
    format!("paused after {failures} failed runs in a row; the last: {last}")
}

fn i(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Columns read back into a [`StoredTrigger`].
struct Row {
    id: String,
    tenant: String,
    enabled: bool,
    paused_reason: Option<String>,
    failures: i64,
    next_due_ms: Option<i64>,
    secret: Option<String>,
    body: String,
}

impl Row {
    fn into_trigger(self) -> io::Result<StoredTrigger> {
        let mut t: StoredTrigger = parse("trigger", &self.id, &self.body)?;
        t.id = self.id;
        t.tenant = self.tenant;
        t.enabled = self.enabled;
        t.paused_reason = self.paused_reason;
        t.failures = u32::try_from(self.failures).unwrap_or(0);
        t.next_due_ms = self.next_due_ms.map(|v| v.max(0) as u64);
        t.secret = self.secret;
        Ok(t)
    }
}

const TRIGGER_COLUMNS: &str =
    "id, tenant, enabled, paused_reason, failures, next_due_ms, secret, body";

/// The store a server with `config` keeps its triggers in: PostgreSQL
/// with `database`, else the data directory's `state.db`.
pub fn open(config: &crate::Config) -> Result<std::sync::Arc<dyn TriggerStore>, String> {
    #[cfg(feature = "postgres")]
    if let Some(url) = &config.database {
        return PostgresTriggers::open(url)
            .map(|s| std::sync::Arc::new(s) as std::sync::Arc<dyn TriggerStore>)
            .map_err(|e| format!("triggers in the database: {e}"));
    }
    #[cfg(not(feature = "postgres"))]
    if config.database.is_some() {
        return Err(
            "this build has no PostgreSQL support; build with the postgres feature \
                    to use --database"
                .into(),
        );
    }
    let path = SqliteTriggers::path_in(&config.data_dir);
    SqliteTriggers::open(&path)
        .map(|s| std::sync::Arc::new(s) as std::sync::Arc<dyn TriggerStore>)
        .map_err(|e| format!("triggers in {}: {e}", path.display()))
}

// ---------------------------------------------------------------------
// SQLite

/// Triggers in the data directory's `state.db`, beside the operation
/// registry, on a connection of their own.
pub struct SqliteTriggers {
    conn: Mutex<rusqlite::Connection>,
}

const SQLITE_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS triggers (
        id TEXT PRIMARY KEY,
        seq INTEGER NOT NULL,
        tenant TEXT NOT NULL,
        name TEXT NOT NULL,
        repo TEXT NOT NULL,
        schedule INTEGER NOT NULL,
        enabled INTEGER NOT NULL,
        paused_reason TEXT,
        failures INTEGER NOT NULL DEFAULT 0,
        next_due_ms INTEGER,
        secret TEXT,
        body TEXT NOT NULL
    );
    CREATE UNIQUE INDEX IF NOT EXISTS triggers_name ON triggers (tenant, name);
    CREATE TABLE IF NOT EXISTS trigger_runs (
        id TEXT PRIMARY KEY,
        seq INTEGER NOT NULL,
        trigger_id TEXT NOT NULL,
        key TEXT NOT NULL,
        repo TEXT NOT NULL,
        state TEXT NOT NULL,
        settled INTEGER NOT NULL,
        attempt INTEGER NOT NULL DEFAULT 0,
        claimer TEXT,
        claimed_until INTEGER,
        body TEXT NOT NULL
    );
    CREATE UNIQUE INDEX IF NOT EXISTS trigger_runs_key ON trigger_runs (trigger_id, key);
    CREATE INDEX IF NOT EXISTS trigger_runs_state ON trigger_runs (state, settled);
";

fn sql(error: rusqlite::Error) -> io::Error {
    io::Error::other(format!("trigger store: {error}"))
}

fn sqlite_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        tenant: r.get(1)?,
        enabled: r.get::<_, i64>(2)? != 0,
        paused_reason: r.get(3)?,
        failures: r.get(4)?,
        next_due_ms: r.get(5)?,
        secret: r.get(6)?,
        body: r.get(7)?,
    })
}

/// `?, ?, ...` for `n` values starting at parameter `from`.
fn placeholders(from: usize, n: usize) -> String {
    (from..from + n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl SqliteTriggers {
    /// Open the database at `path` (made if missing) and its tables.
    pub fn open(path: &Path) -> io::Result<SqliteTriggers> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = rusqlite::Connection::open(path).map_err(sql)?;
        conn.busy_timeout(Duration::from_secs(30)).map_err(sql)?;
        // The registry switches the file to write-ahead logging; a local
        // `by trigger` may open it first.
        let _: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .map_err(sql)?;
        conn.execute_batch("PRAGMA synchronous = FULL;")
            .map_err(sql)?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql)?;
        tx.execute_batch(SQLITE_SCHEMA).map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(SqliteTriggers {
            conn: Mutex::new(conn),
        })
    }

    /// The tables in a database of their own, in memory: for tests.
    pub fn memory() -> SqliteTriggers {
        let conn = rusqlite::Connection::open_in_memory().expect("an in-memory database");
        conn.execute_batch(SQLITE_SCHEMA).expect("the schema");
        SqliteTriggers {
            conn: Mutex::new(conn),
        }
    }

    /// The default place: `DATA-DIR/state.db`.
    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join("state.db")
    }

    fn immediate<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> io::Result<(T, bool)>,
    ) -> io::Result<T> {
        let mut conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
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

    fn triggers(
        &self,
        filter: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> io::Result<Vec<StoredTrigger>> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut statement = conn
            .prepare(&format!(
                "SELECT {TRIGGER_COLUMNS} FROM triggers {filter} ORDER BY seq"
            ))
            .map_err(sql)?;
        let rows: Vec<Row> = statement
            .query_map(params, sqlite_row)
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)?;
        rows.into_iter().map(Row::into_trigger).collect()
    }

    fn runs_where(
        &self,
        filter: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> io::Result<Vec<TriggerRun>> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut statement = conn
            .prepare(&format!("SELECT id, body FROM trigger_runs {filter}"))
            .map_err(sql)?;
        let rows: Vec<(String, String)> = statement
            .query_map(params, |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)?;
        rows.iter().map(|(id, b)| parse("run", id, b)).collect()
    }
}

fn sqlite_insert_run(
    tx: &rusqlite::Transaction<'_>,
    run: &TriggerRun,
    repo: &str,
    claim: Option<(&str, u64)>,
) -> io::Result<bool> {
    let (attempt, claimer, until) = match claim {
        Some((claimer, until)) => (1i64, Some(claimer), Some(i(until))),
        None => (0, None, None),
    };
    let rows = tx
        .execute(
            "INSERT INTO trigger_runs (id, seq, trigger_id, key, repo, state, settled, attempt, \
             claimer, claimed_until, body) \
             VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM trigger_runs), ?2, ?3, ?4, ?5, \
             ?6, ?7, ?8, ?9, ?10) ON CONFLICT DO NOTHING",
            rusqlite::params![
                run.id,
                run.trigger,
                run.key,
                repo,
                run.state.as_str(),
                i64::from(!needs_settling(run) && run.state != RunState::Pending),
                attempt,
                claimer,
                until,
                body(run)?,
            ],
        )
        .map_err(sql)?;
    Ok(rows == 1)
}

/// Count a failure (or a success) of `trigger_id`'s run, pausing it at
/// `pause_after` failures in a row.
fn sqlite_account(
    tx: &rusqlite::Transaction<'_>,
    trigger_id: &str,
    failure: Option<&str>,
    pause_after: u32,
) -> io::Result<Option<String>> {
    let Some(reason) = failure else {
        tx.execute(
            "UPDATE triggers SET failures = 0 WHERE id = ?1",
            [trigger_id],
        )
        .map_err(sql)?;
        return Ok(None);
    };
    tx.execute(
        "UPDATE triggers SET failures = failures + 1 WHERE id = ?1",
        [trigger_id],
    )
    .map_err(sql)?;
    let row: Option<(i64, i64)> = tx
        .query_row(
            "SELECT failures, enabled FROM triggers WHERE id = ?1",
            [trigger_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql)?;
    let Some((failures, enabled)) = row else {
        return Ok(None);
    };
    if pause_after == 0 || enabled == 0 || failures < i64::from(pause_after) {
        return Ok(None);
    }
    let why = pause_reason(failures as u32, reason);
    tx.execute(
        "UPDATE triggers SET enabled = 0, next_due_ms = NULL, paused_reason = ?2 WHERE id = ?1",
        rusqlite::params![trigger_id, why],
    )
    .map_err(sql)?;
    Ok(Some(why))
}

fn failure_of(run: &TriggerRun) -> Option<String> {
    match run.state {
        RunState::Failed => Some(run.reason.clone().unwrap_or_else(|| "failed".into())),
        _ => None,
    }
}

impl TriggerStore for SqliteTriggers {
    fn create(&self, t: &StoredTrigger) -> io::Result<bool> {
        self.immediate(|tx| {
            let rows = tx
                .execute(
                    "INSERT INTO triggers (id, seq, tenant, name, repo, schedule, enabled, \
                     paused_reason, failures, next_due_ms, secret, body) \
                     VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM triggers), ?2, ?3, ?4, \
                     ?5, ?6, ?7, ?8, ?9, ?10, ?11) ON CONFLICT DO NOTHING",
                    rusqlite::params![
                        t.id,
                        t.tenant,
                        t.spec.name,
                        t.spec.repo,
                        i64::from(t.spec.when.is_schedule()),
                        i64::from(t.enabled),
                        t.paused_reason,
                        i64::from(t.failures),
                        t.next_due_ms.map(i),
                        t.secret,
                        body(t)?,
                    ],
                )
                .map_err(sql)?;
            Ok((rows == 1, true))
        })
    }

    fn get(&self, id: &str) -> io::Result<Option<StoredTrigger>> {
        Ok(self.triggers("WHERE id = ?1", &[&id])?.into_iter().next())
    }

    fn find(&self, tenant: Option<&str>, key: &str) -> io::Result<Vec<StoredTrigger>> {
        match tenant {
            Some(tenant) => self.triggers(
                "WHERE tenant = ?1 AND (id = ?2 OR name = ?2)",
                &[&tenant, &key],
            ),
            None => self.triggers("WHERE id = ?1 OR name = ?1", &[&key]),
        }
    }

    fn list(&self, tenant: Option<&str>) -> io::Result<Vec<StoredTrigger>> {
        match tenant {
            Some(tenant) => self.triggers("WHERE tenant = ?1", &[&tenant]),
            None => self.triggers("", &[]),
        }
    }

    fn set_state(
        &self,
        id: &str,
        enabled: bool,
        paused_reason: Option<&str>,
        failures: u32,
        next_due_ms: Option<u64>,
    ) -> io::Result<bool> {
        self.immediate(|tx| {
            let rows = tx
                .execute(
                    "UPDATE triggers SET enabled = ?2, paused_reason = ?3, failures = ?4, \
                     next_due_ms = ?5 WHERE id = ?1",
                    rusqlite::params![
                        id,
                        i64::from(enabled),
                        paused_reason,
                        i64::from(failures),
                        next_due_ms.map(i)
                    ],
                )
                .map_err(sql)?;
            Ok((rows == 1, true))
        })
    }

    fn set_secret(&self, id: &str, secret: &str) -> io::Result<bool> {
        self.immediate(|tx| {
            let rows = tx
                .execute(
                    "UPDATE triggers SET secret = ?2 WHERE id = ?1",
                    [id, secret],
                )
                .map_err(sql)?;
            Ok((rows == 1, true))
        })
    }

    fn remove(&self, id: &str) -> io::Result<bool> {
        self.immediate(|tx| {
            tx.execute("DELETE FROM trigger_runs WHERE trigger_id = ?1", [id])
                .map_err(sql)?;
            let rows = tx
                .execute("DELETE FROM triggers WHERE id = ?1", [id])
                .map_err(sql)?;
            Ok((rows == 1, true))
        })
    }

    fn due(&self, repos: &[String], now_ms: u64) -> io::Result<Vec<StoredTrigger>> {
        if repos.is_empty() {
            return Ok(Vec::new());
        }
        let filter = format!(
            "WHERE schedule = 1 AND enabled = 1 AND next_due_ms IS NOT NULL \
             AND next_due_ms <= ?1 AND repo IN ({})",
            placeholders(2, repos.len())
        );
        let now = i(now_ms);
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&now];
        params.extend(repos.iter().map(|r| r as &dyn rusqlite::ToSql));
        self.triggers(&filter, &params)
    }

    fn claim_schedule(
        &self,
        id: &str,
        expected: u64,
        next: Option<u64>,
        runs: &[TriggerRun],
        claimer: &str,
        until_ms: u64,
    ) -> io::Result<bool> {
        self.immediate(|tx| {
            let repo: Option<String> = tx
                .query_row(
                    "UPDATE triggers SET next_due_ms = ?3 \
                     WHERE id = ?1 AND enabled = 1 AND next_due_ms = ?2 RETURNING repo",
                    rusqlite::params![id, i(expected), next.map(i)],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            let Some(repo) = repo else {
                return Ok((false, false));
            };
            for run in runs {
                let claim = (run.state == RunState::Pending).then_some((claimer, until_ms));
                sqlite_insert_run(tx, run, &repo, claim)?;
            }
            Ok((true, true))
        })
    }

    fn record(&self, run: &TriggerRun) -> io::Result<Recorded> {
        self.immediate(|tx| {
            let repo: Option<String> = tx
                .query_row(
                    "SELECT repo FROM triggers WHERE id = ?1",
                    [&run.trigger],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            let Some(repo) = repo else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no trigger {}", run.trigger),
                ));
            };
            if sqlite_insert_run(tx, run, &repo, None)? {
                return Ok((Recorded::Inserted, true));
            }
            let existing: String = tx
                .query_row(
                    "SELECT body FROM trigger_runs WHERE trigger_id = ?1 AND key = ?2",
                    [&run.trigger, &run.key],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            Ok((
                Recorded::Existing(Box::new(parse("run", &run.key, &existing)?)),
                false,
            ))
        })
    }

    fn claim_run(
        &self,
        repos: &[String],
        claimer: &str,
        now_ms: u64,
        until_ms: u64,
    ) -> io::Result<Option<(TriggerRun, i64)>> {
        if repos.is_empty() {
            return Ok(None);
        }
        self.immediate(|tx| {
            let query = format!(
                "SELECT id, attempt FROM trigger_runs WHERE state = 'pending' \
                 AND (claimed_until IS NULL OR claimed_until < ?1) AND repo IN ({}) \
                 ORDER BY seq LIMIT 1",
                placeholders(2, repos.len())
            );
            let now = i(now_ms);
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![&now];
            params.extend(repos.iter().map(|r| r as &dyn rusqlite::ToSql));
            let found: Option<(String, i64)> = tx
                .query_row(&query, params.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
                .map_err(sql)?;
            let Some((id, attempt)) = found else {
                return Ok((None, false));
            };
            let body: String = tx
                .query_row(
                    "UPDATE trigger_runs SET attempt = attempt + 1, claimer = ?3, \
                     claimed_until = ?4 WHERE id = ?1 AND attempt = ?2 RETURNING body",
                    rusqlite::params![id, attempt, claimer, i(until_ms)],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            Ok((Some((parse("run", &id, &body)?, attempt + 1)), true))
        })
    }

    fn defer_run(&self, run_id: &str, fence: i64, until_ms: u64) -> io::Result<bool> {
        self.immediate(|tx| {
            let rows = tx
                .execute(
                    "UPDATE trigger_runs SET claimed_until = ?3 \
                     WHERE id = ?1 AND attempt = ?2 AND state = 'pending'",
                    rusqlite::params![run_id, fence, i(until_ms)],
                )
                .map_err(sql)?;
            Ok((rows == 1, true))
        })
    }

    fn release_claims(&self) -> io::Result<()> {
        self.immediate(|tx| {
            tx.execute(
                "UPDATE trigger_runs SET claimer = NULL, claimed_until = NULL \
                 WHERE state = 'pending'",
                [],
            )
            .map_err(sql)?;
            Ok(((), true))
        })
    }

    fn finish_run(&self, run: &TriggerRun, fence: i64, pause_after: u32) -> io::Result<Accounted> {
        self.immediate(|tx| {
            let rows = tx
                .execute(
                    "UPDATE trigger_runs SET state = ?3, settled = ?4, claimed_until = NULL, \
                     body = ?5 WHERE id = ?1 AND attempt = ?2 AND state = 'pending'",
                    rusqlite::params![
                        run.id,
                        fence,
                        run.state.as_str(),
                        i64::from(!needs_settling(run)),
                        body(run)?
                    ],
                )
                .map_err(sql)?;
            if rows != 1 {
                return Ok((Accounted::default(), false));
            }
            let paused = match failure_of(run) {
                Some(reason) => sqlite_account(tx, &run.trigger, Some(&reason), pause_after)?,
                None => None,
            };
            Ok((
                Accounted {
                    written: true,
                    paused,
                },
                true,
            ))
        })
    }

    fn unsettled(&self, repos: &[String]) -> io::Result<Vec<TriggerRun>> {
        if repos.is_empty() {
            return Ok(Vec::new());
        }
        let filter = format!(
            "WHERE state = 'fired' AND settled = 0 AND repo IN ({}) ORDER BY seq",
            placeholders(1, repos.len())
        );
        let params: Vec<&dyn rusqlite::ToSql> =
            repos.iter().map(|r| r as &dyn rusqlite::ToSql).collect();
        self.runs_where(&filter, &params)
    }

    fn settle(
        &self,
        run_id: &str,
        outcome: &RunOutcome,
        pause_after: u32,
    ) -> io::Result<Accounted> {
        self.immediate(|tx| {
            let found: Option<String> = tx
                .query_row(
                    "SELECT body FROM trigger_runs WHERE id = ?1 AND settled = 0",
                    [run_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            let Some(found) = found else {
                return Ok((Accounted::default(), false));
            };
            let mut run: TriggerRun = parse("run", run_id, &found)?;
            run.outcome = Some(outcome.clone());
            tx.execute(
                "UPDATE trigger_runs SET settled = 1, body = ?2 WHERE id = ?1",
                rusqlite::params![run_id, body(&run)?],
            )
            .map_err(sql)?;
            let failure = (!outcome.ok).then(|| outcome.detail.clone());
            let paused = sqlite_account(tx, &run.trigger, failure.as_deref(), pause_after)?;
            Ok((
                Accounted {
                    written: true,
                    paused,
                },
                true,
            ))
        })
    }

    fn runs(&self, trigger_id: &str, limit: usize) -> io::Result<Vec<TriggerRun>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.runs_where(
            "WHERE trigger_id = ?1 ORDER BY seq DESC LIMIT ?2",
            &[&trigger_id, &limit],
        )
    }
}

// ---------------------------------------------------------------------
// PostgreSQL

/// Triggers in PostgreSQL, beside the operation registry's tables, on a
/// connection of their own.
#[cfg(feature = "postgres")]
pub struct PostgresTriggers {
    url: String,
    conn: Mutex<Option<postgres::Client>>,
}

/// The tables, one object per step, made like the registry's
/// (`crate::store::PostgresStore::open`): only the missing steps run, each
/// alone in a transaction under one advisory lock, so opening never holds
/// one table's lock while waiting for another's.
#[cfg(feature = "postgres")]
const PG_SCHEMA: &[(&str, &str)] = &[
    (
        "by_triggers",
        "CREATE TABLE IF NOT EXISTS by_triggers (
            id TEXT PRIMARY KEY,
            seq BIGINT GENERATED ALWAYS AS IDENTITY,
            tenant TEXT NOT NULL,
            name TEXT NOT NULL,
            repo TEXT NOT NULL,
            schedule BOOLEAN NOT NULL,
            enabled BOOLEAN NOT NULL,
            paused_reason TEXT,
            failures BIGINT NOT NULL DEFAULT 0,
            next_due_ms BIGINT,
            secret TEXT,
            body TEXT NOT NULL
        )",
    ),
    (
        "by_triggers_name",
        "CREATE UNIQUE INDEX IF NOT EXISTS by_triggers_name ON by_triggers (tenant, name)",
    ),
    (
        "by_trigger_runs",
        "CREATE TABLE IF NOT EXISTS by_trigger_runs (
            id TEXT PRIMARY KEY,
            seq BIGINT GENERATED ALWAYS AS IDENTITY,
            trigger_id TEXT NOT NULL,
            key TEXT NOT NULL,
            repo TEXT NOT NULL,
            state TEXT NOT NULL,
            settled BOOLEAN NOT NULL,
            attempt BIGINT NOT NULL DEFAULT 0,
            claimer TEXT,
            claimed_until BIGINT,
            body TEXT NOT NULL
        )",
    ),
    (
        "by_trigger_runs_key",
        "CREATE UNIQUE INDEX IF NOT EXISTS by_trigger_runs_key ON by_trigger_runs (trigger_id, key)",
    ),
    (
        "by_trigger_runs_state",
        "CREATE INDEX IF NOT EXISTS by_trigger_runs_state ON by_trigger_runs (state, settled)",
    ),
];

/// Which of `names` (see [`PG_SCHEMA`]) are missing, read from the
/// catalogs without locking any table; the registry's own check
/// (`crate::store`), repeated here so each store makes its own tables.
#[cfg(feature = "postgres")]
fn pg_missing(
    c: &mut impl postgres::GenericClient,
    names: &[&str],
) -> Result<Vec<String>, postgres::Error> {
    let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    let rows = c.query(
        "SELECT n FROM unnest($1::text[]) WITH ORDINALITY AS t (n, i) \
         WHERE to_regclass(n) IS NULL ORDER BY i",
        &[&names],
    )?;
    Ok(rows.iter().map(|row| row.get(0)).collect())
}

#[cfg(feature = "postgres")]
fn pg_row(row: &postgres::Row) -> Row {
    Row {
        id: row.get(0),
        tenant: row.get(1),
        enabled: row.get(2),
        paused_reason: row.get(3),
        failures: row.get(4),
        next_due_ms: row.get(5),
        secret: row.get(6),
        body: row.get(7),
    }
}

#[cfg(feature = "postgres")]
fn pg_triggers(rows: &[postgres::Row]) -> io::Result<Vec<StoredTrigger>> {
    rows.iter().map(|r| pg_row(r).into_trigger()).collect()
}

#[cfg(feature = "postgres")]
fn pg_runs(rows: &[postgres::Row]) -> io::Result<Vec<TriggerRun>> {
    rows.iter()
        .map(|r| {
            let (id, b): (String, String) = (r.get(0), r.get(1));
            parse("run", &id, &b)
        })
        .collect()
}

#[cfg(feature = "postgres")]
fn pg_insert_run(
    tx: &mut postgres::Transaction<'_>,
    run: &TriggerRun,
    repo: &str,
    claim: Option<(&str, u64)>,
) -> Result<bool, postgres::Error> {
    let (attempt, claimer, until) = match claim {
        Some((claimer, until)) => (1i64, Some(claimer.to_owned()), Some(i(until))),
        None => (0, None, None),
    };
    let text = serde_json::to_string(run).expect("a run serializes");
    let rows = tx.execute(
        "INSERT INTO by_trigger_runs (id, trigger_id, key, repo, state, settled, attempt, \
         claimer, claimed_until, body) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         ON CONFLICT DO NOTHING",
        &[
            &run.id,
            &run.trigger,
            &run.key,
            &repo,
            &run.state.as_str(),
            &(!needs_settling(run) && run.state != RunState::Pending),
            &attempt,
            &claimer,
            &until,
            &text,
        ],
    )?;
    Ok(rows == 1)
}

#[cfg(feature = "postgres")]
fn pg_account(
    tx: &mut postgres::Transaction<'_>,
    trigger_id: &str,
    failure: Option<&str>,
    pause_after: u32,
) -> Result<Option<String>, postgres::Error> {
    let Some(reason) = failure else {
        tx.execute(
            "UPDATE by_triggers SET failures = 0 WHERE id = $1",
            &[&trigger_id],
        )?;
        return Ok(None);
    };
    let row = tx.query_opt(
        "UPDATE by_triggers SET failures = failures + 1 WHERE id = $1 \
         RETURNING failures, enabled",
        &[&trigger_id],
    )?;
    let Some(row) = row else {
        return Ok(None);
    };
    let (failures, enabled): (i64, bool) = (row.get(0), row.get(1));
    if pause_after == 0 || !enabled || failures < i64::from(pause_after) {
        return Ok(None);
    }
    let why = pause_reason(failures as u32, reason);
    tx.execute(
        "UPDATE by_triggers SET enabled = false, next_due_ms = NULL, paused_reason = $2 \
         WHERE id = $1",
        &[&trigger_id, &why],
    )?;
    Ok(Some(why))
}

#[cfg(feature = "postgres")]
impl PostgresTriggers {
    /// Connect and make the tables that are missing.
    pub fn open(url: &str) -> io::Result<PostgresTriggers> {
        let store = PostgresTriggers {
            url: url.to_owned(),
            conn: Mutex::new(None),
        };
        store.with(|client| {
            let names: Vec<&str> = PG_SCHEMA.iter().map(|(name, _)| *name).collect();
            let missing = pg_missing(client, &names)?;
            for (name, statement) in PG_SCHEMA {
                if !missing.iter().any(|m| m == name) {
                    continue;
                }
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
                        *conn = Some(postgres::Client::connect(url, postgres::NoTls)?);
                    }
                    f(conn.as_mut().expect("connected above"))
                })
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
        .map_err(|e| match e.as_db_error() {
            Some(db) => io::Error::other(format!("trigger store: {}", db.message())),
            None => io::Error::other(format!("trigger store: {e}")),
        })
    }

    fn select(
        &self,
        filter: &str,
        params: Vec<Box<dyn postgres::types::ToSql + Sync + Send>>,
    ) -> io::Result<Vec<StoredTrigger>> {
        let query = format!("SELECT {TRIGGER_COLUMNS} FROM by_triggers {filter} ORDER BY seq");
        let rows = self.with(move |c| {
            let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
                params.iter().map(|p| p.as_ref() as _).collect();
            c.query(&query, &refs)
        })?;
        pg_triggers(&rows)
    }
}

#[cfg(feature = "postgres")]
impl Drop for PostgresTriggers {
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
impl TriggerStore for PostgresTriggers {
    fn create(&self, t: &StoredTrigger) -> io::Result<bool> {
        let text = body(t)?;
        let t = t.clone();
        self.with(move |c| {
            let rows = c.execute(
                "INSERT INTO by_triggers (id, tenant, name, repo, schedule, enabled, \
                 paused_reason, failures, next_due_ms, secret, body) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT DO NOTHING",
                &[
                    &t.id,
                    &t.tenant,
                    &t.spec.name,
                    &t.spec.repo,
                    &t.spec.when.is_schedule(),
                    &t.enabled,
                    &t.paused_reason,
                    &i64::from(t.failures),
                    &t.next_due_ms.map(i),
                    &t.secret,
                    &text,
                ],
            )?;
            Ok(rows == 1)
        })
    }

    fn get(&self, id: &str) -> io::Result<Option<StoredTrigger>> {
        Ok(self
            .select("WHERE id = $1", vec![Box::new(id.to_owned())])?
            .into_iter()
            .next())
    }

    fn find(&self, tenant: Option<&str>, key: &str) -> io::Result<Vec<StoredTrigger>> {
        match tenant {
            Some(tenant) => self.select(
                "WHERE tenant = $1 AND (id = $2 OR name = $2)",
                vec![Box::new(tenant.to_owned()), Box::new(key.to_owned())],
            ),
            None => self.select("WHERE id = $1 OR name = $1", vec![Box::new(key.to_owned())]),
        }
    }

    fn list(&self, tenant: Option<&str>) -> io::Result<Vec<StoredTrigger>> {
        match tenant {
            Some(tenant) => self.select("WHERE tenant = $1", vec![Box::new(tenant.to_owned())]),
            None => self.select("", Vec::new()),
        }
    }

    fn set_state(
        &self,
        id: &str,
        enabled: bool,
        paused_reason: Option<&str>,
        failures: u32,
        next_due_ms: Option<u64>,
    ) -> io::Result<bool> {
        let (id, reason) = (id.to_owned(), paused_reason.map(str::to_owned));
        self.with(move |c| {
            let rows = c.execute(
                "UPDATE by_triggers SET enabled = $2, paused_reason = $3, failures = $4, \
                 next_due_ms = $5 WHERE id = $1",
                &[
                    &id,
                    &enabled,
                    &reason,
                    &i64::from(failures),
                    &next_due_ms.map(i),
                ],
            )?;
            Ok(rows == 1)
        })
    }

    fn set_secret(&self, id: &str, secret: &str) -> io::Result<bool> {
        let (id, secret) = (id.to_owned(), secret.to_owned());
        self.with(move |c| {
            let rows = c.execute(
                "UPDATE by_triggers SET secret = $2 WHERE id = $1",
                &[&id, &secret],
            )?;
            Ok(rows == 1)
        })
    }

    fn remove(&self, id: &str) -> io::Result<bool> {
        let id = id.to_owned();
        self.with(move |c| {
            let mut tx = c.transaction()?;
            tx.execute("DELETE FROM by_trigger_runs WHERE trigger_id = $1", &[&id])?;
            let rows = tx.execute("DELETE FROM by_triggers WHERE id = $1", &[&id])?;
            tx.commit()?;
            Ok(rows == 1)
        })
    }

    fn due(&self, repos: &[String], now_ms: u64) -> io::Result<Vec<StoredTrigger>> {
        if repos.is_empty() {
            return Ok(Vec::new());
        }
        self.select(
            "WHERE schedule AND enabled AND next_due_ms IS NOT NULL AND next_due_ms <= $1 \
             AND repo = ANY($2)",
            vec![Box::new(i(now_ms)), Box::new(repos.to_vec())],
        )
    }

    fn claim_schedule(
        &self,
        id: &str,
        expected: u64,
        next: Option<u64>,
        runs: &[TriggerRun],
        claimer: &str,
        until_ms: u64,
    ) -> io::Result<bool> {
        let (id, runs, claimer) = (id.to_owned(), runs.to_vec(), claimer.to_owned());
        self.with(move |c| {
            let mut tx = c.transaction()?;
            let row = tx.query_opt(
                "UPDATE by_triggers SET next_due_ms = $3 \
                 WHERE id = $1 AND enabled AND next_due_ms = $2 RETURNING repo",
                &[&id, &i(expected), &next.map(i)],
            )?;
            let Some(row) = row else {
                tx.rollback()?;
                return Ok(false);
            };
            let repo: String = row.get(0);
            for run in &runs {
                let claim =
                    (run.state == RunState::Pending).then_some((claimer.as_str(), until_ms));
                pg_insert_run(&mut tx, run, &repo, claim)?;
            }
            tx.commit()?;
            Ok(true)
        })
    }

    fn record(&self, run: &TriggerRun) -> io::Result<Recorded> {
        let run = run.clone();
        let found = self.with(move |c| {
            let mut tx = c.transaction()?;
            let Some(row) = tx.query_opt(
                "SELECT repo FROM by_triggers WHERE id = $1",
                &[&run.trigger],
            )?
            else {
                return Ok(None);
            };
            let repo: String = row.get(0);
            if pg_insert_run(&mut tx, &run, &repo, None)? {
                tx.commit()?;
                return Ok(Some(None));
            }
            let existing = tx.query_one(
                "SELECT body FROM by_trigger_runs WHERE trigger_id = $1 AND key = $2",
                &[&run.trigger, &run.key],
            )?;
            tx.commit()?;
            Ok(Some(Some((run.key.clone(), existing.get::<_, String>(0)))))
        })?;
        match found {
            None => Err(io::Error::new(io::ErrorKind::NotFound, "no such trigger")),
            Some(None) => Ok(Recorded::Inserted),
            Some(Some((key, b))) => Ok(Recorded::Existing(Box::new(parse("run", &key, &b)?))),
        }
    }

    fn claim_run(
        &self,
        repos: &[String],
        claimer: &str,
        now_ms: u64,
        until_ms: u64,
    ) -> io::Result<Option<(TriggerRun, i64)>> {
        if repos.is_empty() {
            return Ok(None);
        }
        let (repos, claimer) = (repos.to_vec(), claimer.to_owned());
        let row = self.with(move |c| {
            c.query_opt(
                "UPDATE by_trigger_runs SET attempt = attempt + 1, claimer = $3, \
                 claimed_until = $4 \
                 WHERE id = (SELECT id FROM by_trigger_runs WHERE state = 'pending' \
                     AND (claimed_until IS NULL OR claimed_until < $1) AND repo = ANY($2) \
                     ORDER BY seq LIMIT 1 FOR UPDATE SKIP LOCKED) \
                 RETURNING id, body, attempt",
                &[&i(now_ms), &repos, &claimer, &i(until_ms)],
            )
        })?;
        row.map(|r| {
            let (id, b, attempt): (String, String, i64) = (r.get(0), r.get(1), r.get(2));
            Ok((parse("run", &id, &b)?, attempt))
        })
        .transpose()
    }

    fn defer_run(&self, run_id: &str, fence: i64, until_ms: u64) -> io::Result<bool> {
        let id = run_id.to_owned();
        self.with(move |c| {
            let rows = c.execute(
                "UPDATE by_trigger_runs SET claimed_until = $3 \
                 WHERE id = $1 AND attempt = $2 AND state = 'pending'",
                &[&id, &fence, &i(until_ms)],
            )?;
            Ok(rows == 1)
        })
    }

    fn release_claims(&self) -> io::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE by_trigger_runs SET claimer = NULL, claimed_until = NULL \
                 WHERE state = 'pending'",
                &[],
            )
            .map(|_| ())
        })
    }

    fn finish_run(&self, run: &TriggerRun, fence: i64, pause_after: u32) -> io::Result<Accounted> {
        let text = body(run)?;
        let run = run.clone();
        self.with(move |c| {
            let mut tx = c.transaction()?;
            let rows = tx.execute(
                "UPDATE by_trigger_runs SET state = $3, settled = $4, claimed_until = NULL, \
                 body = $5 WHERE id = $1 AND attempt = $2 AND state = 'pending'",
                &[
                    &run.id,
                    &fence,
                    &run.state.as_str(),
                    &!needs_settling(&run),
                    &text,
                ],
            )?;
            if rows != 1 {
                tx.rollback()?;
                return Ok(Accounted::default());
            }
            let paused = match failure_of(&run) {
                Some(reason) => pg_account(&mut tx, &run.trigger, Some(&reason), pause_after)?,
                None => None,
            };
            tx.commit()?;
            Ok(Accounted {
                written: true,
                paused,
            })
        })
    }

    fn unsettled(&self, repos: &[String]) -> io::Result<Vec<TriggerRun>> {
        if repos.is_empty() {
            return Ok(Vec::new());
        }
        let repos = repos.to_vec();
        let rows = self.with(move |c| {
            c.query(
                "SELECT id, body FROM by_trigger_runs WHERE state = 'fired' AND NOT settled \
                 AND repo = ANY($1) ORDER BY seq",
                &[&repos],
            )
        })?;
        pg_runs(&rows)
    }

    fn settle(
        &self,
        run_id: &str,
        outcome: &RunOutcome,
        pause_after: u32,
    ) -> io::Result<Accounted> {
        let (run_id, outcome) = (run_id.to_owned(), outcome.clone());
        self.with(move |c| {
            let mut tx = c.transaction()?;
            let Some(row) = tx.query_opt(
                "SELECT body FROM by_trigger_runs WHERE id = $1 AND NOT settled FOR UPDATE",
                &[&run_id],
            )?
            else {
                tx.rollback()?;
                return Ok(Ok(Accounted::default()));
            };
            let mut run: TriggerRun = match parse("run", &run_id, &row.get::<_, String>(0)) {
                Ok(run) => run,
                Err(e) => return Ok(Err(e)),
            };
            run.outcome = Some(outcome.clone());
            let text = serde_json::to_string(&run).expect("a run serializes");
            tx.execute(
                "UPDATE by_trigger_runs SET settled = true, body = $2 WHERE id = $1",
                &[&run_id, &text],
            )?;
            let failure = (!outcome.ok).then(|| outcome.detail.clone());
            let paused = pg_account(&mut tx, &run.trigger, failure.as_deref(), pause_after)?;
            tx.commit()?;
            Ok(Ok(Accounted {
                written: true,
                paused,
            }))
        })?
    }

    fn runs(&self, trigger_id: &str, limit: usize) -> io::Result<Vec<TriggerRun>> {
        let id = trigger_id.to_owned();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = self.with(move |c| {
            c.query(
                "SELECT id, body FROM by_trigger_runs WHERE trigger_id = $1 \
                 ORDER BY seq DESC LIMIT $2",
                &[&id, &limit],
            )
        })?;
        pg_runs(&rows)
    }
}

/// The store's contract, run against each backend: by this module's
/// tests on SQLite, and by `tests/triggers.rs` on PostgreSQL.
#[doc(hidden)]
pub mod conformance {
    use std::collections::BTreeSet;

    use branchyard_client::api::TaskRequest;
    use branchyard_client::triggers::{
        EventSource, RunOutcome, RunState, TriggerPolicy, TriggerRun, TriggerSpec, When,
    };

    use super::{Recorded, TriggerStore};
    use crate::config::Principal;
    use crate::triggers::StoredTrigger;

    pub fn trigger(tenant: &str, name: &str, schedule: bool) -> StoredTrigger {
        StoredTrigger {
            id: crate::triggers::new_id("trg"),
            tenant: tenant.into(),
            spec: TriggerSpec {
                name: name.into(),
                repo: "app".into(),
                when: match schedule {
                    true => When::Interval { seconds: 60 },
                    false => When::Event {
                        source: EventSource::Github,
                    },
                },
                conditions: Default::default(),
                task: TaskRequest {
                    prompt: "p".into(),
                    ..TaskRequest::default()
                },
                route: None,
                precheck: None,
                enabled: true,
                policy: TriggerPolicy::default(),
                secret: None,
            },
            principal: Principal::default_for("tester"),
            created_at_ms: 1,
            secret: (!schedule).then(|| "s".into()),
            enabled: true,
            paused_reason: None,
            failures: 0,
            next_due_ms: schedule.then_some(60_000),
        }
    }

    pub fn run(trigger: &str, key: &str, state: RunState) -> TriggerRun {
        TriggerRun {
            id: crate::triggers::new_id("run"),
            trigger: trigger.into(),
            key: key.into(),
            state,
            at_ms: 1,
            scheduled_ms: None,
            missed: None,
            last_missed_ms: None,
            event: None,
            reason: None,
            precheck: None,
            operation: None,
            branches: Vec::new(),
            outcome: None,
            finished_at_ms: None,
        }
    }

    fn repos() -> Vec<String> {
        vec!["app".into()]
    }

    /// Every check, in order.
    pub fn all(store: &dyn TriggerStore) {
        crud_and_tenants(store);
        schedules_are_claimed_once(store);
        runs_are_recorded_once_and_claimed_by_fence(store);
        failures_pause_and_successes_reset(store);
    }

    pub fn crud_and_tenants(store: &dyn TriggerStore) {
        let a = trigger("acme", "nightly", true);
        assert!(store.create(&a).unwrap());
        // A second trigger of that name in the tenant is refused; in
        // another tenant it is not.
        assert!(!store.create(&trigger("acme", "nightly", true)).unwrap());
        let b = trigger("other", "nightly", false);
        assert!(store.create(&b).unwrap());
        let got = store.get(&a.id).unwrap().unwrap();
        assert_eq!(got, a);
        assert_eq!(
            store.find(Some("acme"), "nightly").unwrap(),
            vec![a.clone()]
        );
        assert_eq!(store.find(Some("acme"), &a.id).unwrap(), vec![a.clone()]);
        assert!(store.find(Some("acme"), &b.id).unwrap().is_empty());
        assert_eq!(store.find(None, "nightly").unwrap().len(), 2);
        let ids: BTreeSet<String> = store
            .list(None)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(ids.contains(&a.id) && ids.contains(&b.id));
        assert_eq!(store.list(Some("other")).unwrap(), vec![b.clone()]);
        assert!(store
            .set_state(&a.id, false, Some("disabled by tester"), 2, None)
            .unwrap());
        let got = store.get(&a.id).unwrap().unwrap();
        assert!(!got.enabled);
        assert_eq!(got.paused_reason.as_deref(), Some("disabled by tester"));
        assert_eq!((got.failures, got.next_due_ms), (2, None));
        assert!(store.set_secret(&b.id, "new").unwrap());
        assert_eq!(
            store.get(&b.id).unwrap().unwrap().secret.as_deref(),
            Some("new")
        );
        assert!(store
            .record(&run(&b.id, "event:1", RunState::Pending))
            .is_ok());
        assert!(store.remove(&b.id).unwrap());
        assert!(!store.remove(&b.id).unwrap());
        assert!(store.get(&b.id).unwrap().is_none());
        assert!(store.runs(&b.id, 10).unwrap().is_empty());
        assert!(store.remove(&a.id).unwrap());
    }

    pub fn schedules_are_claimed_once(store: &dyn TriggerStore) {
        let t = trigger("acme", "every-minute", true);
        assert!(store.create(&t).unwrap());
        assert!(store.due(&repos(), 59_999).unwrap().is_empty());
        assert!(store.due(&["docs".to_owned()], 60_000).unwrap().is_empty());
        let due = store.due(&repos(), 60_000).unwrap();
        assert_eq!(due.len(), 1);
        let pending = run(&t.id, "schedule:60000", RunState::Pending);
        // The first claim moves the due time; a second with the same
        // expectation finds it moved and writes nothing.
        assert!(store
            .claim_schedule(
                &t.id,
                60_000,
                Some(120_000),
                std::slice::from_ref(&pending),
                "w1",
                90_000
            )
            .unwrap());
        let other = run(&t.id, "schedule:60000-again", RunState::Pending);
        assert!(!store
            .claim_schedule(&t.id, 60_000, Some(120_000), &[other], "w2", 90_000)
            .unwrap());
        assert!(store.due(&repos(), 60_000).unwrap().is_empty());
        let runs = store.runs(&t.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].key, "schedule:60000");
        // Claimed by w1 until 90 000: nobody else takes it before.
        assert!(store
            .claim_run(&repos(), "w2", 80_000, 200_000)
            .unwrap()
            .is_none());
        // After its claim expired, another dispatcher takes it, under a
        // new fence; the first claimant's fence no longer finishes it.
        let (taken, fence) = store
            .claim_run(&repos(), "w2", 95_000, 200_000)
            .unwrap()
            .unwrap();
        assert_eq!((taken.id.as_str(), fence), (pending.id.as_str(), 2));
        // Deferred, it is claimable once the deferral passes, under the
        // next fence; a stale fence defers nothing.
        assert!(!store.defer_run(&taken.id, 1, 0).unwrap());
        assert!(store.defer_run(&taken.id, fence, 96_000).unwrap());
        assert!(store
            .claim_run(&repos(), "w3", 95_500, 200_000)
            .unwrap()
            .is_none());
        let (_, fence) = store
            .claim_run(&repos(), "w3", 96_500, 200_000)
            .unwrap()
            .unwrap();
        assert_eq!(fence, 3);
        // A predecessor's claims, released all at once.
        store.release_claims().unwrap();
        let (taken, fence) = store
            .claim_run(&repos(), "w4", 95_000, 200_000)
            .unwrap()
            .unwrap();
        assert_eq!(fence, 4);
        let mut done = taken.clone();
        done.state = RunState::Fired;
        assert!(!store.finish_run(&done, 1, 3).unwrap().written);
        assert!(store.finish_run(&done, 4, 3).unwrap().written);
        assert!(!store.finish_run(&done, 4, 3).unwrap().written, "once only");
        // A disabled trigger is never claimed.
        assert!(store
            .set_state(&t.id, false, None, 0, Some(120_000))
            .unwrap());
        assert!(store.due(&repos(), 500_000).unwrap().is_empty());
        assert!(!store
            .claim_schedule(&t.id, 120_000, Some(180_000), &[], "w1", 0)
            .unwrap());
        assert!(store.remove(&t.id).unwrap());
    }

    pub fn runs_are_recorded_once_and_claimed_by_fence(store: &dyn TriggerStore) {
        let t = trigger("acme", "on-issue", false);
        assert!(store.create(&t).unwrap());
        let first = run(&t.id, "event:d-1", RunState::Pending);
        assert_eq!(store.record(&first).unwrap(), Recorded::Inserted);
        let again = run(&t.id, "event:d-1", RunState::Pending);
        match store.record(&again).unwrap() {
            Recorded::Existing(existing) => assert_eq!(existing.id, first.id),
            other => panic!("{other:?}"),
        }
        let skipped = run(&t.id, "event:d-2", RunState::SkippedCondition);
        assert_eq!(store.record(&skipped).unwrap(), Recorded::Inserted);
        // Only the pending run is claimable; a repository not served is
        // not looked at.
        assert!(store
            .claim_run(&["docs".into()], "w", 0, 10)
            .unwrap()
            .is_none());
        let (claimed, fence) = store.claim_run(&repos(), "w", 0, 10).unwrap().unwrap();
        assert_eq!(claimed.id, first.id);
        assert!(store.claim_run(&repos(), "w", 5, 10).unwrap().is_none());
        let mut fired = claimed.clone();
        fired.state = RunState::Fired;
        fired.operation = Some("op_1".into());
        assert!(store.finish_run(&fired, fence, 3).unwrap().written);
        let unsettled = store.unsettled(&repos()).unwrap();
        assert_eq!(unsettled.len(), 1);
        assert_eq!(unsettled[0].operation.as_deref(), Some("op_1"));
        let ok = RunOutcome {
            ok: true,
            detail: "succeeded".into(),
        };
        assert!(store.settle(&first.id, &ok, 3).unwrap().written);
        assert!(
            !store.settle(&first.id, &ok, 3).unwrap().written,
            "once only"
        );
        assert!(store.unsettled(&repos()).unwrap().is_empty());
        let runs = store.runs(&t.id, 10).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, skipped.id, "newest first");
        assert_eq!(runs[1].outcome, Some(ok));
        assert_eq!(store.runs(&t.id, 1).unwrap().len(), 1);
        assert!(store.remove(&t.id).unwrap());
    }

    pub fn failures_pause_and_successes_reset(store: &dyn TriggerStore) {
        let t = trigger("acme", "flaky", false);
        assert!(store.create(&t).unwrap());
        let fail = |key: &str| {
            let r = run(&t.id, key, RunState::Pending);
            store.record(&r).unwrap();
            let (mut claimed, fence) = store.claim_run(&repos(), "w", 0, 10).unwrap().unwrap();
            claimed.state = RunState::Failed;
            claimed.reason = Some(format!("quota exceeded ({key})"));
            store.finish_run(&claimed, fence, 3).unwrap()
        };
        assert_eq!(fail("event:1").paused, None);
        assert_eq!(fail("event:2").paused, None);
        assert_eq!(store.get(&t.id).unwrap().unwrap().failures, 2);
        // A fired run whose task succeeded starts the count over.
        let r = run(&t.id, "event:3", RunState::Pending);
        store.record(&r).unwrap();
        let (mut claimed, fence) = store.claim_run(&repos(), "w", 0, 10).unwrap().unwrap();
        claimed.state = RunState::Fired;
        store.finish_run(&claimed, fence, 3).unwrap();
        let ok = RunOutcome {
            ok: true,
            detail: "succeeded".into(),
        };
        store.settle(&claimed.id, &ok, 3).unwrap();
        assert_eq!(store.get(&t.id).unwrap().unwrap().failures, 0);
        assert_eq!(fail("event:4").paused, None);
        assert_eq!(fail("event:5").paused, None);
        let paused = fail("event:6").paused.expect("paused at the third");
        assert!(paused.contains("3 failed runs in a row"), "{paused}");
        assert!(paused.contains("event:6"), "{paused}");
        let got = store.get(&t.id).unwrap().unwrap();
        assert!(!got.enabled);
        assert_eq!(got.paused_reason, Some(paused));
        // A failed outcome of a fired task counts too, without pausing
        // twice.
        let r = run(&t.id, "event:7", RunState::Pending);
        store.record(&r).unwrap();
        let (mut claimed, fence) = store.claim_run(&repos(), "w", 0, 10).unwrap().unwrap();
        claimed.state = RunState::Fired;
        store.finish_run(&claimed, fence, 3).unwrap();
        let bad = RunOutcome {
            ok: false,
            detail: "the branch failed".into(),
        };
        assert_eq!(store.settle(&claimed.id, &bad, 3).unwrap().paused, None);
        assert_eq!(store.get(&t.id).unwrap().unwrap().failures, 4);
        assert!(store.remove(&t.id).unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_conforms() {
        conformance::all(&SqliteTriggers::memory());
    }

    #[test]
    fn sqlite_shares_the_registrys_file() {
        let dir = std::env::temp_dir().join(format!("by-trigger-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = SqliteTriggers::path_in(&dir);
        let registry = crate::store::SqliteStore::open(&path, None).unwrap();
        let a = SqliteTriggers::open(&path).unwrap();
        let b = SqliteTriggers::open(&path).unwrap();
        let t = conformance::trigger("default", "shared", true);
        assert!(a.create(&t).unwrap());
        assert_eq!(b.get(&t.id).unwrap().unwrap().id, t.id);
        drop(registry);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
