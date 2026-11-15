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

/// Run the check on a thread and wait for it: a check that blocks fails the
/// test after the timeout instead of hanging it.
fn within_a_while(path: &Path) -> Result<(), PairError> {
    let path = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tx.send(check_wal_pair(&path)).ok();
    });
    branchyard_testkit::wait::until_for(
        "the check returns",
        std::time::Duration::from_secs(10),
        || rx.try_recv().ok(),
    )
}

fn refusal_within_a_while(path: &Path) -> String {
    match within_a_while(path) {
        Err(PairError::Refused(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn names(dir: &Scratch) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

#[cfg(unix)]
fn fifo(path: &Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
}

/// `count` links in a row ending at `target`; returns the first link.
#[cfg(unix)]
fn chain(dir: &Scratch, target: &Path, count: usize) -> std::path::PathBuf {
    let links = dir.join("links");
    fs::create_dir_all(&links).unwrap();
    let mut previous = target.to_path_buf();
    for i in 0..count {
        let link = links.join(format!("l{i}"));
        std::os::unix::fs::symlink(&previous, &link).unwrap();
        previous = link;
    }
    previous
}

#[cfg(unix)]
fn target_with_crafted_log() -> (Scratch, std::path::PathBuf) {
    let dir = Scratch::new("pair");
    let target = dir.join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("state.db"), database(4096, 4096)).unwrap();
    fs::write(target.join("state.db-wal"), log(32, 512)).unwrap();
    (dir, target.join("state.db"))
}

#[cfg(unix)]
#[test]
fn a_chain_of_links_beyond_the_bound_is_refused_not_followed_on() {
    let (dir, real) = target_with_crafted_log();
    // 40 links are followed: the crafted log beside the target is found.
    let message = refusal(&chain(&dir, &real, 40));
    assert!(message.contains("512"), "{message}");
    // 41 are not: refuse rather than look beside the 41st name.
    let links = Scratch::new("pair");
    let first = chain(&links, &real, 41);
    let message = refusal(&first);
    assert!(
        message.contains(&first.display().to_string()) && message.contains("longer than"),
        "{message}"
    );
    assert!(message.contains("symbolic links"), "{message}");
}

#[cfg(unix)]
#[test]
fn a_short_chain_of_links_still_opens() {
    let dir = Scratch::new("pair");
    let real = dir.join("state.db");
    fs::write(&real, database(4096, 4096)).unwrap();
    fs::write(dir.join("state.db-wal"), log(32, 4096)).unwrap();
    check_wal_pair(&chain(&dir, &real, 3)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_loop_of_links_is_refused_with_the_chain_message() {
    let dir = Scratch::new("pair");
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::os::unix::fs::symlink(&b, &a).unwrap();
    std::os::unix::fs::symlink(&a, &b).unwrap();
    let message = refusal(&a);
    assert!(
        message.contains("longer than") && message.contains("loop"),
        "{message}"
    );
}

#[test]
fn a_name_that_sqlite_reads_as_a_uri_is_refused() {
    let dir = Scratch::new("pair");
    for name in ["file:reg.db", "file:///data/x.db?mode=ro"] {
        let message = refusal(Path::new(name));
        assert!(message.contains(name), "{message}");
        assert!(
            message.contains("URI") && message.contains("another file"),
            "{message}"
        );
        assert!(message.contains("./"), "{message}");
    }
    // Only the five bytes `file:` at the start; `./file:x` and a directory
    // named so are plain names.
    check_wal_pair(&dir.join("file:reg.db")).unwrap();
    check_wal_pair(Path::new("./file:reg.db")).unwrap();
    check_wal_pair(Path::new("File:reg.db")).unwrap();
}

#[cfg(unix)]
#[test]
fn a_fifo_as_the_log_is_refused_without_blocking() {
    let (dir, path) = pair(&database(4096, 4096), None);
    fifo(&dir.join("state.db-wal"));
    let before = names(&dir);
    let message = refusal_within_a_while(&path);
    assert!(message.contains("state.db-wal"), "{message}");
    assert!(
        message.contains("FIFO") && message.contains("not a regular file"),
        "{message}"
    );
    assert_eq!(names(&dir), before);
}

#[cfg(unix)]
#[test]
fn a_fifo_as_the_database_beside_a_real_log_is_refused_without_blocking() {
    let dir = Scratch::new("pair");
    fifo(&dir.join("state.db"));
    fs::write(dir.join("state.db-wal"), log(32, 4096)).unwrap();
    let before = names(&dir);
    let message = refusal_within_a_while(&dir.join("state.db"));
    assert!(message.contains("FIFO"), "{message}");
    assert!(message.contains("not a regular file"), "{message}");
    assert_eq!(names(&dir), before);
}

#[test]
fn a_directory_as_the_log_or_the_database_is_refused() {
    let (dir, path) = pair(&database(4096, 4096), None);
    fs::create_dir(dir.join("state.db-wal")).unwrap();
    let message = refusal_within_a_while(&path);
    assert!(message.contains("a directory"), "{message}");
    assert!(message.contains("state.db-wal"), "{message}");

    let dir = Scratch::new("pair");
    fs::create_dir(dir.join("state.db")).unwrap();
    fs::write(dir.join("state.db-wal"), log(32, 4096)).unwrap();
    let before = names(&dir);
    let message = refusal_within_a_while(&dir.join("state.db"));
    assert!(message.contains("a directory"), "{message}");
    assert!(message.contains("not a regular file"), "{message}");
    assert_eq!(names(&dir), before);
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_as_the_log_is_refused_as_sqlite_does_not_open_it() {
    let (dir, path) = pair(&database(4096, 4096), None);
    let other = dir.join("other-wal");
    fs::write(&other, log(32, 4096)).unwrap();
    std::os::unix::fs::symlink(&other, dir.join("state.db-wal")).unwrap();
    let message = refusal_within_a_while(&path);
    assert!(message.contains("state.db-wal"), "{message}");
    assert!(
        message.contains("symbolic link") && message.contains("does not open a log through one"),
        "{message}"
    );
    // Even one that points at a FIFO is not followed.
    let dir = Scratch::new("pair");
    let path = dir.join("state.db");
    fs::write(&path, database(4096, 4096)).unwrap();
    fifo(&dir.join("pipe"));
    std::os::unix::fs::symlink(dir.join("pipe"), dir.join("state.db-wal")).unwrap();
    assert!(refusal_within_a_while(&path).contains("symbolic link"));
}

#[test]
fn a_log_beside_a_missing_or_empty_database_says_the_log_has_no_database() {
    let dir = Scratch::new("pair");
    fs::write(dir.join("state.db-wal"), log(64, 4096)).unwrap();
    for empty in [false, true] {
        if empty {
            fs::write(dir.join("state.db"), []).unwrap();
        }
        let message = refusal(&dir.join("state.db"));
        let state = if empty { "is empty" } else { "is missing" };
        assert!(message.contains(state), "{message}");
        assert!(message.contains("has no database"), "{message}");
        assert!(
            message.contains("cannot be used without its database"),
            "{message}"
        );
        assert!(message.contains("move the log away"), "{message}");
        assert!(!message.contains("this pair was not written"), "{message}");
        assert!(!message.contains("delete"), "{message}");
        assert!(!message.contains("move both"), "{message}");
    }
}

#[test]
fn a_short_database_beside_a_log_names_both_files() {
    let (dir, path) = pair(&database(99, 4096), Some(&log(32, 4096)));
    let message = refusal(&path);
    assert!(
        message.contains("shorter than its 100-byte header"),
        "{message}"
    );
    assert!(message.contains("move both files away"), "{message}");
    assert!(
        message.contains(&dir.join("state.db-wal").display().to_string()),
        "{message}"
    );
}

#[cfg(unix)]
#[test]
fn a_path_below_a_regular_file_is_unreadable_and_the_error_names_it() {
    let dir = Scratch::new("pair");
    fs::write(dir.join("plain"), b"x").unwrap();
    let path = dir.join("plain").join("state.db");
    let error = check_wal_pair(&path).unwrap_err();
    let PairError::Unreadable { path: named, .. } = &error else {
        panic!("expected unreadable, got {error:?}");
    };
    assert!(named.ends_with("state.db-wal"), "{named:?}");
    assert!(error.to_string().contains("state.db-wal"), "{error}");
    // As an io::Error, which the registry and the server print, too.
    let io_error = std::io::Error::from(error);
    let text = io_error.to_string();
    assert!(
        text.contains("state.db-wal") && text.contains("could not be read"),
        "{text}"
    );
    assert_eq!(io_error.kind(), std::io::ErrorKind::NotADirectory);
}

#[cfg(unix)]
#[test]
fn a_refusal_names_where_the_files_are_and_the_path_it_was_given() {
    let dir = Scratch::new("pair");
    let real = dir.join("real");
    fs::create_dir_all(real.join("data")).unwrap();
    fs::write(real.join("state.db"), database(4096, 4096)).unwrap();
    fs::write(real.join("state.db-wal"), log(32, 512)).unwrap();
    // `repo/.by` is a link to `real/data`; `state.db` in it is `../state.db`.
    fs::create_dir(dir.join("repo")).unwrap();
    std::os::unix::fs::symlink(real.join("data"), dir.join("repo/.by")).unwrap();
    std::os::unix::fs::symlink("../state.db", real.join("data/state.db")).unwrap();
    let given = dir.join("repo/.by/state.db");
    let message = refusal(&given);
    let real_dir = real.canonicalize().unwrap();
    assert!(
        message.contains(&real_dir.join("state.db").display().to_string()),
        "{message}"
    );
    assert!(
        message.contains(&real_dir.join("state.db-wal").display().to_string()),
        "{message}"
    );
    assert!(
        message.contains(&format!("given as {}", given.display())),
        "{message}"
    );
    assert!(!message.contains(".by/../"), "{message}");
}

#[test]
fn the_given_path_is_not_repeated_when_it_is_where_the_files_are() {
    let (_dir, path) = pair(&database(4096, 4096), Some(&log(32, 8192)));
    assert!(!refusal(&path).contains("given as"));
}
