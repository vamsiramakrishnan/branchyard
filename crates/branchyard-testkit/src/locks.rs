//! What this process holds on a file, as Linux reports it: the POSIX record
//! locks (`/proc/locks`) and the open descriptors (`/proc/self/fd`).
//!
//! A process cannot ask for a lock it holds itself (`F_GETLK` reports only
//! other processes' locks), so a test that must show its own process still
//! holds SQLite's lock on a database reads `/proc/locks`.

use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

/// The POSIX record locks this process holds on the file at `path`, one
/// entry per line of `/proc/locks`, as `READ 1073741826-1073742335` (the
/// kind and the byte range; `EOF` for a lock to the end of the file).
/// Locks of other kinds (`FLOCK`, `OFDLCK`) and waiters are not included.
///
/// # Errors
///
/// When `path` cannot be looked at or `/proc/locks` cannot be read.
pub fn posix_locks_on(path: &Path) -> io::Result<Vec<String>> {
    let meta = std::fs::metadata(path)?;
    let table = std::fs::read_to_string("/proc/locks")?;
    Ok(posix_locks_in(
        &table,
        std::process::id(),
        meta.dev(),
        meta.ino(),
    ))
}

/// The POSIX locks `pid` holds on device `dev` (as `st_dev` encodes it) and
/// inode `ino`, read from `table`, the text of `/proc/locks`.
pub fn posix_locks_in(table: &str, pid: u32, dev: u64, ino: u64) -> Vec<String> {
    // `/proc/locks` prints the device as `major:minor` in hexadecimal; this
    // is how glibc's `major` and `minor` decode `st_dev`.
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    let mut held = Vec::new();
    for line in table.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // `N: POSIX ADVISORY READ pid maj:min:ino start end`; a waiter has a
        // `->` after the number and is skipped.
        let [_, kind, _, mode, owner, file, start, end] = fields[..] else {
            continue;
        };
        if kind != "POSIX" || owner.parse::<u32>().ok() != Some(pid) {
            continue;
        }
        let mut parts = file.split(':');
        let (Some(maj), Some(min), Some(inode), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let same = u64::from_str_radix(maj, 16).ok() == Some(major)
            && u64::from_str_radix(min, 16).ok() == Some(minor)
            && inode.parse::<u64>().ok() == Some(ino);
        if same {
            held.push(format!("{mode} {start}-{end}"));
        }
    }
    held
}

/// How many of this process's open descriptors refer to the file at `path`
/// (the same device and inode), read from `/proc/self/fd`.
///
/// # Errors
///
/// When `path` or `/proc/self/fd` cannot be read.
pub fn descriptors_on(path: &Path) -> io::Result<usize> {
    let meta = std::fs::metadata(path)?;
    descriptors_of(meta.dev(), meta.ino())
}

/// How many of this process's open descriptors refer to device `dev` and
/// inode `ino` (as `metadata` reports them): for a file that no longer has a
/// name.
///
/// # Errors
///
/// When `/proc/self/fd` cannot be read.
pub fn descriptors_of(dev: u64, ino: u64) -> io::Result<usize> {
    let mut count = 0;
    for entry in std::fs::read_dir("/proc/self/fd")? {
        let entry = entry?;
        // The descriptor of the directory listing itself is gone by the time
        // it is looked at; a descriptor closed meanwhile is not counted.
        if let Ok(target) = std::fs::metadata(entry.path()) {
            if target.dev() == dev && target.ino() == ino {
                count += 1;
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // tests: a panic is the failure report
    use super::*;

    /// `st_dev` for `major:minor` as Linux encodes it.
    fn dev(major: u64, minor: u64) -> u64 {
        (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12) | ((major & !0xfff) << 32)
    }

    #[test]
    fn only_posix_locks_of_this_pid_on_this_file_are_listed() {
        let table = "\
1: POSIX  ADVISORY  READ  42 fd:01:7 1073741826 1073742335
2: POSIX  ADVISORY  WRITE 43 fd:01:7 0 EOF
3: FLOCK  ADVISORY  WRITE 42 fd:01:7 0 EOF
4: OFDLCK ADVISORY  READ  -1 fd:01:7 0 EOF
5: POSIX  ADVISORY  READ  42 fd:01:8 0 EOF
6: POSIX  ADVISORY  READ  42 fd:02:7 0 EOF
6: -> POSIX  ADVISORY  WRITE 42 fd:01:7 0 EOF
7: POSIX  ADVISORY  WRITE 42 fd:01:7 0 EOF
";
        assert_eq!(
            posix_locks_in(table, 42, dev(0xfd, 1), 7),
            vec!["READ 1073741826-1073742335", "WRITE 0-EOF"]
        );
        // A minor number above 255 is split across `st_dev`.
        let wide = "1: POSIX  ADVISORY  READ  42 103:1a3:9 0 EOF\n";
        assert_eq!(
            posix_locks_in(wide, 42, dev(0x103, 0x1a3), 9),
            vec!["READ 0-EOF"]
        );
    }

    #[test]
    fn a_file_with_no_lock_has_none_and_a_flock_is_not_a_posix_lock() {
        let dir = crate::Scratch::new("locks");
        let path = dir.join("f");
        std::fs::write(&path, b"x").unwrap();
        assert_eq!(posix_locks_on(&path).unwrap(), Vec::<String>::new());
        let file = std::fs::File::open(&path).unwrap();
        file.lock_shared().unwrap();
        assert_eq!(posix_locks_on(&path).unwrap(), Vec::<String>::new());
        assert_eq!(descriptors_on(&path).unwrap(), 1);
        drop(file);
        assert_eq!(descriptors_on(&path).unwrap(), 0);
    }
}
