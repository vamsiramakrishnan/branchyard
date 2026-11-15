//! The check that runs before SQLite is given a database path: it reads the
//! page sizes of the database and of its write-ahead log itself, and refuses
//! a pair that SQLite cannot have written.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::path::Path;

use branchyard_support::{check_wal_pair, PairError};
use branchyard_testkit::Scratch;

/// A database header of `len` bytes recording `field` as its page size.
fn database(len: usize, field: u16) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    if len >= 18 {
        bytes[16..18].copy_from_slice(&field.to_be_bytes());
    }
    bytes
}

/// A log of `len` bytes whose header records `size` as the page size.
fn log(len: usize, size: u32) -> Vec<u8> {
    let mut bytes = vec![0xAB; len];
    if len >= 12 {
        bytes[8..12].copy_from_slice(&size.to_be_bytes());
    }
    bytes
}

fn pair(db: &[u8], wal: Option<&[u8]>) -> (Scratch, std::path::PathBuf) {
    let dir = Scratch::new("pair");
    let path = dir.join("state.db");
    fs::write(&path, db).unwrap();
    if let Some(wal) = wal {
        fs::write(dir.join("state.db-wal"), wal).unwrap();
    }
    (dir, path)
}

fn refusal(path: &Path) -> String {
    match check_wal_pair(path) {
        Err(PairError::Refused(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn agreeing_page_sizes_are_accepted() {
    let (_dir, path) = pair(&database(4096, 4096), Some(&log(32 + 100, 4096)));
    check_wal_pair(&path).unwrap();
}

#[test]
fn another_page_size_is_refused_naming_the_file_and_both_sizes() {
    let (dir, path) = pair(&database(4096, 4096), Some(&log(32, 8192)));
    let message = refusal(&path);
    assert!(message.contains(&dir.join("state.db-wal").display().to_string()));
    assert!(
        message.contains("8192") && message.contains("4096"),
        "{message}"
    );
    assert!(message.contains("move both files away"), "{message}");
    assert!(!message.contains("delete"), "{message}");
}

#[test]
fn a_missing_empty_or_short_log_is_accepted() {
    let (_a, path) = pair(&database(4096, 4096), None);
    check_wal_pair(&path).unwrap();
    let (_b, path) = pair(&database(4096, 4096), Some(&[]));
    check_wal_pair(&path).unwrap();
    let (_c, path) = pair(&database(4096, 4096), Some(&log(31, 8192)));
    check_wal_pair(&path).unwrap();
}

#[test]
fn a_missing_database_is_accepted_without_a_log_and_refused_with_one() {
    let dir = Scratch::new("pair");
    check_wal_pair(&dir.join("state.db")).unwrap();
    fs::write(dir.join("state.db-wal"), log(32, 4096)).unwrap();
    let message = refusal(&dir.join("state.db"));
    assert!(message.contains("missing"), "{message}");
}

#[test]
fn a_database_shorter_than_its_header_is_refused_with_a_log() {
    let (_dir, path) = pair(&database(99, 4096), Some(&log(32, 4096)));
    let message = refusal(&path);
    assert!(message.contains("99"), "{message}");
    let (_dir, path) = pair(&database(99, 4096), Some(&log(31, 4096)));
    check_wal_pair(&path).unwrap();
    let (_dir, path) = pair(&[], Some(&[]));
    check_wal_pair(&path).unwrap();
}

#[test]
fn the_log_is_not_judged_by_its_magic_number() {
    // The helper fills every byte with 0xAB: no magic number, same page size.
    let (_dir, path) = pair(&database(100, 512), Some(&log(64, 512)));
    check_wal_pair(&path).unwrap();
}

#[test]
fn page_size_field_one_means_65536() {
    let (_dir, path) = pair(&database(100, 1), Some(&log(32, 65536)));
    check_wal_pair(&path).unwrap();
    let (_dir, path) = pair(&database(100, 1), Some(&log(32, 1)));
    let message = refusal(&path);
    assert!(message.contains("65536"), "{message}");
}

#[test]
fn the_refusal_leaves_the_files_alone_and_makes_no_shm() {
    let (dir, path) = pair(&database(4096, 4096), Some(&log(40, 8192)));
    let (db, wal) = (
        fs::read(&path).unwrap(),
        fs::read(dir.join("state.db-wal")).unwrap(),
    );
    refusal(&path);
    assert_eq!(fs::read(&path).unwrap(), db);
    assert_eq!(fs::read(dir.join("state.db-wal")).unwrap(), wal);
    assert!(!dir.join("state.db-shm").exists());
}

#[cfg(unix)]
#[test]
fn a_symlinked_database_is_checked_against_the_log_beside_its_target() {
    let dir = Scratch::new("pair");
    let real = dir.join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("state.db"), database(4096, 4096)).unwrap();
    fs::write(real.join("state.db-wal"), log(32, 8192)).unwrap();
    let link = dir.join("state.db");
    std::os::unix::fs::symlink(real.join("state.db"), &link).unwrap();
    // SQLite resolves the link and opens the log beside the target.
    refusal(&link);
    // A log beside the link alone is not the one SQLite would open.
    fs::write(dir.join("state.db-wal"), log(32, 512)).unwrap();
    fs::write(real.join("state.db-wal"), log(32, 4096)).unwrap();
    check_wal_pair(&link).unwrap();
}
