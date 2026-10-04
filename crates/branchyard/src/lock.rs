//! One process per directory: an operating-system advisory lock on a file
//! in it, held for as long as the process uses the directory.
//!
//! The lock is `flock` on Linux and macOS (through [`std::fs::File::try_lock`])
//! and `LockFileEx` on Windows. The operating system releases it when the
//! process exits, however it exits, so a stopped process never leaves the
//! directory locked. It is advisory: it keeps out another process that asks
//! for it, nothing else. On a network file system it is only as good as the
//! file system's own locking.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::Error;

/// How long [`DirLock::acquire`] retries a lock that is held. A process
/// starting a child copies every open file, the lock file included, until
/// the child runs its program; for that instant the copy still holds a lock
/// its owner has just released, as when a server in this process stops and
/// another starts while any thread starts a child.
const WAIT: Duration = Duration::from_secs(2);

/// An exclusive lock on a directory, released when dropped or when the
/// process exits. See [`DirLock::acquire`].
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
    _file: File,
}

impl DirLock {
    /// Take the lock on `dir` for `what` (such as "a Branchyard server"),
    /// creating `dir` and its lock file, `lock`, if needed. Fails at once,
    /// without waiting, while another process (or another open of it in
    /// this process) holds it; the error names who holds it, as that
    /// holder wrote.
    ///
    /// A lock that stays held for `WAIT` is refused; one released within
    /// it, such as a copy a starting child held for an instant, is taken.
    pub fn acquire(dir: &Path, what: &str) -> Result<DirLock, Error> {
        Self::acquire_within(dir, what, WAIT)
    }

    #[allow(clippy::let_underscore_must_use)] // ratchet: branchyard
    fn acquire_within(dir: &Path, what: &str, wait: Duration) -> Result<DirLock, Error> {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::State(format!("create {}: {e}", dir.display())))?;
        let path = dir.join("lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::State(format!("open {}: {e}", path.display())))?;
        let deadline = Instant::now() + wait;
        let mut held = file.try_lock();
        while matches!(held, Err(TryLockError::WouldBlock)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            held = file.try_lock();
        }
        match held {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut holder = String::new();
                let _ = file.read_to_string(&mut holder);
                let holder = match holder.trim() {
                    "" => "another process".to_owned(),
                    held => held.to_owned(),
                };
                return Err(Error::State(format!(
                    "{} is already in use by {holder}; only one process may use it at a time. \
                     Stop that process, or choose another directory",
                    dir.display()
                )));
            }
            Err(TryLockError::Error(error)) => {
                return Err(Error::State(format!(
                    "could not lock {}: {error}",
                    path.display()
                )))
            }
        }
        // Who holds it, for the next process's error; the lock itself does
        // not depend on this.
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(file, "{what} (pid {})", std::process::id());
        let _ = file.sync_data();
        Ok(DirLock { path, _file: file })
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Whether a process holds `dir`'s lock now. Takes and drops the lock for
/// an instant when it is free, without writing to the file; a process
/// acquiring it at that instant retries within its [`WAIT`].
pub(crate) fn is_held(dir: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join("lock"))
    else {
        return false;
    };
    matches!(file.try_lock(), Err(TryLockError::WouldBlock))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_until_the_first_lets_go() {
        let temp = tempfile::Builder::new()
            .prefix("by-lock-")
            .tempdir()
            .unwrap();
        let dir = temp.path();
        let first = DirLock::acquire(dir, "the first test holder").unwrap();
        assert_eq!(first.path(), dir.join("lock"));
        // A separate open of the file is a separate holder, in this
        // process as in another.
        let refused = DirLock::acquire_within(dir, "the second", Duration::from_millis(100))
            .unwrap_err()
            .to_string();
        assert!(refused.contains("already in use"), "{refused}");
        assert!(
            refused.contains(&format!(
                "the first test holder (pid {})",
                std::process::id()
            )),
            "{refused}"
        );
        drop(first);
        let again = DirLock::acquire(dir, "the second").unwrap();
        drop(again);
    }

    #[test]
    fn a_lock_released_while_waiting_is_taken() {
        let temp = tempfile::Builder::new()
            .prefix("by-lock-wait-")
            .tempdir()
            .unwrap();
        let dir = temp.path();
        let first = DirLock::acquire(dir, "the first").unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(first);
        });
        let started = Instant::now();
        let second = DirLock::acquire(dir, "the second").unwrap();
        assert!(started.elapsed() >= Duration::from_millis(150));
        releaser.join().unwrap();
        drop(second);
    }
}
