//! Where the companion keeps pairing codes, paired tokens and Web Push
//! subscriptions: the same database as the operation registry, in tables
//! of its own. Only hashes of codes and tokens are stored, never the
//! plaintext.
//!
//! - A pairing code is redeemed by deleting its row (`DELETE ... RETURNING`
//!   in one transaction with the token's insert), so two redemptions of
//!   one code can never both succeed, on any server sharing the database.
//! - A paired token is revoked by stamping `revoked_at_ms`; every request
//!   reads the row, so a revocation holds at once everywhere.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use branchyard::store_codec::{from_db, from_db_opt, to_db};
use rusqlite::OptionalExtension;

use crate::config::Principal;

/// An unredeemed pairing code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pairing {
    pub code_sha256: String,
    /// The principal the token will act as; its `name` names the token.
    pub principal: Principal,
    /// How long the token lasts once redeemed.
    pub token_ttl_ms: u64,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}

/// A token made by redeeming a pairing code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairedToken {
    pub token_sha256: String,
    pub principal: Principal,
    pub device: Option<String>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub revoked_at_ms: Option<u64>,
}

impl PairedToken {
    pub fn usable_at(&self, now_ms: u64) -> bool {
        self.revoked_at_ms.is_none() && now_ms < self.expires_at_ms
    }
}

/// A browser's Web Push subscription, bound to the credential that made
/// it: it is sent to only while that credential still verifies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscription {
    pub endpoint: String,
    pub token_sha256: String,
    pub p256dh: String,
    pub auth: String,
    /// Empty means every kind.
    pub kinds: Vec<String>,
    pub created_at_ms: u64,
}

/// Pairing codes, paired tokens and push subscriptions.
pub trait CompanionStore: Send + Sync {
    /// Record a code; false when an active paired token or unredeemed code
    /// already has the principal's name. Drops expired codes first.
    fn create_pairing(&self, pairing: &Pairing, now_ms: u64) -> io::Result<bool>;
    /// Redeem `code_sha256` at `now_ms`: remove it and, if it had not
    /// expired, record `token` from it in the same transaction. `None`
    /// when there is no such code or it expired.
    fn redeem(
        &self,
        code_sha256: &str,
        token_sha256: &str,
        device: Option<&str>,
        now_ms: u64,
    ) -> io::Result<Option<PairedToken>>;
    fn token(&self, token_sha256: &str) -> io::Result<Option<PairedToken>>;
    /// Every paired token, oldest first, and the unredeemed codes.
    fn tokens(&self) -> io::Result<(Vec<PairedToken>, Vec<Pairing>)>;
    /// Revoke the active tokens and unredeemed codes named `name` (in
    /// `tenant`, if given), and drop their tokens' subscriptions; how many
    /// tokens and codes there were.
    fn revoke(&self, name: &str, tenant: Option<&str>, now_ms: u64) -> io::Result<usize>;

    /// Add or replace (by endpoint) a subscription.
    fn subscribe(&self, subscription: &Subscription) -> io::Result<()>;
    /// Remove `endpoint` if it belongs to one of `token_sha256`es (or to
    /// anyone, with `None`); whether it existed.
    fn unsubscribe(&self, endpoint: &str, token_sha256: Option<&[String]>) -> io::Result<bool>;
    fn subscriptions(&self) -> io::Result<Vec<Subscription>>;
}

fn sql(error: rusqlite::Error) -> io::Error {
    io::Error::other(format!("companion store: {error}"))
}

fn principal_json(p: &Principal) -> io::Result<String> {
    serde_json::to_string(p).map_err(io::Error::other)
}

fn principal_of(text: &str) -> io::Result<Principal> {
    serde_json::from_str(text)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("principal: {e}")))
}

fn kinds_of(text: &str) -> Vec<String> {
    serde_json::from_str(text).unwrap_or_default()
}

/// The store a server with `config` keeps its companion state in:
/// PostgreSQL with `database`, else the data directory's `state.db`.
pub fn open(config: &crate::Config) -> Result<std::sync::Arc<dyn CompanionStore>, String> {
    #[cfg(feature = "postgres")]
    if let Some(url) = &config.database {
        return PostgresCompanion::open(url)
            .map(|s| std::sync::Arc::new(s) as std::sync::Arc<dyn CompanionStore>)
            .map_err(|e| format!("companion state in the database: {e}"));
    }
    #[cfg(not(feature = "postgres"))]
    if config.database.is_some() {
        return Err(
            "this build has no PostgreSQL support; build with the postgres feature \
                    to use --database"
                .into(),
        );
    }
    let path = SqliteCompanion::path_in(&config.data_dir);
    SqliteCompanion::open(&path)
        .map(|s| std::sync::Arc::new(s) as std::sync::Arc<dyn CompanionStore>)
        .map_err(|e| format!("companion state in {}: {e}", path.display()))
}

