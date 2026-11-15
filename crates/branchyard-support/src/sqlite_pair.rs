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
                write!(f, "{} could not be read: {source}", shown(path))
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
                format!("{} could not be read: {source}", shown(&path)),
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

/// A path as a refusal shows it: a control character (a newline, an escape)
/// or a character that reorders or splits text is written as an escape, so a
/// planted name cannot add a line to the output or hide what follows it.
fn shown(path: &Path) -> String {
    let mut out = String::new();
    for c in path.to_string_lossy().chars() {
        let odd = c.is_control()
            || matches!(c,
                '\u{2028}' | '\u{2029}' | '\u{200e}' | '\u{200f}'
                | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if odd {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// The name of a database that was checked, to be given to SQLite as it is.
///
/// Returned by [`check_wal_pair`]. It is an absolute path with no `.` or `..`
/// element and no empty one, and none of its parts, the last included, was a
/// symbolic link when it was made; the log that was checked is `<name>-wal`.
/// SQLite builds the full name of a file it opens by itself and drops `.` and
/// `..` from the text without looking at the file system, so a name that
/// SQLite would resolve differently from the system is exactly what the check
/// must not leave to it: for this name its resolution returns the same bytes.
/// (The bundled SQLite refuses to open a name of 505 bytes or more at all; that
/// limit is its own and was the same before the check, 504 bytes open.)
///
/// Open this, not the path the check was given. Dropping it unused is a
/// warning, and clippy turns that into an error.
#[must_use = "open the database at the name the check returned, not at the path it was given"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedPath(PathBuf);

impl CheckedPath {
    /// The plain name to give SQLite, and to use for anything else that
    /// touches the file before it is opened.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// The plain name, owned.
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for CheckedPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// The names of the pair as they are (both plain), and the path the caller
/// gave.
struct Names {
    given: PathBuf,
    db: PathBuf,
    wal: PathBuf,
}

impl Names {
    /// A refusal that names the database, the log and the path given.
    fn refuse(&self, found: impl fmt::Display, advice: &str) -> PairError {
        self.refusal(format_args!("{found} (log {})", shown(&self.wal)), advice)
    }

    /// A refusal whose `found` already names the file at fault.
    fn refusal(&self, found: impl fmt::Display, advice: &str) -> PairError {
        PairError::Refused(format!(
            "refusing to open {}{}: {found}; {advice}",
            shown(&self.db),
            given_note(&self.given, &self.db)
        ))
    }

    /// The file of `role`.
    fn of(&self, role: Role) -> &Path {
        match role {
            Role::Log => &self.wal,
            Role::Database => &self.db,
        }
    }
}

/// ` (given as ...)` when the path given is not the name checked.
fn given_note(given: &Path, plain: &Path) -> String {
    if given == plain {
        String::new()
    } else {
        format!(" (given as {})", shown(given))
    }
}

/// Whether `path` is exactly `:memory:`, the name for which SQLite opens an
/// in-memory database and no file.
fn is_memory_name(path: &Path) -> bool {
    path.as_os_str() == ":memory:"
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

/// What the person can do about a directory that cannot be resolved, by cause.
/// A cause not listed gets no advice, only the system's words.
fn resolve_advice(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "; create the directory, or point the path at one that exists",
        io::ErrorKind::PermissionDenied => {
            "; a directory on the way cannot be searched: change its permissions, or point the path at another"
        }
        io::ErrorKind::NotADirectory => {
            "; a part of the path is a file, not a directory: point the path at a directory"
        }
        _ => "",
    }
}

/// The plain name of the file `given` names: the system resolves every
/// directory (`canonicalize`), and a link at the last name is read and
/// followed, at most [`MAX_LINKS`] of them. The name of a file that does not
/// exist yet is its resolved directory and its name.
fn plain_name(given: &Path) -> Result<PathBuf, PairError> {
    let refuse =
        |why: String| PairError::Refused(format!("refusing to open {}: {why}", shown(given)));
    let mut current = given.to_path_buf();
    // The link whose target `current` is, and the target as it was read.
    let mut via: Option<(PathBuf, PathBuf)> = None;
    for round in 0..=MAX_LINKS {
        let Some(last) = current.file_name() else {
            return Err(refuse(format!(
                "{} has no file name (it ends in `..`, or is a root), so there is no file to check; give the path of the database file",
                shown(&current)
            )));
        };
        let dir = match current.parent() {
            Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
            Some(parent) => parent,
            None => return Err(refuse(format!("{} has no directory", shown(&current)))),
        };
        let dir = dir.canonicalize().map_err(|e| {
            let advice = resolve_advice(e.kind());
            let what = match &via {
                Some((link, target)) => format!(
                    "{} is a symbolic link to {}, and the directory {} of that target",
                    shown(link),
                    shown(target),
                    shown(dir)
                ),
                None => format!("its directory {}", shown(dir)),
            };
            refuse(format!(
                "{what} cannot be resolved ({e}), so the file cannot be named{advice}"
            ))
        })?;
        let name = dir.join(last);
        match std::fs::symlink_metadata(&name) {
            Ok(m) if m.file_type().is_symlink() => {}
            // A new database, or a file; a name below a file is read as a
            // failure by the reads that follow, with the log's name.
            Ok(_) => return Ok(name),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(name);
            }
            Err(source) => return Err(PairError::Unreadable { path: name, source }),
        }
        if round == MAX_LINKS {
            break;
        }
        let target = std::fs::read_link(&name).map_err(|source| PairError::Unreadable {
            path: name.clone(),
            source,
        })?;
        // A relative target is read from the directory the link is in, which
        // is the resolved one; an absolute target replaces everything.
        current = dir.join(&target);
        via = Some((name, target));
    }
    Err(refuse(format!(
        "it is a chain of more than {MAX_LINKS} symbolic links (or a loop of them), longer than the check follows, so the file SQLite would open cannot be named; point the path at the database file itself"
    )))
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
/// ever). Only called for a name that was just seen to be a regular file.
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

/// Where `O_NOFOLLOW` and `O_NONBLOCK` are not available (this branch is not
/// built or tested; only Unix is): the name was looked at just before, and
/// this opens it with a second lookup, so a swap in between is not stopped.
/// The type of the handle is still judged by [`head`].
#[cfg(not(unix))]
fn open_plain(path: &Path) -> io::Result<Opened> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Ok(Opened::Link),
        Ok(_) => std::fs::File::open(path).map(Opened::File),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Opened::Missing),
        Err(e) => Err(e),
    }
}

/// What kind of file `file_type` is, in words, when it is not a regular one.
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
/// exist. The type of the name is read first, without opening it
/// (`symlink_metadata`), so that a FIFO is not opened (which would release a
/// writer waiting on it) nor a device (which would run its driver): a symbolic
/// link and anything that is not a regular file is refused there, naming what
/// it is. Only a regular file is opened, without blocking and without
/// following a link, and the type of the handle is judged again, so a swap
/// between the two looks cannot make the read come from something else.
fn head(names: &Names, role: Role, limit: usize) -> Result<Option<Vec<u8>>, PairError> {
    use std::io::Read as _;
    let path = names.of(role);
    let unreadable = |source| PairError::Unreadable {
        path: path.to_path_buf(),
        source,
    };
    let what = match role {
        Role::Log => "the write-ahead log",
        Role::Database => "the database",
    };
    // `opened`: whether the file had been opened when the refusal is made.
    let not_regular = |file_type: std::fs::FileType, opened: bool| {
        let found = format_args!(
            "{what} {} is {}, not a regular file",
            shown(path),
            kind_of(file_type)
        );
        let advice = if opened {
            "SQLite cannot use it; move it away (it had been opened without blocking and was not read or changed)"
        } else {
            "SQLite cannot use it; move it away (nothing was opened or changed)"
        };
        match role {
            Role::Log => names.refusal(found, advice),
            Role::Database => names.refuse(found, advice),
        }
    };
    let link = || {
        match role {
        Role::Log => names.refusal(
            format_args!(
                "the write-ahead log {} is a symbolic link, and SQLite does not open a log through one",
                shown(path)
            ),
            "move the link away, or replace it with the log itself",
        ),
        Role::Database => names.refusal(
            format_args!(
                "the database {} is a symbolic link, though it was not one when the path was resolved; the files changed while the check ran",
                shown(path)
            ),
            "run it again",
        ),
    }
    };
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => return Err(link()),
        Ok(m) if !m.is_file() => return Err(not_regular(m.file_type(), false)),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e)),
    }
    let file = match open_plain(path).map_err(unreadable)? {
        Opened::Missing => return Ok(None),
        Opened::Link => return Err(link()),
        Opened::File(file) => file,
    };
    let metadata = file.metadata().map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(not_regular(metadata.file_type(), true));
    }
    let mut bytes = Vec::with_capacity(limit);
    file.take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    Ok(Some(bytes))
}

