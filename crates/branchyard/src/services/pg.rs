//! The registry's rows in PostgreSQL, for a server's operation store: the
//! tables ([`SCHEMA`], one step per object, for the store to run only the
//! steps whose object is missing) and [`PgRows`] on one of its
//! transactions. Every transaction locks the change counter's row first,
//! so writers take turns on it, and one that waited reads what the one
//! before it committed (`READ COMMITTED` reads each statement afresh).

use std::io;

use postgres::Transaction;

use super::{decode, encode, Rows, Service, ServiceState};
use crate::store_codec::{from_db, to_db};

/// The registry's objects and the statements that make them.
pub const SCHEMA: &[(&str, &str)] = &[
    (
        "by_services",
        "CREATE TABLE IF NOT EXISTS by_services (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            owner TEXT NOT NULL,
            state TEXT NOT NULL,
            lease_until BIGINT NOT NULL,
            changed BIGINT NOT NULL,
            seq BIGINT NOT NULL,
            body TEXT NOT NULL
        )",
    ),
    (
        "by_services_seq",
        "CREATE INDEX IF NOT EXISTS by_services_seq ON by_services (seq)",
    ),
    (
        "by_service_seq",
        "CREATE TABLE IF NOT EXISTS by_service_seq (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            seq BIGINT NOT NULL
        )",
    ),
];

fn sql(error: postgres::Error) -> io::Error {
    match error.as_db_error() {
        Some(db) => io::Error::other(format!("service registry: {}", db.message())),
        None => io::Error::other(format!("service registry: {error}")),
    }
}

const COLUMNS: &str = "body, state, lease_until, changed, seq";

fn service(row: &postgres::Row) -> io::Result<Service> {
    let (body, state, lease, changed, seq): (String, String, i64, i64, i64) =
        (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4));
    decode(
        &body,
        &state,
        from_db("lease", lease)?,
        from_db("changed", changed)?,
        from_db("seq", seq)?,
    )
}

/// The registry's rows inside one PostgreSQL transaction.
pub struct PgRows<'a, 'b>(pub &'a mut Transaction<'b>);

impl Rows for PgRows<'_, '_> {
    fn next_seq(&mut self) -> io::Result<u64> {
        let row = self
            .0
            .query_one(
                "INSERT INTO by_service_seq (id, seq) VALUES (1, 1) \
                 ON CONFLICT (id) DO UPDATE SET seq = by_service_seq.seq + 1 RETURNING seq",
                &[],
            )
            .map_err(sql)?;
        Ok(from_db("seq", row.get(0))?)
    }

    fn head(&mut self) -> io::Result<u64> {
        let rows = self
            .0
            .query("SELECT seq FROM by_service_seq WHERE id = 1", &[])
            .map_err(sql)?;
        Ok(from_db(
            "seq",
            rows.first().map_or(0, |r| r.get::<_, i64>(0)),
        )?)
    }

    fn get(&mut self, id: &str) -> io::Result<Option<Service>> {
        let rows = self
            .0
            .query(
                &format!("SELECT {COLUMNS} FROM by_services WHERE id = $1"),
                &[&id],
            )
            .map_err(sql)?;
        rows.first().map(service).transpose()
    }

    fn put(&mut self, s: &Service) -> io::Result<()> {
        let body = encode(s)?;
        self.0
            .execute(
                "INSERT INTO by_services (id, kind, owner, state, lease_until, changed, seq, body) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (id) DO UPDATE SET kind = EXCLUDED.kind, owner = EXCLUDED.owner, \
                     state = EXCLUDED.state, lease_until = EXCLUDED.lease_until, \
                     changed = EXCLUDED.changed, seq = EXCLUDED.seq, body = EXCLUDED.body",
                &[
                    &s.id,
                    &s.kind,
                    &s.owner.id,
                    &s.state.as_str(),
                    &to_db("lease_until_ms", s.lease_until_ms)?,
                    &to_db("changed_ms", s.changed_ms)?,
                    &to_db("seq", s.seq)?,
                    &body,
                ],
            )
            .map_err(sql)?;
        Ok(())
    }

    fn all(&mut self) -> io::Result<Vec<Service>> {
        let rows = self
            .0
            .query(
                &format!("SELECT {COLUMNS} FROM by_services ORDER BY id"),
                &[],
            )
            .map_err(sql)?;
        rows.iter().map(service).collect()
    }

    fn since(&mut self, seq: u64) -> io::Result<Vec<Service>> {
        let rows = self
            .0
            .query(
                &format!("SELECT {COLUMNS} FROM by_services WHERE seq > $1 ORDER BY seq"),
                &[&to_db("seq", seq)?],
            )
            .map_err(sql)?;
        rows.iter().map(service).collect()
    }

    fn prune(&mut self, before_ms: u64) -> io::Result<usize> {
        let n = self
            .0
            .execute(
                "DELETE FROM by_services WHERE state IN ($1, $2) AND changed < $3",
                &[
                    &ServiceState::Left.as_str(),
                    &ServiceState::Reclaimed.as_str(),
                    &to_db("before_ms", before_ms)?,
                ],
            )
            .map_err(sql)?;
        Ok(n as usize)
    }
}

/// Run `f` in a transaction on `client`, committed when it returns `Ok`.
/// The outer error is the connection's; the inner one `f`'s, after which
/// nothing was committed.
pub fn transact(
    client: &mut postgres::Client,
    f: &mut (dyn FnMut(&mut dyn Rows) -> io::Result<()> + Send),
) -> Result<io::Result<()>, postgres::Error> {
    let mut tx = client.transaction()?;
    tx.execute(
        "INSERT INTO by_service_seq (id, seq) VALUES (1, 0) ON CONFLICT (id) DO NOTHING",
        &[],
    )?;
    tx.query(
        "SELECT seq FROM by_service_seq WHERE id = 1 FOR UPDATE",
        &[],
    )?;
    match f(&mut PgRows(&mut tx)) {
        Ok(()) => {
            tx.commit()?;
            Ok(Ok(()))
        }
        Err(e) => Ok(Err(e)),
    }
}