// ---------------------------------------------------------------------
// SQLite

/// The tables in the data directory's `state.db`, beside the operation
/// registry, on a connection of their own.
pub struct SqliteCompanion {
    conn: Mutex<rusqlite::Connection>,
}

const SQLITE_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS companion_pairings (
        code_sha256 TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        tenant TEXT NOT NULL,
        principal TEXT NOT NULL,
        token_ttl_ms INTEGER NOT NULL,
        created_at_ms INTEGER NOT NULL,
        expires_at_ms INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS companion_tokens (
        token_sha256 TEXT PRIMARY KEY,
        seq INTEGER NOT NULL,
        name TEXT NOT NULL,
        tenant TEXT NOT NULL,
        principal TEXT NOT NULL,
        device TEXT,
        created_at_ms INTEGER NOT NULL,
        expires_at_ms INTEGER NOT NULL,
        revoked_at_ms INTEGER
    );
    CREATE INDEX IF NOT EXISTS companion_tokens_name ON companion_tokens (name);
    CREATE TABLE IF NOT EXISTS companion_push (
        endpoint TEXT PRIMARY KEY,
        token_sha256 TEXT NOT NULL,
        p256dh TEXT NOT NULL,
        auth TEXT NOT NULL,
        kinds TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    );
";

const TOKEN_COLUMNS: &str =
    "token_sha256, principal, device, created_at_ms, expires_at_ms, revoked_at_ms";

fn sqlite_token(r: &rusqlite::Row<'_>) -> rusqlite::Result<TokenRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}

type TokenRow = (String, String, Option<String>, i64, i64, Option<i64>);

fn token_from(row: TokenRow) -> io::Result<PairedToken> {
    let (token_sha256, principal, device, created, expires, revoked) = row;
    Ok(PairedToken {
        token_sha256,
        principal: principal_of(&principal)?,
        device,
        created_at_ms: from_db("created", created)?,
        expires_at_ms: from_db("expires", expires)?,
        revoked_at_ms: from_db_opt("revoked", revoked)?,
    })
}

type PairingRow = (String, String, i64, i64, i64);

fn pairing_from(row: PairingRow) -> io::Result<Pairing> {
    let (code_sha256, principal, ttl, created, expires) = row;
    Ok(Pairing {
        code_sha256,
        principal: principal_of(&principal)?,
        token_ttl_ms: from_db("ttl", ttl)?,
        created_at_ms: from_db("created", created)?,
        expires_at_ms: from_db("expires", expires)?,
    })
}

impl SqliteCompanion {
    /// Open the database at `path` (made if missing) and its tables.
    pub fn open(path: &Path) -> io::Result<SqliteCompanion> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = rusqlite::Connection::open(path).map_err(sql)?;
        conn.busy_timeout(Duration::from_secs(30)).map_err(sql)?;
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
        Ok(SqliteCompanion {
            conn: Mutex::new(conn),
        })
    }

    /// The tables in a database of their own, in memory: for tests.
    pub fn memory() -> SqliteCompanion {
        let conn = rusqlite::Connection::open_in_memory().expect("an in-memory database");
        conn.execute_batch(SQLITE_SCHEMA).expect("the schema");
        SqliteCompanion {
            conn: Mutex::new(conn),
        }
    }

    /// The default place: `DATA-DIR/state.db`.
    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join("state.db")
    }

    fn immediate<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql)?;
        let value = f(&tx)?;
        tx.commit().map_err(sql)?;
        Ok(value)
    }
}