/// Check the database at `path` and its write-ahead log before SQLite is
/// given the file, and return the name SQLite must be given.
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
/// The file the check reads must be the file SQLite opens, so the check does
/// not leave the resolution of `path` to SQLite (which drops `.` and `..` from
/// the text, keeps a missing element without error and follows links by
/// itself, and so can arrive at another file than the system does). It
/// resolves `path` itself, with the system, into a plain name ([`CheckedPath`]:
/// absolute, no `.` or `..`, no link in it, the last element included), checks
/// that, and returns it. **Open the returned name, and use it for anything
/// else that touches the file between the check and the open**, never `path`.
/// Nothing else uses it: a store keeps, returns and prints the path it was
/// given. The function must be called after the directory exists: a directory
/// that cannot be resolved is refused.
///
/// What the returned name is worth: for every path the check accepts, SQLite's
/// own resolution of that path reaches the same file, because the cases where
/// the two differ are refused (`..` after a missing element or a file, a
/// `file:` name, `:memory:`, the empty name, a chain of more than 40 links).
/// Giving SQLite the returned name is therefore a second line of defence, and
/// no test of outcomes can tell a site that does from one that does not (three
/// sites were changed to give SQLite the original path, and every test still
/// passed); the `must_use` warning is what holds a site to it.
///
/// - A path whose text begins with `file:` is refused: SQLite reads such a
///   name as a URI, not as a path, and Branchyard gives SQLite paths only.
///   Write it as an absolute path or one that begins with `./`. The exact
///   name `:memory:` is refused too (SQLite opens an in-memory database for
///   it and no file; `./:memory:` names a file), and so is the empty path,
///   which has no file name.
/// - Each directory is resolved with the system, so a directory part that
///   cannot be resolved (it is missing, `nope/..` after a missing element,
///   not searchable, below a file) is refused, with advice that follows the
///   cause, naming the link and its target when the directory came from a
///   link; a path that ends in `..` or is a root has no file name and is
///   refused.
/// - A symbolic link at the last name is followed, up to 40 links in a row
///   (SQLite itself follows up to 200), and the log checked is the one beside
///   the target, as SQLite opens it. A chain of more than 40 links, or a
///   loop, is refused. A link whose target does not exist is followed to the
///   name of the target.
/// - No log, an empty log or one shorter than its 32-byte header: nothing to
///   compare; `Ok`.
/// - A log that is a symbolic link (SQLite does not open a log through one)
///   or not a regular file (a FIFO, a socket, a directory, a device): refused,
///   naming what it is. The type of a name is read without opening it; only a
///   regular file is opened, without blocking and without following a link,
///   and its type is read again from the handle. Both files are treated so.
/// - A log of 32 bytes or more beside a database that is missing or empty:
///   refused, as is one beside a database that is not a regular file. A log
///   cannot be used without its database; SQLite used to remove such a log
///   unread, and it may hold committed changes.
/// - A log of 32 bytes or more beside a database shorter than its 100-byte
///   header (SQLite writes the database header before any log in WAL mode),
///   or with another page size: refused.
///
/// A refusal names the database and the log where they are, and the path
/// given when that differs; control characters in any name it shows are
/// written as escapes.
///
/// Not stopped: the check and the open are two steps. Whoever can change the
/// files, or a directory above them, between the two (replace a file or a
/// directory by another, plant a log) is not stopped, and the check cannot see
/// a log written after it. Only Unix is built and tested; elsewhere the files
/// are looked at and then opened by name, with a second lookup, so a swap is
/// no less possible.
///
/// # Errors
///
/// [`PairError::Refused`] for a pair or a name SQLite cannot be given,
/// [`PairError::Unreadable`] when a file of the pair exists and cannot be
/// looked at, opened or read; both name the path.
pub fn check_wal_pair(path: &Path) -> Result<CheckedPath, PairError> {
    if path.as_os_str().is_empty() {
        return Err(PairError::Refused(
            "refusing to open an empty path: the path is empty, so there is no file to check; give the path of the database file".to_owned(),
        ));
    }
    if is_memory_name(path) {
        return Err(PairError::Refused(
            "refusing to open `:memory:`: SQLite opens an in-memory database for that name, not a file; to name a file called `:memory:`, write it as `./:memory:`".to_owned(),
        ));
    }
    if reads_as_uri(path) {
        return Err(PairError::Refused(format!(
            "refusing to open {}: SQLite reads a name that begins with `file:` as a URI, not as a path, and Branchyard gives SQLite paths only; write the path as an absolute path, or a relative one that begins with `./`",
            shown(path)
        )));
    }
    let db = plain_name(path)?;
    let mut wal_name = db.clone().into_os_string();
    wal_name.push("-wal");
    let names = Names {
        given: path.to_path_buf(),
        wal: PathBuf::from(wal_name),
        db,
    };
    let checked = CheckedPath(names.db.clone());
    let Some(wal) = head(&names, Role::Log, WAL_HEADER)? else {
        return Ok(checked);
    };
    if wal.len() < WAL_HEADER {
        return Ok(checked);
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
                return Ok(checked);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_advice_for_a_directory_follows_the_cause() {
        assert!(resolve_advice(io::ErrorKind::NotFound).contains("create the directory"));
        let denied = resolve_advice(io::ErrorKind::PermissionDenied);
        assert!(denied.contains("cannot be searched"), "{denied}");
        assert!(!denied.contains("create the directory"), "{denied}");
        let file = resolve_advice(io::ErrorKind::NotADirectory);
        assert!(file.contains("is a file"), "{file}");
        assert!(!file.contains("create the directory"), "{file}");
        // Any other cause: the system's words only, no advice that might be
        // wrong.
        assert_eq!(resolve_advice(io::ErrorKind::TimedOut), "");
        assert_eq!(resolve_advice(io::ErrorKind::Other), "");
    }
}
