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
            PairError::Unreadable { path, source } => io::Error::new(
                source.kind(),
                format!("{} could not be read: {source}", path.display()),
            ),
        }
    }
}

/// The size of a SQLite database file's header, and of a write-ahead log's.
const DB_HEADER: usize = 100;
const WAL_HEADER: usize = 32;
/// Links followed before giving up on a chain. This is where a plain open of
/// a file stops working (Linux's `MAXSYMLINKS`); the bundled SQLite resolves
/// up to 200 itself (`SQLITE_MAX_SYMLINK`), so a longer chain is refused, not
/// followed on.
const MAX_LINKS: usize = 40;

/// The advice for a pair whose files do not belong together.
const MOVE_BOTH: &str = "this pair was not written by SQLite for this database; move both files away to start with a new store, or restore them from a copy you trust";

/// The advice for a log whose database is missing or empty: the one file to
/// move, and the way to keep the store.
const NO_DATABASE: &str = "a log cannot be used without its database; to start with a new store, move the log away; to keep the store, restore both files from a copy you trust";

/// Which file of the pair is meant.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Log,
    Database,
}

/// The names of the pair: as built (what is opened) and as shown (where the
/// files are, for a refusal), and the path the caller gave.
struct Names {
    given: PathBuf,
    db: PathBuf,
    wal: PathBuf,
    db_raw: PathBuf,
    wal_raw: PathBuf,
}

impl Names {
    /// A refusal that names the database, the log and the path given.
    fn refuse(&self, found: impl fmt::Display, advice: &str) -> PairError {
        self.refusal(format_args!("{found} (log {})", self.wal.display()), advice)
    }

    /// A refusal whose `found` already names the file at fault.
    fn refusal(&self, found: impl fmt::Display, advice: &str) -> PairError {
        let given = if self.given == self.db {
            String::new()
        } else {
            format!(" (given as {})", self.given.display())
        };
        PairError::Refused(format!(
            "refusing to open {}{given}: {found}; {advice}",
            self.db.display()
        ))
    }

    /// The file of `role` as shown.
    fn of(&self, role: Role) -> &Path {
        match role {
            Role::Log => &self.wal,
            Role::Database => &self.db,
        }
    }

    /// The file of `role` as built: what is opened.
    fn raw(&self, role: Role) -> &Path {
        match role {
            Role::Log => &self.wal_raw,
            Role::Database => &self.db_raw,
        }
    }
}

/// `path` with its directory resolved, the file name kept; `path` as built
/// when the directory cannot be resolved.
fn located(path: &Path) -> PathBuf {
    let dir = match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => return path.to_path_buf(),
    };
    match (dir.canonicalize(), path.file_name()) {
        (Ok(real), Some(name)) => real.join(name),
        _ => path.to_path_buf(),
    }
}

/// Whether SQLite reads `path` as a URI (it compares the first five bytes).
fn reads_as_uri(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        path.as_os_str().as_bytes().starts_with(b"file:")
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().starts_with("file:")
    }
}

/// The file SQLite opens for `path`: a symbolic link is followed to its
/// target, and the log sits beside the target, not beside the link. A name
/// that is still a link after [`MAX_LINKS`] (a longer chain, or a loop) is
/// `None`: the check never goes on with a link as if it were the database.
fn resolved(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINKS {
        let Ok(target) = std::fs::read_link(&current) else {
            return Some(current);
        };
        current = match current.parent() {
            Some(parent) => parent.join(target),
            None => target,
        };
    }
    match std::fs::read_link(&current) {
        Ok(_) => None,
        Err(_) => Some(current),
    }
}

/// What opening a file of the pair found.
enum Opened {
    Missing,
    /// A symbolic link at the name (the open does not follow one).
    Link,
    File(std::fs::File),
}

/// Open `path` as SQLite opens a log: without following a link at the name,
/// and, unlike SQLite, without blocking (a FIFO with no writer would wait for
/// ever).
#[cfg(unix)]
fn open_plain(path: &Path) -> io::Result<Opened> {
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => Ok(Opened::File(std::fs::File::from(fd))),
        Err(Errno::NOENT) => Ok(Opened::Missing),
        // `O_NOFOLLOW` on a link: ELOOP (EMLINK on some BSDs). ELOOP also
        // means too many links in a directory part; only a link at the name
        // is the case this reports.
        Err(Errno::LOOP | Errno::MLINK)
            if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) =>
        {
            Ok(Opened::Link)
        }
        Err(e) => Err(io::Error::from(e)),
    }
}

/// Where `O_NOFOLLOW` and `O_NONBLOCK` are not available: look at the name
/// first (a second lookup, so a swap in between is not stopped), then open.
/// There is no FIFO to block on where this builds; the type of the handle is
/// still judged by [`head`].
#[cfg(not(unix))]
fn open_plain(path: &Path) -> io::Result<Opened> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Ok(Opened::Link),
        Ok(_) => std::fs::File::open(path).map(Opened::File),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Opened::Missing),
        Err(e) => Err(e),
    }
}

/// What kind of file `file` is, in words, when it is not a regular one.
fn kind_of(file_type: std::fs::FileType) -> &'static str {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        if file_type.is_fifo() {
            return "a FIFO";
        }
        if file_type.is_socket() {
            return "a socket";
        }
        if file_type.is_char_device() || file_type.is_block_device() {
            return "a device";
        }
    }
    if file_type.is_dir() {
        "a directory"
    } else {
        "something other than a regular file"
    }
}