/// Whether `name` is taken by an active token or a live code.
fn sqlite_name_taken(tx: &rusqlite::Transaction<'_>, name: &str, now: i64) -> io::Result<bool> {
    let tokens: i64 = tx
        .query_row(
            "SELECT count(*) FROM companion_tokens WHERE name = ?1 AND revoked_at_ms IS NULL \
             AND expires_at_ms > ?2",
            rusqlite::params![name, now],
            |r| r.get(0),
        )
        .map_err(sql)?;
    let codes: i64 = tx
        .query_row(
            "SELECT count(*) FROM companion_pairings WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .map_err(sql)?;
    Ok(tokens + codes > 0)
}

impl CompanionStore for SqliteCompanion {
    fn create_pairing(&self, p: &Pairing, now_ms: u64) -> io::Result<bool> {
        let body = principal_json(&p.principal)?;
        self.immediate(|tx| {
            tx.execute(
                "DELETE FROM companion_pairings WHERE expires_at_ms <= ?1",
                [to_db("now_ms", now_ms)?],
            )
            .map_err(sql)?;
            if sqlite_name_taken(tx, &p.principal.name, to_db("now_ms", now_ms)?)? {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO companion_pairings (code_sha256, name, tenant, principal, \
                 token_ttl_ms, created_at_ms, expires_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    p.code_sha256,
                    p.principal.name,
                    p.principal.tenant,
                    body,
                    to_db("token_ttl_ms", p.token_ttl_ms)?,
                    to_db("created_at_ms", p.created_at_ms)?,
                    to_db("expires_at_ms", p.expires_at_ms)?
                ],
            )
            .map_err(sql)?;
            Ok(true)
        })
    }

    fn redeem(
        &self,
        code_sha256: &str,
        token_sha256: &str,
        device: Option<&str>,
        now_ms: u64,
    ) -> io::Result<Option<PairedToken>> {
        self.immediate(|tx| {
            let row: Option<PairingRow> = tx
                .query_row(
                    "DELETE FROM companion_pairings WHERE code_sha256 = ?1 RETURNING \
                     code_sha256, principal, token_ttl_ms, created_at_ms, expires_at_ms",
                    [code_sha256],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()
                .map_err(sql)?;
            let Some(pairing) = row.map(pairing_from).transpose()? else {
                return Ok(None);
            };
            if pairing.expires_at_ms <= now_ms {
                return Ok(None);
            }
            let token = PairedToken {
                token_sha256: token_sha256.to_owned(),
                principal: pairing.principal,
                device: device.map(str::to_owned),
                created_at_ms: now_ms,
                expires_at_ms: now_ms.saturating_add(pairing.token_ttl_ms),
                revoked_at_ms: None,
            };
            let seq: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(seq), 0) + 1 FROM companion_tokens",
                    [],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            tx.execute(
                "INSERT INTO companion_tokens (token_sha256, seq, name, tenant, principal, device, \
                 created_at_ms, expires_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    token.token_sha256,
                    seq,
                    token.principal.name,
                    token.principal.tenant,
                    principal_json(&token.principal)?,
                    token.device,
                    to_db("created_at_ms", token.created_at_ms)?,
                    to_db("expires_at_ms", token.expires_at_ms)?
                ],
            )
            .map_err(sql)?;
            Ok(Some(token))
        })
    }

    fn token(&self, token_sha256: &str) -> io::Result<Option<PairedToken>> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let row: Option<TokenRow> = conn
            .query_row(
                &format!("SELECT {TOKEN_COLUMNS} FROM companion_tokens WHERE token_sha256 = ?1"),
                [token_sha256],
                sqlite_token,
            )
            .optional()
            .map_err(sql)?;
        row.map(token_from).transpose()
    }

    fn tokens(&self) -> io::Result<(Vec<PairedToken>, Vec<Pairing>)> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {TOKEN_COLUMNS} FROM companion_tokens ORDER BY seq"
            ))
            .map_err(sql)?;
        let tokens = stmt
            .query_map([], sqlite_token)
            .map_err(sql)?
            .map(|r| r.map_err(sql).and_then(token_from))
            .collect::<io::Result<Vec<_>>>()?;
        let mut stmt = conn
            .prepare(
                "SELECT code_sha256, principal, token_ttl_ms, created_at_ms, expires_at_ms \
                 FROM companion_pairings ORDER BY created_at_ms",
            )
            .map_err(sql)?;
        let codes = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map_err(sql)?
            .map(|r| r.map_err(sql).and_then(pairing_from))
            .collect::<io::Result<Vec<_>>>()?;
        Ok((tokens, codes))
    }

    fn revoke(&self, name: &str, tenant: Option<&str>, now_ms: u64) -> io::Result<usize> {
        self.immediate(|tx| {
            let tenant_ok = |t: &str| tenant.is_none_or(|want| want == t);
            let mut hashes = Vec::new();
            {
                let mut stmt = tx
                    .prepare(
                        "SELECT token_sha256, tenant FROM companion_tokens WHERE name = ?1 \
                         AND revoked_at_ms IS NULL",
                    )
                    .map_err(sql)?;
                let rows = stmt
                    .query_map([name], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })
                    .map_err(sql)?;
                for row in rows {
                    let (hash, t) = row.map_err(sql)?;
                    if tenant_ok(&t) {
                        hashes.push(hash);
                    }
                }
            }
            for hash in &hashes {
                tx.execute(
                    "UPDATE companion_tokens SET revoked_at_ms = ?2 WHERE token_sha256 = ?1",
                    rusqlite::params![hash, to_db("now_ms", now_ms)?],
                )
                .map_err(sql)?;
                tx.execute("DELETE FROM companion_push WHERE token_sha256 = ?1", [hash])
                    .map_err(sql)?;
            }
            let codes = match tenant {
                Some(t) => tx.execute(
                    "DELETE FROM companion_pairings WHERE name = ?1 AND tenant = ?2",
                    [name, t],
                ),
                None => tx.execute("DELETE FROM companion_pairings WHERE name = ?1", [name]),
            }
            .map_err(sql)?;
            Ok(hashes.len() + codes)
        })
    }

    fn subscribe(&self, s: &Subscription) -> io::Result<()> {
        let kinds = serde_json::to_string(&s.kinds).map_err(io::Error::other)?;
        self.immediate(|tx| {
            tx.execute(
                "INSERT INTO companion_push (endpoint, token_sha256, p256dh, auth, kinds, \
                 created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (endpoint) DO UPDATE \
                 SET token_sha256 = excluded.token_sha256, p256dh = excluded.p256dh, \
                 auth = excluded.auth, kinds = excluded.kinds, \
                 created_at_ms = excluded.created_at_ms",
                rusqlite::params![
                    s.endpoint,
                    s.token_sha256,
                    s.p256dh,
                    s.auth,
                    kinds,
                    to_db("created_at_ms", s.created_at_ms)?
                ],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    fn unsubscribe(&self, endpoint: &str, owners: Option<&[String]>) -> io::Result<bool> {
        self.immediate(|tx| {
            let owner: Option<String> = tx
                .query_row(
                    "SELECT token_sha256 FROM companion_push WHERE endpoint = ?1",
                    [endpoint],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            let Some(owner) = owner else {
                return Ok(false);
            };
            if owners.is_some_and(|o| !o.contains(&owner)) {
                return Ok(false);
            }
            tx.execute("DELETE FROM companion_push WHERE endpoint = ?1", [endpoint])
                .map_err(sql)?;
            Ok(true)
        })
    }

    fn subscriptions(&self) -> io::Result<Vec<Subscription>> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn
            .prepare(
                "SELECT endpoint, token_sha256, p256dh, auth, kinds, created_at_ms \
                 FROM companion_push ORDER BY created_at_ms, endpoint",
            )
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Subscription {
                    endpoint: r.get(0)?,
                    token_sha256: r.get(1)?,
                    p256dh: r.get(2)?,
                    auth: r.get(3)?,
                    kinds: kinds_of(&r.get::<_, String>(4)?),
                    created_at_ms: from_db("created_at_ms", r.get(5)?)?,
                })
            })
            .map_err(sql)?;
        rows.map(|r| r.map_err(sql)).collect()
    }
}

