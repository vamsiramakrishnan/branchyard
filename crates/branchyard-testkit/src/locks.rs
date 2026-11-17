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
/// The file is found by this process's pid and its inode number, not by its
/// device: `/proc/locks` prints the device of the inode's superblock, which
/// is not always the one `stat` reports (an overlay file system whose layers
/// are on different file systems, a btrfs subvolume), and a match by device
/// would find no lock there although one is held. The cost: a file with the
/// same inode number on another file system, locked by this process, would
/// be counted too; no test here locks one.
///
/// The kernel prints `/proc/locks` at most a page per read. A table that
/// comes in one read is a snapshot, and the answer is exactly what it shows.
/// A longer one is not: a lock taken or released anywhere between two reads
/// shifts the lines after it, so one pass can skip a line or print it twice
/// (seen with other threads taking locks: a held lock went missing). Such a
/// table is read several times (`PASSES`) and the answer is every lock any pass
/// showed, each once. A lock this process holds throughout is then missed
/// only if every pass skips it, and a lock it does not hold is in none.
///
/// # Errors
///
/// When `path` cannot be looked at or `/proc/locks` cannot be read.
pub fn posix_locks_on(path: &Path) -> io::Result<Vec<String>> {
    let meta = std::fs::metadata(path)?;
    let (pid, ino) = (std::process::id(), meta.ino());
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..PASSES {
        let (table, reads) = lock_table_once()?;
        let mut held: Vec<String> = Vec::new();
        for lock in posix_locks_in(&table, pid, ino) {
            if !held.contains(&lock) {
                held.push(lock);
            }
        }
        if reads <= 1 {
            return Ok(held);
        }
        for lock in held {
            if !seen.contains(&lock) {
                seen.push(lock);
            }
        }
    }
    Ok(seen)
}

/// How many times a table that does not come in one read is read.
const PASSES: usize = 8;

/// One pass over `/proc/locks`, and the number of reads it took.
fn lock_table_once() -> io::Result<(String, usize)> {
    use std::io::Read as _;
    let mut file = std::fs::File::open("/proc/locks")?;
    let mut chunk = vec![0u8; 1 << 16];
    let mut text = Vec::new();
    let mut reads = 0;
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                reads += 1;
                text.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok((String::from_utf8_lossy(&text).into_owned(), reads))
}

/// The POSIX locks `pid` holds on inode number `ino`, on any device, read
/// from `table`, the text of `/proc/locks` (see [`posix_locks_on`] for why
/// the device is not compared).
pub fn posix_locks_in(table: &str, pid: u32, ino: u64) -> Vec<String> {
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
        if file.rsplit(':').next().and_then(|i| i.parse::<u64>().ok()) == Some(ino) {
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

    #[test]
    fn only_posix_locks_of_this_pid_on_this_inode_are_listed() {
        let table = "\
1: POSIX  ADVISORY  READ  42 fd:01:7 1073741826 1073742335
2: POSIX  ADVISORY  WRITE 43 fd:01:7 0 EOF
3: FLOCK  ADVISORY  WRITE 42 fd:01:7 0 EOF
4: OFDLCK ADVISORY  READ  -1 fd:01:7 0 EOF
5: POSIX  ADVISORY  READ  42 fd:01:8 0 EOF
5: -> POSIX  ADVISORY  WRITE 42 fd:01:7 0 EOF
6: POSIX  ADVISORY  WRITE 42 fd:01:7 0 EOF
7: POSIX  ADVISORY  READ  42 00:29:77 0 EOF
";
        assert_eq!(
            posix_locks_in(table, 42, 7),
            vec!["READ 1073741826-1073742335", "WRITE 0-EOF"]
        );
        // The device is not compared: an overlay file `stat` reports on
        // 00:2a is printed on its superblock's device, 00:29.
        assert_eq!(posix_locks_in(table, 42, 77), vec!["READ 0-EOF"]);
        // An inode number is matched whole, not as a suffix.
        assert_eq!(posix_locks_in(table, 42, 777), Vec::<String>::new());
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
