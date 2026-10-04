//! The registry's rows in SQLite: [`LocalRegistry`], a repository's own
//! file, and [`SqliteRows`], which a server's operation store uses on its
//! own connection with [`SCHEMA`] among its tables.

use branchyard_support::LockExt as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};

use super::{decode, encode, Rows, Service, ServiceState, ServiceStore};
use crate::store_codec::{from_db, to_db};

/// The registry's tables: one row per record, its body JSON, and the
/// change counter.
pub const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS services (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        owner TEXT NOT NULL,
        state TEXT NOT NULL,
        lease_until INTEGER NOT NULL,
        changed INTEGER NOT NULL,
        seq INTEGER NOT NULL,
        body TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS services_seq ON services (seq);
    CREATE TABLE IF NOT EXISTS service_seq (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        seq INTEGER NOT NULL
    );
    INSERT OR IGNORE INTO service_seq (id, seq) VALUES (1, 0);";

fn sql(error: rusqlite::Error) -> io::Error {
    io::Error::other(format!("service registry: {error}"))
}

/// The registry's rows on a SQLite connection inside a transaction.
pub struct SqliteRows<'a>(pub &'a Connection);

const COLUMNS: &str = "body, state, lease_until, changed, seq";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, i64, i64, i64)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}

fn service(
    (body, state, lease, changed, seq): (String, String, i64, i64, i64),
) -> io::Result<Service> {
    decode(
        &body,
        &state,
        from_db("lease", lease)?,
        from_db("changed", changed)?,
        from_db("seq", seq)?,
    )
}

impl SqliteRows<'_> {
    fn query(&self, text: &str, param: i64) -> io::Result<Vec<Service>> {
        let mut statement = self.0.prepare(text).map_err(sql)?;
        let rows = statement
            .query_map([param], row)
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        rows.into_iter().map(service).collect()
    }
}

impl Rows for SqliteRows<'_> {
    fn next_seq(&mut self) -> io::Result<u64> {
        self.0
            .query_row(
                "INSERT INTO service_seq (id, seq) VALUES (1, 1) \
                 ON CONFLICT (id) DO UPDATE SET seq = seq + 1 RETURNING seq",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(sql)
            .and_then(|seq| Ok(from_db("seq", seq)?))
    }

    fn head(&mut self) -> io::Result<u64> {
        self.0
            .query_row("SELECT seq FROM service_seq WHERE id = 1", [], |r| {
                r.get::<_, i64>(0)
            })
            .optional()
            .map_err(sql)
            .and_then(|seq| Ok(from_db("seq", seq.unwrap_or(0))?))
    }

    fn get(&mut self, id: &str) -> io::Result<Option<Service>> {
        self.0
            .query_row(
                &format!("SELECT {COLUMNS} FROM services WHERE id = ?1"),
                [id],
                row,
            )
            .optional()
            .map_err(sql)?
            .map(service)
            .transpose()
    }

    fn put(&mut self, s: &Service) -> io::Result<()> {
        self.0
            .execute(
                "INSERT INTO services (id, kind, owner, state, lease_until, changed, seq, body) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, owner = excluded.owner, \
                     state = excluded.state, lease_until = excluded.lease_until, \
                     changed = excluded.changed, seq = excluded.seq, body = excluded.body",
                params![
                    s.id,
                    s.kind,
                    s.owner.id,
                    s.state.as_str(),
                    to_db("lease_until_ms", s.lease_until_ms)?,
                    to_db("changed_ms", s.changed_ms)?,
                    to_db("seq", s.seq)?,
                    encode(s)?
                ],
            )
            .map_err(sql)?;
        Ok(())
    }

    fn all(&mut self) -> io::Result<Vec<Service>> {
        self.query(
            &format!("SELECT {COLUMNS} FROM services WHERE ?1 = ?1 ORDER BY id"),
            0,
        )
    }

    fn since(&mut self, seq: u64) -> io::Result<Vec<Service>> {
        self.query(
            &format!("SELECT {COLUMNS} FROM services WHERE seq > ?1 ORDER BY seq"),
            to_db("seq", seq)?,
        )
    }

    fn prune(&mut self, before_ms: u64) -> io::Result<usize> {
        self.0
            .execute(
                "DELETE FROM services WHERE state IN (?1, ?2) AND changed < ?3",
                params![
                    ServiceState::Left.as_str(),
                    ServiceState::Reclaimed.as_str(),
                    to_db("before_ms", before_ms)?
                ],
            )
            .map_err(sql)
    }
}

/// Run `f` on `conn` in a `BEGIN IMMEDIATE` transaction, so that writers
/// in every process take turns; committed when it returns `Ok`.
pub fn transact(
    conn: &mut Connection,
    f: &mut (dyn FnMut(&mut dyn Rows) -> io::Result<()> + Send),
) -> io::Result<()> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(sql)?;
    f(&mut SqliteRows(&tx))?;
    tx.commit().map_err(sql)
}

/// A repository's registry: one SQLite file every `by` process on this
/// machine shares, in write-ahead-log mode, made with mode 0600.
pub struct LocalRegistry {
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for LocalRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LocalRegistry({})", self.path.display())
    }
}

/// How long a writer waits for another process's transaction.
const BUSY: Duration = Duration::from_secs(30);

impl LocalRegistry {
    /// Open or create the registry at `path`.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<LocalRegistry> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        private(&path)?;
        let mut conn = Connection::open(&path).map_err(sql)?;
        conn.busy_timeout(BUSY).map_err(sql)?;
        let deadline = std::time::Instant::now() + BUSY;
        let mode: String = loop {
            match conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)) {
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::DatabaseBusy
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                other => break other.map_err(sql)?,
            }
        };
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(io::Error::other(format!(
                "{} could not use write-ahead logging (journal mode {mode})",
                path.display()
            )));
        }
        conn.execute_batch("PRAGMA synchronous = FULL;")
            .map_err(sql)?;
        // Openers take turns making the tables.
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql)?;
        tx.execute_batch(SCHEMA).map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(LocalRegistry {
            path,
            conn: Mutex::new(conn),
        })
    }

    /// An in-memory registry, for tests and embedding.
    #[allow(clippy::expect_used)] // ratchet: branchyard
    pub fn memory() -> LocalRegistry {
        let conn = Connection::open_in_memory().expect("an in-memory database");
        conn.execute_batch(SCHEMA)
            .expect("the schema on an in-memory database");
        LocalRegistry {
            path: PathBuf::from(":memory:"),
            conn: Mutex::new(conn),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Make `path` with mode 0600 if it does not exist, and make an existing
/// one 0600: the registry names processes, sockets and what to reclaim.
fn private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let mode = fs::metadata(path)?.permissions().mode() & 0o777;
            if mode != 0o600 {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

impl ServiceStore for LocalRegistry {
    fn transact(
        &self,
        f: &mut (dyn FnMut(&mut dyn Rows) -> io::Result<()> + Send),
    ) -> io::Result<()> {
        let mut conn = self.conn.lock_recovering("conn");
        transact(&mut conn, f)
    }
}