// ---------------------------------------------------------------------
// PostgreSQL

/// The tables in PostgreSQL, beside the operation registry's, on a
/// connection of their own.
#[cfg(feature = "postgres")]
pub struct PostgresCompanion {
    url: String,
    conn: Mutex<Option<postgres::Client>>,
}

/// The tables, one object per step, made like the registry's
/// (`crate::store::PostgresStore::open`): the catalog is read first, and
/// only the missing steps run, each alone in a transaction under one
/// advisory lock, so opening never holds one table's lock while waiting
/// for another's.
#[cfg(feature = "postgres")]
const PG_SCHEMA: &[(&str, &str)] = &[
    (
        "by_companion_pairings",
        "CREATE TABLE IF NOT EXISTS by_companion_pairings (
            code_sha256 TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            tenant TEXT NOT NULL,
            principal TEXT NOT NULL,
            token_ttl_ms BIGINT NOT NULL,
            created_at_ms BIGINT NOT NULL,
            expires_at_ms BIGINT NOT NULL
        )",
    ),
    (
        "by_companion_tokens",
        "CREATE TABLE IF NOT EXISTS by_companion_tokens (
            token_sha256 TEXT PRIMARY KEY,
            seq BIGINT GENERATED ALWAYS AS IDENTITY,
            name TEXT NOT NULL,
            tenant TEXT NOT NULL,
            principal TEXT NOT NULL,
            device TEXT,
            created_at_ms BIGINT NOT NULL,
            expires_at_ms BIGINT NOT NULL,
            revoked_at_ms BIGINT
        )",
    ),
    (
        "by_companion_tokens_name",
        "CREATE INDEX IF NOT EXISTS by_companion_tokens_name ON by_companion_tokens (name)",
    ),
    (
        "by_companion_push",
        "CREATE TABLE IF NOT EXISTS by_companion_push (
            endpoint TEXT PRIMARY KEY,
            token_sha256 TEXT NOT NULL,
            p256dh TEXT NOT NULL,
            auth TEXT NOT NULL,
            kinds TEXT NOT NULL,
            created_at_ms BIGINT NOT NULL
        )",
    ),
];

