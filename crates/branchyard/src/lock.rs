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

use crate::Error;

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
    pub fn acquire(dir: &Path, what: &str) -> Result<DirLock, Error> {
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
        match file.try_lock() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_until_the_first_lets_go() {
        let dir = std::env::temp_dir().join(format!("by-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = DirLock::acquire(&dir, "the first test holder").unwrap();
        assert_eq!(first.path(), dir.join("lock"));
        // A separate open of the file is a separate holder, in this
        // process as in another.
        let refused = DirLock::acquire(&dir, "the second")
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
        let again = DirLock::acquire(&dir, "the second").unwrap();
        drop(again);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
