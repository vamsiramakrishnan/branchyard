//! Refuse a SQLite database whose write-ahead log has another page size,
//! before SQLite is given the path.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Why a database and its write-ahead log were not handed to SQLite.
#[derive(Debug)]
pub enum PairError {
    /// The pair, or the path given for it, is not one to hand to SQLite; the
    /// message says why and what to do.
    Refused(String),
    /// A file of the pair, or the name given for it, could not be looked at,
    /// opened or read.
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
/// about 200 itself (`SQLITE_MAX_SYMLINK`), so a longer chain is refused, not
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
/// SQLite resolves a path by itself and drops `.` and `..` from the text
/// without looking at the file system, so the check resolves the path with the
/// system and returns the result.
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

/// What the person can do about a directory that cannot be used, by cause.
/// A cause not listed gets no advice, only the system's words.
fn resolve_advice(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "; create the directory, or point the path at one that exists",
        io::ErrorKind::PermissionDenied => {
            "; a directory on the way cannot be searched: change its permissions, or point the path at another"
        }
        io::ErrorKind::NotADirectory => {
            "; a part of the path is a file, not a directory: the path has to name a file inside a directory, so point it at one"
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
        // A directory that cannot be used, by cause; the link and its target
        // are named when the directory came from a link's target.
        let unusable = |dir: &Path, how: &str, e: &io::Error| {
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
                "{what} {how} ({e}), so the file cannot be named{advice}"
            ))
        };
        let dir = dir
            .canonicalize()
            .map_err(|e| unusable(dir, "cannot be resolved", &e))?;
        let name = dir.join(last);
        match std::fs::symlink_metadata(&name) {
            Ok(m) if m.file_type().is_symlink() => {}
            // A new database, or a file.
            Ok(_) => return Ok(name),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(name),
            // The resolved directory is a file: refused like a directory that
            // cannot be resolved.
            Err(e) if e.kind() == io::ErrorKind::NotADirectory => {
                return Err(unusable(&dir, "is a file and cannot hold the database", &e));
            }
            // The resolved directory exists and cannot be searched.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                return Err(unusable(&dir, "cannot be searched", &e));
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

/// What reading a file's header found.
enum Read {
    Missing,
    /// A symbolic link at the name (the open does not follow one).
    Link,
    /// The handle is not a regular file; it was closed.
    NotRegular(std::fs::FileType),
    Bytes(Vec<u8>),
}

/// The descriptors this process keeps on the files of the pairs whose headers
/// the check has read: databases and their logs.
///
/// On Unix, SQLite's locks are POSIX record locks (`fcntl(F_SETLK)`), which
/// belong to the process and the file: closing **any** descriptor on a file
/// releases every such lock the process holds on it, and SQLite, believing it
/// still holds its lock, does not take it again. The check cannot know
/// whether the process has a connection on a file (another thread may be
/// opening one), nor whether the name of a log is a hard link to a database,
/// so it never closes a descriptor on a regular file that still has a name,
/// whichever role the name plays. It keeps a read-only descriptor per file
/// (device and inode), opened close-on-exec so a child does not inherit it,
/// and reads through it with positioned reads, which move no file offset.
/// That is up to two per pair; a name swapped between the look and the open,
/// or two threads opening the same file at once, can leave more than one on a
/// file, and each is kept.
///
/// A descriptor is closed by a later check, of any pair, once its file has no
/// name left (`nlink` 0, read from the handle), not at the moment the name
/// goes. Closing it releases any lock this process still holds on that
/// deleted file. That matters in one case: this process and another both
/// still have the deleted database open, and the other can then do what the
/// lock would have stopped (`PRAGMA journal_mode=DELETE` removes the log by
/// name without checking that the database has not moved, and the name may
/// by then be another database's log). The sweep stays all the same: without
/// it, a long-lived process that creates and removes databases would keep a
/// descriptor, and the file's blocks, for each until it exits.
///
/// While a descriptor is held its inode cannot be freed, so its inode number
/// is not reused for another file on that device and an entry found by device
/// and inode is that file.
#[cfg(unix)]
mod held {
    use std::fs::{File, Metadata};
    use std::io;
    use std::os::unix::fs::{FileExt as _, MetadataExt as _};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::{open_plain, Opened, Read};
    use crate::LockExt as _;

    struct Held {
        /// Device and inode as the handle reported them; `None` when the
        /// handle could not be asked (it is kept all the same).
        identity: Option<(u64, u64)>,
        file: Arc<File>,
    }

    /// The table, and only the table: every `open`, `fstat`, `pread` and
    /// `close` is made outside this lock, so a file system that hangs stalls
    /// the checks that touch it and not every store open in the process.
    static HELD: Mutex<Vec<Held>> = Mutex::new(Vec::new());

    fn table() -> std::sync::MutexGuard<'static, Vec<Held>> {
        HELD.lock_recovering("held descriptors")
    }

    /// Up to `limit` leading bytes of the file at `path`, whose name was just
    /// seen (`seen`, not followed) to be a regular file.
    pub(super) fn read(path: &Path, seen: &Metadata, limit: usize) -> io::Result<Read> {
        sweep();
        let identity = (seen.dev(), seen.ino());
        let found = table()
            .iter()
            .find(|h| h.identity == Some(identity))
            .map(|h| Arc::clone(&h.file));
        if let Some(file) = found {
            return read_head(&file, limit).map(Read::Bytes);
        }
        let file = match open_plain(path)? {
            Opened::Missing => return Ok(Read::Missing),
            Opened::Link => return Ok(Read::Link),
            Opened::File(file) => Arc::new(file),
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(e) => {
                // It may still have a name.
                keep(None, file);
                return Err(e);
            }
        };
        if !metadata.is_file() {
            // Not a database or a log; SQLite holds no lock on it. Closed
            // here.
            return Ok(Read::NotRegular(metadata.file_type()));
        }
        // Kept even when the table has this identity already (the name was
        // swapped between the look and the open, or another thread opened
        // the file meanwhile): it must not be closed either.
        keep(Some((metadata.dev(), metadata.ino())), Arc::clone(&file));
        read_head(&file, limit).map(Read::Bytes)
    }

    fn keep(identity: Option<(u64, u64)>, file: Arc<File>) {
        table().push(Held { identity, file });
    }

    /// Drop the entries whose file has no name left. The handles are taken
    /// under the lock and asked outside it; a descriptor closes when its
    /// last clone is dropped, after the lock is released. A handle that
    /// cannot be asked is kept: its file may still have a name.
    fn sweep() {
        let all: Vec<Arc<File>> = table().iter().map(|h| Arc::clone(&h.file)).collect();
        let gone: Vec<Arc<File>> = all
            .into_iter()
            .filter(|file| file.metadata().is_ok_and(|m| m.nlink() == 0))
            .collect();
        if !gone.is_empty() {
            table().retain(|h| !gone.iter().any(|g| Arc::ptr_eq(g, &h.file)));
        }
    }

    /// Up to `limit` bytes from the start of `file`, by positioned reads.
    fn read_head(file: &File, limit: usize) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0u8; limit];
        let mut filled = 0;
        while filled < limit {
            match file.read_at(&mut bytes[filled..], filled as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        bytes.truncate(filled);
        Ok(bytes)
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
    let seen = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => return Err(link()),
        Ok(m) if !m.is_file() => return Err(not_regular(m.file_type(), false)),
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e)),
    };
    // Both files are read through a descriptor that is kept open (see
    // `held`): the log's name may be a hard link to a database.
    #[cfg(unix)]
    let read = held::read(path, &seen, limit);
    #[cfg(not(unix))]
    let read = {
        drop(seen);
        read_closing(path, limit)
    };
    match read.map_err(unreadable)? {
        Read::Missing => Ok(None),
        Read::Link => Err(link()),
        Read::NotRegular(file_type) => Err(not_regular(file_type, true)),
        Read::Bytes(bytes) => Ok(Some(bytes)),
    }
}