/// The advisory lock the registry's and the trigger store's DDL take, so
/// every store's table creation takes turns.
#[cfg(feature = "postgres")]
const PG_DDL_LOCK: i64 = 7390184326;

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
fn pg_name_taken(
    tx: &mut postgres::Transaction<'_>,
    name: &str,
    now: i64,
) -> Result<bool, postgres::Error> {
    let row = tx.query_one(
        "SELECT (SELECT count(*) FROM by_companion_tokens WHERE name = $1 \
         AND revoked_at_ms IS NULL AND expires_at_ms > $2) \
         + (SELECT count(*) FROM by_companion_pairings WHERE name = $1)",
        &[&name, &now],
    )?;
    Ok(row.get::<_, i64>(0) > 0)
}

#[cfg(feature = "postgres")]
impl PostgresCompanion {
    /// Connect and make the tables that are missing.
    pub fn open(url: &str) -> io::Result<PostgresCompanion> {
        let store = PostgresCompanion {
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
                tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PG_DDL_LOCK])?;
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
            Some(db) => io::Error::other(format!("companion store: {}", db.message())),
            None => io::Error::other(format!("companion store: {e}")),
        })
    }
}

#[cfg(feature = "postgres")]
impl Drop for PostgresCompanion {
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
fn pg_token(r: &postgres::Row) -> TokenRow {
    (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5))
}

