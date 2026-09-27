//! Where operation records persist. [`OperationStore`] is the seam for the
//! PostgreSQL store of `docs/design.md` §8 and `docs/durability.md`;
//! [`SqliteStore`] is what the server uses. [`FileStore`], the JSON-lines
//! file earlier versions used, is imported once and kept for embedding.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use branchyard_client::api::Operation;
use serde::{Deserialize, Serialize};

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
}

/// Durable operation records.
///
/// `save` must not return until the record would survive a crash: the
/// server answers `202 Accepted` only after it, which is what makes an
/// idempotent retry safe (invariant 4).
pub trait OperationStore: Send + Sync {
    /// Every operation as last saved, oldest first.
    fn load(&self) -> io::Result<Vec<StoredOperation>>;
    /// Insert or replace one operation.
    fn save(&self, operation: &StoredOperation) -> io::Result<()>;
}

/// Operations as JSON lines in one append-only file, each save fsynced.
/// The latest line for an ID wins. Compacted when opened.
///
/// Single-process: two servers must not share a data directory.
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

impl OperationStore for FileStore {
    fn load(&self) -> io::Result<Vec<StoredOperation>> {
        read_latest(&self.path)
    }

    fn save(&self, operation: &StoredOperation) -> io::Result<()> {
        let mut line = serde_json::to_vec(operation)?;
        line.push(b'\n');
        let mut file = self.file.lock().unwrap_or_else(|p| p.into_inner());
        file.write_all(&line)?;
        file.sync_data()
    }
}

/// Operations in SQLite at `DATA-DIR/state.db`, in write-ahead-log mode,
/// each save committed with `synchronous=FULL` before it returns.
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

impl SqliteStore {
    /// Open or create the database at `path`. If `legacy` names an
    /// `operations.jsonl` left by an earlier version, its operations are
    /// imported once and the file is renamed to `operations.jsonl.imported`.
    pub fn open(path: impl Into<PathBuf>, legacy: Option<&Path>) -> io::Result<SqliteStore> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(&path).map_err(sql)?;
        conn.busy_timeout(std::time::Duration::from_secs(30))
            .map_err(sql)?;
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .map_err(sql)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(io::Error::other(format!(
                "{} could not use write-ahead logging (journal mode {mode})",
                path.display()
            )));
        }
        conn.execute_batch(
            "PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS operations (
                 id TEXT PRIMARY KEY,
                 seq INTEGER NOT NULL,
                 body TEXT NOT NULL
             );",
        )
        .map_err(sql)?;
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

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Save `operation`; `replace` keeps an existing one otherwise.
    fn insert(&self, operation: &StoredOperation, replace: bool) -> io::Result<()> {
        let body = serde_json::to_string(operation)?;
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let conflict = match replace {
            true => "DO UPDATE SET body = excluded.body",
            false => "DO NOTHING",
        };
        conn.execute(
            &format!(
                "INSERT INTO operations (id, seq, body) \
                 VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM operations), ?2) \
                 ON CONFLICT (id) {conflict}"
            ),
            rusqlite::params![operation.operation.id, body],
        )
        .map_err(sql)?;
        Ok(())
    }
}

impl OperationStore for SqliteStore {
    fn load(&self) -> io::Result<Vec<StoredOperation>> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut statement = conn
            .prepare("SELECT id, body FROM operations ORDER BY seq")
            .map_err(sql)?;
        let rows = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(sql)?;
        let mut ops = Vec::new();
        for row in rows {
            let (id, body) = row.map_err(sql)?;
            ops.push(serde_json::from_str(&body).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} operation {id}: {e}", self.path.display()),
                )
            })?);
        }
        Ok(ops)
    }

    fn save(&self, operation: &StoredOperation) -> io::Result<()> {
        self.insert(operation, true)
    }
}

/// Operations in memory only, for tests and embedding.
#[derive(Default)]
pub struct MemoryStore {
    ops: Mutex<Vec<StoredOperation>>,
}

impl OperationStore for MemoryStore {
    fn load(&self) -> io::Result<Vec<StoredOperation>> {
        Ok(self.ops.lock().unwrap_or_else(|p| p.into_inner()).clone())
    }

    fn save(&self, operation: &StoredOperation) -> io::Result<()> {
        let mut ops = self.ops.lock().unwrap_or_else(|p| p.into_inner());
        match ops
            .iter_mut()
            .find(|op| op.operation.id == operation.operation.id)
        {
            Some(existing) => *existing = operation.clone(),
            None => ops.push(operation.clone()),
        }
        Ok(())
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
            },
            idempotency: None,
            locks: Vec::new(),
        }
    }

    #[test]
    fn the_latest_save_wins_and_survives_reopening() {
        let dir = std::env::temp_dir().join(format!("branchyard-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
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
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sqlite_imports_the_file_once_and_keeps_order_and_the_latest_save() {
        let dir =
            std::env::temp_dir().join(format!("branchyard-sqlite-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let legacy = dir.join("operations.jsonl");
        let file = FileStore::open(&legacy).unwrap();
        file.save(&op("a", OperationState::Queued)).unwrap();
        file.save(&op("b", OperationState::Succeeded)).unwrap();
        drop(file);
        let db = dir.join("state.db");
        let store = SqliteStore::open(&db, Some(&legacy)).unwrap();
        assert!(!legacy.exists());
        assert!(dir.join("operations.jsonl.imported").is_file());
        store.save(&op("a", OperationState::Interrupted)).unwrap();
        store.save(&op("c", OperationState::Queued)).unwrap();
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
                ("c".to_owned(), OperationState::Queued),
            ]
        );
        let _ = fs::remove_dir_all(dir);
    }
}