/// Where locks belong to the handle and not to the process (not Unix; this
/// branch is not built here), closing a descriptor releases no lock SQLite
/// holds, so a file is opened, read and closed.
#[cfg(not(unix))]
fn read_closing(path: &Path, limit: usize) -> io::Result<Read> {
    use std::io::Read as _;
    let file = match open_plain(path)? {
        Opened::Missing => return Ok(Read::Missing),
        Opened::Link => return Ok(Read::Link),
        Opened::File(file) => file,
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(Read::NotRegular(metadata.file_type()));
    }
    let mut bytes = Vec::with_capacity(limit);
    file.take(limit as u64).read_to_end(&mut bytes)?;
    Ok(Read::Bytes(bytes))
}

/// Check the database at `path` and its write-ahead log before SQLite is
/// given the file, and return the name SQLite must be given.
///
/// SQLite before 3.54.0 can return uninitialized memory when a database is
/// opened with a log whose page size differs from the database's
/// (RUSTSEC-2026-0459). This reads the two page sizes itself (bytes 16 and 17
/// of the database, big-endian, 1 meaning 65536; bytes 8 to 11 of the log,
/// big-endian) and refuses a pair that disagrees. It judges nothing else about
/// the contents. The log is read first and the database second, so a store
/// another process is creating is not refused for the file it has not yet
/// written.
///
/// The file the check reads must be the file SQLite opens, so the check does
/// not leave the resolution of `path` to SQLite. It resolves `path` itself,
/// with the system, into a plain name ([`CheckedPath`]: absolute, no `.` or
/// `..`, no link in it, the last element included), checks that, and returns
/// it. **Open the returned name, and use it for anything else that touches the
/// file between the check and the open**, not `path`. A store keeps, returns
/// and prints the path it was given. A directory that cannot be resolved is
/// refused, so a store that creates its directory makes it before the check.
///
/// Giving SQLite the returned name is a second line of defence against a
/// difference in resolution nobody has found. It also lets a path open that
/// SQLite's own resolution refuses: one whose name, as SQLite builds it
/// element by element, passes its buffer of about 512 bytes while the
/// resolved name is short, or one that needs more than about 200 links in
/// all. Each open site has a test that opens a store through such a path.
///
/// - A path whose text begins with `file:` is refused: SQLite reads such a
///   name as a URI, not as a path, and Branchyard gives SQLite paths only.
///   Write it as an absolute path or one that begins with `./`. The exact
///   name `:memory:` is refused too (SQLite opens an in-memory database for
///   it and no file; `./:memory:` names a file), and so is the empty path,
///   which has no file name.
/// - Each directory is resolved with the system, so a directory that cannot be
///   used (missing, not searchable, a file, or below one) is refused, with
///   advice that follows the cause, naming the link and its target when the
///   directory came from a link; a path that ends in `..` or is a root has no
///   file name and is refused.
/// - A symbolic link at the last name is followed, up to 40 links in a row
///   (SQLite itself follows about 200), and the log checked is the one beside
///   the target, as SQLite opens it. A chain of more than 40 links, or a
///   loop, is refused. A link whose target does not exist is followed to the
///   name of the target.
/// - No log, an empty log or one shorter than its 32-byte header: nothing to
///   compare; `Ok`.
/// - A log that is a symbolic link (SQLite does not open a log through one)
///   or not a regular file (a FIFO, a socket, a directory, a device): refused,
///   naming what it is. The type of a name is read without opening it; a
///   regular file is opened without blocking and without following a link.
/// - A log of 32 bytes or more beside a database that is missing or empty:
///   refused, as is one beside a database that is not a regular file. A log
///   cannot be used without its database, and it may hold committed changes.
/// - A log of 32 bytes or more beside a database shorter than its 100-byte
///   header, or with another page size: refused.
///
/// A refusal of the pair names the database and the log where they are, and
/// the path given when that differs. A refusal of the path names the path
/// given, and the link and its target when one led there. Control characters
/// in any name shown are written as escapes.
///
/// On Unix the database and the log are each read through a read-only
/// descriptor the process keeps, because closing a descriptor on a file would
/// release the POSIX locks SQLite holds on it in this process, and the log's
/// name may be a hard link to a database: the check never closes a
/// descriptor on a regular file that still has a name. That is up to two per
/// pair (more than one on a file when its name was swapped during the check,
/// or two threads opened it at once). A descriptor is released by a later
/// check once its file has no name left, not when the name goes, so a
/// process that checks nothing more keeps it until it exits.
///
/// Not stopped: the check and the open are two steps. Whoever can change the
/// files, or a directory above them, between the two (replace a file or a
/// directory by another, plant a log) is not stopped, and the check cannot see
/// a log written after it. Only Unix is built and tested.
///
/// # Errors
///
/// [`PairError::Refused`] for a pair or a name SQLite cannot be given,
/// [`PairError::Unreadable`] when a file of the pair, or the name given for
/// it, cannot be looked at, opened or read; both name the path.
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