#[cfg(feature = "postgres")]
impl CompanionStore for PostgresCompanion {
    fn create_pairing(&self, p: &Pairing, now_ms: u64) -> io::Result<bool> {
        let body = principal_json(&p.principal)?;
        let p = p.clone();
        let now_ms_db = to_db("now_ms", now_ms)?;
        let token_ttl_ms_db = to_db("token_ttl_ms", p.token_ttl_ms)?;
        let created_at_ms_db = to_db("created_at_ms", p.created_at_ms)?;
        let expires_at_ms_db = to_db("expires_at_ms", p.expires_at_ms)?;
        self.with(move |c| {
            let mut tx = c.transaction()?;
            // Pairings of one name take turns, so two cannot both pass the
            // check below.
            tx.execute(
                "SELECT pg_advisory_xact_lock(hashtextextended('branchyard companion ' || $1, 0))",
                &[&p.principal.name],
            )?;
            tx.execute(
                "DELETE FROM by_companion_pairings WHERE expires_at_ms <= $1",
                &[&now_ms_db],
            )?;
            if pg_name_taken(&mut tx, &p.principal.name, now_ms_db)? {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO by_companion_pairings (code_sha256, name, tenant, principal, \
                 token_ttl_ms, created_at_ms, expires_at_ms) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &p.code_sha256,
                    &p.principal.name,
                    &p.principal.tenant,
                    &body,
                    &token_ttl_ms_db,
                    &created_at_ms_db,
                    &expires_at_ms_db,
                ],
            )?;
            tx.commit()?;
            Ok(true)
        })
    }

    fn redeem(
        &self,
        code_sha256: &str,
        token_sha256: &str,
        device: Option<&str>,
        now_ms: u64,
    ) -> io::Result<Option<PairedToken>> {
        let (code, hash, label) = (
            code_sha256.to_owned(),
            token_sha256.to_owned(),
            device.map(str::to_owned),
        );
        let now_ms_db = to_db("now_ms", now_ms)?;
        let row = self.with(move |c| {
            let mut tx = c.transaction()?;
            let row = tx.query_opt(
                "DELETE FROM by_companion_pairings WHERE code_sha256 = $1 RETURNING \
                 code_sha256, principal, token_ttl_ms, created_at_ms, expires_at_ms",
                &[&code],
            )?;
            let Some(row) = row else {
                tx.commit()?;
                return Ok(None);
            };
            let pairing: PairingRow = (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4));
            // Expired, or a stored negative the decode below reports.
            if pairing.2 < 0 || pairing.4 <= now_ms_db {
                tx.commit()?;
                return Ok(Some((pairing, false)));
            }
            let expires_at_ms_db = now_ms_db.saturating_add(pairing.2);
            let principal: Principal = serde_json::from_str(&pairing.1)
                .unwrap_or_else(|_| Principal::default_for("unreadable"));
            tx.execute(
                "INSERT INTO by_companion_tokens (token_sha256, name, tenant, principal, device, \
                 created_at_ms, expires_at_ms) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[
                    &hash,
                    &principal.name,
                    &principal.tenant,
                    &pairing.1,
                    &label,
                    &now_ms_db,
                    &expires_at_ms_db,
                ],
            )?;
            tx.commit()?;
            Ok(Some((pairing, true)))
        })?;
        let Some((pairing, redeemed)) = row else {
            return Ok(None);
        };
        let pairing = pairing_from(pairing)?;
        if !redeemed {
            return Ok(None);
        }
        Ok(Some(PairedToken {
            token_sha256: token_sha256.to_owned(),
            principal: pairing.principal,
            device: device.map(str::to_owned),
            created_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(pairing.token_ttl_ms),
            revoked_at_ms: None,
        }))
    }

    fn token(&self, token_sha256: &str) -> io::Result<Option<PairedToken>> {
        let hash = token_sha256.to_owned();
        let row = self.with(move |c| {
            c.query_opt(
                &format!("SELECT {TOKEN_COLUMNS} FROM by_companion_tokens WHERE token_sha256 = $1"),
                &[&hash],
            )
        })?;
        row.map(|r| token_from(pg_token(&r))).transpose()
    }

    fn tokens(&self) -> io::Result<(Vec<PairedToken>, Vec<Pairing>)> {
        let (tokens, codes) = self.with(|c| {
            let tokens = c.query(
                &format!("SELECT {TOKEN_COLUMNS} FROM by_companion_tokens ORDER BY seq"),
                &[],
            )?;
            let codes = c.query(
                "SELECT code_sha256, principal, token_ttl_ms, created_at_ms, expires_at_ms \
                 FROM by_companion_pairings ORDER BY created_at_ms",
                &[],
            )?;
            Ok((tokens, codes))
        })?;
        let tokens = tokens
            .iter()
            .map(|r| token_from(pg_token(r)))
            .collect::<io::Result<_>>()?;
        let codes = codes
            .iter()
            .map(|r| pairing_from((r.get(0), r.get(1), r.get(2), r.get(3), r.get(4))))
            .collect::<io::Result<_>>()?;
        Ok((tokens, codes))
    }

    fn revoke(&self, name: &str, tenant: Option<&str>, now_ms: u64) -> io::Result<usize> {
        let (name, tenant) = (name.to_owned(), tenant.map(str::to_owned));
        let now_ms_db = to_db("now_ms", now_ms)?;
        let (tokens, codes) = self.with(move |c| {
            let mut tx = c.transaction()?;
            let rows = tx.query(
                "UPDATE by_companion_tokens SET revoked_at_ms = $3 WHERE name = $1 \
                 AND revoked_at_ms IS NULL AND ($2::text IS NULL OR tenant = $2) \
                 RETURNING token_sha256",
                &[&name, &tenant, &now_ms_db],
            )?;
            let hashes: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
            tx.execute(
                "DELETE FROM by_companion_push WHERE token_sha256 = ANY($1)",
                &[&hashes],
            )?;
            let codes = tx.execute(
                "DELETE FROM by_companion_pairings WHERE name = $1 \
                 AND ($2::text IS NULL OR tenant = $2)",
                &[&name, &tenant],
            )?;
            tx.commit()?;
            Ok((hashes.len(), codes))
        })?;
        Ok(tokens + branchyard::store_codec::to_usize("revoked pairings", codes)?)
    }

    fn subscribe(&self, s: &Subscription) -> io::Result<()> {
        let kinds = serde_json::to_string(&s.kinds).map_err(io::Error::other)?;
        let s = s.clone();
        let created_at_ms_db = to_db("created_at_ms", s.created_at_ms)?;
        self.with(move |c| {
            c.execute(
                "INSERT INTO by_companion_push (endpoint, token_sha256, p256dh, auth, kinds, \
                 created_at_ms) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (endpoint) DO UPDATE \
                 SET token_sha256 = excluded.token_sha256, p256dh = excluded.p256dh, \
                 auth = excluded.auth, kinds = excluded.kinds, \
                 created_at_ms = excluded.created_at_ms",
                &[
                    &s.endpoint,
                    &s.token_sha256,
                    &s.p256dh,
                    &s.auth,
                    &kinds,
                    &created_at_ms_db,
                ],
            )?;
            Ok(())
        })
    }

    fn unsubscribe(&self, endpoint: &str, owners: Option<&[String]>) -> io::Result<bool> {
        let endpoint = endpoint.to_owned();
        let owners: Option<Vec<String>> = owners.map(<[String]>::to_vec);
        self.with(move |c| {
            let n = match &owners {
                Some(owners) => c.execute(
                    "DELETE FROM by_companion_push WHERE endpoint = $1 AND token_sha256 = ANY($2)",
                    &[&endpoint, owners],
                )?,
                None => c.execute(
                    "DELETE FROM by_companion_push WHERE endpoint = $1",
                    &[&endpoint],
                )?,
            };
            Ok(n > 0)
        })
    }

    fn subscriptions(&self) -> io::Result<Vec<Subscription>> {
        let rows = self.with(|c| {
            c.query(
                "SELECT endpoint, token_sha256, p256dh, auth, kinds, created_at_ms \
                 FROM by_companion_push ORDER BY created_at_ms, endpoint",
                &[],
            )
        })?;
        rows.iter()
            .map(|r| {
                Ok(Subscription {
                    endpoint: r.get(0),
                    token_sha256: r.get(1),
                    p256dh: r.get(2),
                    auth: r.get(3),
                    kinds: kinds_of(&r.get::<_, String>(4)),
                    created_at_ms: from_db("created_at_ms", r.get(5))?,
                })
            })
            .collect()
    }
}