/// Up to `limit` leading bytes of the file at `role`; `None` when it does not
/// exist. Refuses a symbolic link (SQLite does not open a log through one; a
/// database's links were resolved before) and anything that is not a regular
/// file, judged by the type of the handle that was opened.
fn head(names: &Names, role: Role, limit: usize) -> Result<Option<Vec<u8>>, PairError> {
    use std::io::Read as _;
    let path = names.of(role);
    let unreadable = |source| PairError::Unreadable {
        path: path.to_path_buf(),
        source,
    };
    let file = match open_plain(names.raw(role)).map_err(unreadable)? {
        Opened::Missing => return Ok(None),
        Opened::Link => {
            return Err(match role {
                Role::Log => names.refusal(
                    format_args!(
                        "the write-ahead log {} is a symbolic link, and SQLite does not open a log through one",
                        path.display()
                    ),
                    "move the link away, or replace it with the log itself",
                ),
                Role::Database => names.refusal(
                    format_args!(
                        "{} is still a symbolic link after {MAX_LINKS} links, longer than the check follows",
                        path.display()
                    ),
                    "point the path at the database file itself",
                ),
            });
        }
        Opened::File(file) => file,
    };
    let metadata = file.metadata().map_err(unreadable)?;
    if !metadata.is_file() {
        let what = match role {
            Role::Log => "the write-ahead log",
            Role::Database => "the database",
        };
        let found = format_args!(
            "{what} {} is {}, not a regular file",
            path.display(),
            kind_of(metadata.file_type())
        );
        let advice = "SQLite cannot use it; move it away (nothing was opened or changed)";
        return Err(match role {
            Role::Log => names.refusal(found, advice),
            Role::Database => names.refuse(found, advice),
        });
    }
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
/// big-endian) and refuses a pair that disagrees. It judges nothing else about
/// the contents: a pair SQLite wrote always agrees, whatever its magic number
/// or checksums say. The log is read first and the database second, so a store
/// another process is creating is never refused for the file it has not yet
/// written.
///
/// - A path whose text begins with `file:`: refused, because SQLite reads such
///   a name as a URI and opens the file the URI names, not the one checked.
///   Write it as an absolute path or one that begins with `./`.
/// - A symbolic link at `path` is followed, up to 40 links in a row (SQLite
///   itself follows up to 200), and the log checked is the one beside the
///   target, as SQLite opens it. A chain of more than 40 links, or a loop, is
///   refused: the check never goes on with a link as if it were the database.
/// - No log, an empty log or one shorter than its 32-byte header: nothing to
///   compare; `Ok`.
/// - A log that is a symbolic link (SQLite does not open a log through one)
///   or not a regular file (a FIFO, a directory, a device): refused. Both
///   files are opened without blocking and, at their last name, without
///   following a link; their type is read from the handle.
/// - A log of 32 bytes or more beside a database that is missing or empty:
///   refused, as is one beside a database that is not a regular file. A log
///   cannot be used without its database; SQLite used to remove such a log
///   unread, and it may hold committed changes.
/// - A log of 32 bytes or more beside a database shorter than its 100-byte
///   header (SQLite writes the database header before any log in WAL mode),
///   or with another page size: refused.
///
/// A refusal names the database and the log where they are (the directory
/// resolved), and the path given when that differs. The check and the open are
/// two steps: a process that swaps the files between them is not stopped.
///
/// # Errors
///
/// [`PairError::Refused`] for a pair or a name SQLite cannot be given,
/// [`PairError::Unreadable`] when a file of the pair exists and cannot be
/// opened or read; both name the path.
pub fn check_wal_pair(path: &Path) -> Result<(), PairError> {
    if reads_as_uri(path) {
        return Err(PairError::Refused(format!(
            "refusing to open {}: SQLite reads a name that begins with `file:` as a URI and would open another file than the one checked; write the path as an absolute path, or a relative one that begins with `./`",
            path.display()
        )));
    }
    let Some(db_raw) = resolved(path) else {
        return Err(PairError::Refused(format!(
            "refusing to open {}: it is a chain of more than {MAX_LINKS} symbolic links (or a loop of them), longer than the check follows, so the log SQLite would open cannot be named; point the path at the database file itself",
            path.display()
        )));
    };
    let mut wal_name = db_raw.clone().into_os_string();
    wal_name.push("-wal");
    let wal_raw = PathBuf::from(wal_name);
    let names = Names {
        given: path.to_path_buf(),
        db: located(&db_raw),
        wal: located(&wal_raw),
        db_raw,
        wal_raw,
    };
    let Some(wal) = head(&names, Role::Log, WAL_HEADER)? else {
        return Ok(());
    };
    if wal.len() < WAL_HEADER {
        return Ok(());
    }
    let wal_size = u32::from_be_bytes([wal[8], wal[9], wal[10], wal[11]]);
    match head(&names, Role::Database, DB_HEADER)? {
        None => Err(names.refuse(
            "the write-ahead log has no database: the database file is missing",
            NO_DATABASE,
        )),
        Some(db) if db.is_empty() => Err(names.refuse(
            "the write-ahead log has no database: the database file is empty",
            NO_DATABASE,
        )),
        Some(db) if db.len() < DB_HEADER => Err(names.refuse(
            format_args!(
                "the database file is {} bytes, shorter than its {DB_HEADER}-byte header",
                db.len()
            ),
            MOVE_BOTH,
        )),
        Some(db) => {
            let field = u16::from_be_bytes([db[16], db[17]]);
            let db_size = if field == 1 { 65_536 } else { u32::from(field) };
            if db_size == wal_size {
                return Ok(());
            }
            Err(names.refuse(
                format_args!(
                    "the write-ahead log records a page size of {wal_size} bytes but the database records {db_size}"
                ),
                MOVE_BOTH,
            ))
        }
    }
}
