//! Refuse a SQLite database whose write-ahead log has another page size,
//! before SQLite is given the path.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Why a database and its write-ahead log were not handed to SQLite.
#[derive(Debug)]
pub enum PairError {
    /// The pair cannot have been written by SQLite for this database.
    Refused(String),
    /// A file of the pair could not be read.
    Unreadable {
        /// The file.
        path: PathBuf,
        /// What the system said.
        source: io::Error,
    },
}

impl fmt::Display for PairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PairError::Refused(message) => f.write_str(message),
            PairError::Unreadable { path, source } => {
                write!(f, "{} could not be read: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for PairError {}

impl From<PairError> for io::Error {
    fn from(error: PairError) -> io::Error {
        match error {
            PairError::Refused(message) => io::Error::new(io::ErrorKind::InvalidData, message),
            PairError::Unreadable { source, .. } => source,
        }
    }
}

/// The size of a SQLite database file's header, and of a write-ahead log's.
const DB_HEADER: usize = 100;
const WAL_HEADER: usize = 32;
/// Links followed before giving up on a chain (SQLite's own bound is 40).
const MAX_LINKS: usize = 40;

/// The file SQLite opens for `path`: a symbolic link is followed to its
/// target, and the log sits beside the target, not beside the link.
fn resolved(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINKS {
        let Ok(target) = std::fs::read_link(&current) else {
            break;
        };
        current = match current.parent() {
            Some(parent) => parent.join(target),
            None => target,
        };
    }
    current
}

/// Up to `limit` leading bytes of `path`; `None` when it does not exist.
fn head(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, PairError> {
    use std::io::Read as _;
    let unreadable = |source| PairError::Unreadable {
        path: path.to_path_buf(),
        source,
    };
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e)),
    };
    let mut bytes = Vec::with_capacity(limit);
    file.take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    Ok(Some(bytes))
}

/// Check the database at `path` and its write-ahead log (`<path>-wal`)
/// before SQLite is given the path.
///
/// SQLite before 3.54.0 can return uninitialized memory when a database is
/// opened with a log whose page size differs from the database's
/// (RUSTSEC-2026-0459). This reads the two page sizes itself (bytes 16 and 17
/// of the database, big-endian, 1 meaning 65536; bytes 8 to 11 of the log,
/// big-endian) and refuses a pair that disagrees. It judges nothing else: a
/// pair SQLite wrote always agrees, whatever its magic number or checksums say.
///
/// - No log, an empty log or one shorter than its 32-byte header: nothing to
///   compare; `Ok`.
/// - A log of 32 bytes or more beside a database that is missing or shorter
///   than its 100-byte header: refused (SQLite writes the database header
///   before any log in WAL mode).
/// - A log of 32 bytes or more with another page size: refused.
///
/// A symbolic link at `path` is followed the way SQLite follows it, so the log
/// checked is the one beside the target. The check and the open are two steps:
/// a process that swaps the files between them is not stopped.
///
/// # Errors
///
/// [`PairError::Refused`] for a pair SQLite did not write for this database,
/// [`PairError::Unreadable`] when a file of the pair exists and cannot be
/// read.
pub fn check_wal_pair(path: &Path) -> Result<(), PairError> {
    let db_path = resolved(path);
    let mut wal_name = db_path.clone().into_os_string();
    wal_name.push("-wal");
    let wal_path = PathBuf::from(wal_name);
    let Some(wal) = head(&wal_path, WAL_HEADER)? else {
        return Ok(());
    };
    if wal.len() < WAL_HEADER {
        return Ok(());
    }
    let wal_size = u32::from_be_bytes([wal[8], wal[9], wal[10], wal[11]]);
    let advice = "this pair was not written by SQLite for this database; move both files away to start with a new store, or restore them from a copy you trust";
    let found = match head(&db_path, DB_HEADER)? {
        None => "the database file is missing".to_string(),
        Some(db) if db.len() < DB_HEADER => format!(
            "the database file is {} bytes, shorter than its {DB_HEADER}-byte header",
            db.len()
        ),
        Some(db) => {
            let field = u16::from_be_bytes([db[16], db[17]]);
            let db_size = if field == 1 { 65_536 } else { u32::from(field) };
            if db_size == wal_size {
                return Ok(());
            }
            format!(
                "the write-ahead log records a page size of {wal_size} bytes but the database records {db_size}"
            )
        }
    };
    Err(PairError::Refused(format!(
        "refusing to open {}: {found} (log {}); {advice}",
        db_path.display(),
        wal_path.display()
    )))
}