/// The store's contract, run against each backend: by this module's
/// tests on SQLite, and by `tests/companion.rs` on PostgreSQL.
#[doc(hidden)]
pub mod conformance {
    use super::*;

    pub fn principal(name: &str, tenant: &str) -> Principal {
        let mut p = Principal::default_for(name);
        p.tenant = tenant.to_owned();
        p
    }

    pub fn all(store: &dyn CompanionStore) {
        pairing_is_single_use_and_expires(store);
        revocation_drops_tokens_codes_and_subscriptions(store);
        subscriptions_belong_to_their_token(store);
    }

    pub fn pairing_is_single_use_and_expires(store: &dyn CompanionStore) {
        let pairing = Pairing {
            code_sha256: "c1".into(),
            principal: principal("phone", "acme"),
            token_ttl_ms: 1000,
            created_at_ms: 10,
            expires_at_ms: 100,
        };
        assert!(store.create_pairing(&pairing, 10).unwrap());
        // The name is taken while the code is live.
        let again = Pairing {
            code_sha256: "c2".into(),
            ..pairing.clone()
        };
        assert!(!store.create_pairing(&again, 20).unwrap());
        assert_eq!(store.redeem("nope", "t0", None, 20).unwrap(), None);
        let token = store
            .redeem("c1", "t1", Some("Firefox"), 50)
            .unwrap()
            .expect("redeemed");
        assert_eq!(token.principal.name, "phone");
        assert_eq!(token.principal.tenant, "acme");
        assert_eq!(token.expires_at_ms, 1050);
        assert_eq!(token.device.as_deref(), Some("Firefox"));
        assert!(token.usable_at(1049) && !token.usable_at(1050));
        // Single use.
        assert_eq!(store.redeem("c1", "t2", None, 51).unwrap(), None);
        assert_eq!(store.token("t1").unwrap().unwrap().expires_at_ms, 1050);
        assert_eq!(store.token("t2").unwrap(), None);
        // An expired code is consumed and refused.
        let late = Pairing {
            code_sha256: "c3".into(),
            principal: principal("late", "acme"),
            ..pairing.clone()
        };
        assert!(store.create_pairing(&late, 10).unwrap());
        assert_eq!(store.redeem("c3", "t3", None, 100).unwrap(), None);
        assert_eq!(store.token("t3").unwrap(), None);
        // A name frees once its token expires.
        let reuse = Pairing {
            code_sha256: "c4".into(),
            ..pairing.clone()
        };
        assert!(store.create_pairing(&reuse, 2000).unwrap());
        let (tokens, codes) = store.tokens().unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(codes.len(), 1);
        assert_eq!(store.revoke("phone", None, 2001).unwrap(), 2);
    }

    pub fn revocation_drops_tokens_codes_and_subscriptions(store: &dyn CompanionStore) {
        let pairing = Pairing {
            code_sha256: "r1".into(),
            principal: principal("tablet", "globex"),
            token_ttl_ms: 1000,
            created_at_ms: 0,
            expires_at_ms: 100,
        };
        assert!(store.create_pairing(&pairing, 0).unwrap());
        store.redeem("r1", "rt1", None, 1).unwrap().unwrap();
        store
            .subscribe(&Subscription {
                endpoint: "https://push.example/a".into(),
                token_sha256: "rt1".into(),
                p256dh: "k".into(),
                auth: "a".into(),
                kinds: vec![],
                created_at_ms: 2,
            })
            .unwrap();
        // Another tenant's name is not touched.
        assert_eq!(store.revoke("tablet", Some("acme"), 3).unwrap(), 0);
        assert_eq!(store.revoke("tablet", Some("globex"), 3).unwrap(), 1);
        let token = store.token("rt1").unwrap().unwrap();
        assert_eq!(token.revoked_at_ms, Some(3));
        assert!(!token.usable_at(4));
        assert!(store
            .subscriptions()
            .unwrap()
            .iter()
            .all(|s| s.token_sha256 != "rt1"));
        assert_eq!(store.revoke("tablet", None, 5).unwrap(), 0);
    }

    pub fn subscriptions_belong_to_their_token(store: &dyn CompanionStore) {
        let sub = |endpoint: &str, owner: &str| Subscription {
            endpoint: endpoint.into(),
            token_sha256: owner.into(),
            p256dh: "key".into(),
            auth: "secret".into(),
            kinds: vec!["permission".into()],
            created_at_ms: 7,
        };
        store
            .subscribe(&sub("https://push.example/1", "o1"))
            .unwrap();
        store
            .subscribe(&sub("https://push.example/2", "o2"))
            .unwrap();
        // Re-subscribing an endpoint replaces it.
        store
            .subscribe(&sub("https://push.example/2", "o1"))
            .unwrap();
        let all = store.subscriptions().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|s| s.token_sha256 == "o1"));
        assert_eq!(all[0].kinds, vec!["permission".to_owned()]);
        assert!(!store
            .unsubscribe("https://push.example/1", Some(&["o2".to_owned()]))
            .unwrap());
        assert!(store
            .unsubscribe("https://push.example/1", Some(&["o1".to_owned()]))
            .unwrap());
        assert!(store.unsubscribe("https://push.example/2", None).unwrap());
        assert!(!store.unsubscribe("https://push.example/2", None).unwrap());
        assert!(store.subscriptions().unwrap().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_conforms() {
        conformance::all(&SqliteCompanion::memory());
    }

    #[test]
    fn sqlite_shares_the_registrys_file() {
        let dir = std::env::temp_dir().join(format!("by-companion-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = SqliteCompanion::path_in(&dir);
        let registry = crate::store::SqliteStore::open(&path, None).unwrap();
        let a = SqliteCompanion::open(&path).unwrap();
        let b = SqliteCompanion::open(&path).unwrap();
        let pairing = Pairing {
            code_sha256: "x".into(),
            principal: conformance::principal("p", "default"),
            token_ttl_ms: 10,
            created_at_ms: 0,
            expires_at_ms: 10,
        };
        assert!(a.create_pairing(&pairing, 0).unwrap());
        assert!(b.redeem("x", "y", None, 1).unwrap().is_some());
        assert!(a.token("y").unwrap().is_some());
        drop(registry);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
