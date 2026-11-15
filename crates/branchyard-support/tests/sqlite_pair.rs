//! The check that runs before SQLite is given a database path: it reads the
//! page sizes of the database and of its write-ahead log itself, and refuses
//! a pair that SQLite cannot have written.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::path::{Path, PathBuf};

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

/// The plain name the check returned for a pair it accepts.
fn accepted(path: &Path) -> std::path::PathBuf {
    check_wal_pair(path)
        .unwrap_or_else(|e| panic!("expected the pair to be accepted: {e}"))
        .into_path_buf()
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
    accepted(&path);
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
    accepted(&path);
    let (_b, path) = pair(&database(4096, 4096), Some(&[]));
    accepted(&path);
    let (_c, path) = pair(&database(4096, 4096), Some(&log(31, 8192)));
    accepted(&path);
}

#[test]
fn a_missing_database_is_accepted_without_a_log_and_refused_with_one() {
    let dir = Scratch::new("pair");
    accepted(&dir.join("state.db"));
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
    accepted(&path);
    let (_dir, path) = pair(&[], Some(&[]));
    accepted(&path);
}

#[test]
fn the_log_is_not_judged_by_its_magic_number() {
    // The helper fills every byte with 0xAB: no magic number, same page size.
    let (_dir, path) = pair(&database(100, 512), Some(&log(64, 512)));
    accepted(&path);
}

#[test]
fn page_size_field_one_means_65536() {
    let (_dir, path) = pair(&database(100, 1), Some(&log(32, 65536)));
    accepted(&path);
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
    accepted(&link);
}

/// Run the check on a thread and wait for it: a check that blocks fails the
/// test after the timeout instead of hanging it.
fn within_a_while(path: &Path) -> Result<branchyard_support::CheckedPath, PairError> {
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
    accepted(&chain(&dir, &real, 3));
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
        // The reason is the true one: a URI, not a path, and only paths are
        // given to SQLite; it does not claim another file would be opened.
        assert!(
            message.contains("URI")
                && message.contains("not as a path")
                && message.contains("gives SQLite paths only"),
            "{message}"
        );
        assert!(!message.contains("another file"), "{message}");
        assert!(message.contains("./"), "{message}");
    }
    // Only the five bytes `file:` at the start; `./file:x` and a directory
    // named so are plain names.
    accepted(&dir.join("file:reg.db"));
    accepted(Path::new("./file:reg.db"));
    accepted(Path::new("File:reg.db"));
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
fn a_path_below_a_regular_file_is_refused_with_advice_and_names_the_path() {
    let dir = Scratch::new("pair");
    fs::write(dir.join("plain"), b"x").unwrap();
    // Given directly: the directory is a file.
    let path = dir.join("plain").join("state.db");
    let message = refusal(&path);
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert!(message.contains("is a file"), "{message}");
    assert!(message.contains("inside a directory"), "{message}");
    assert!(!message.contains("-wal"), "{message}");
    // Through a link, which is named with its target.
    let link = dir.join("l.db");
    std::os::unix::fs::symlink("plain/state.db", &link).unwrap();
    let message = refusal(&link);
    assert!(
        message.contains("is a symbolic link to plain/state.db"),
        "{message}"
    );
    assert!(message.contains("is a file"), "{message}");
    assert!(!message.contains("-wal"), "{message}");
    // The refusal does not depend on permissions, and is an io::Error of the
    // same kind for the stores that print it.
    let error = check_wal_pair(&path).unwrap_err();
    assert_eq!(
        std::io::Error::from(error).kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn an_unreadable_file_of_the_pair_keeps_its_kind_and_names_its_path_through_io_error() {
    let error = PairError::Unreadable {
        path: PathBuf::from("/x/state.db-wal"),
        source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    };
    let io_error = std::io::Error::from(error);
    let text = io_error.to_string();
    assert!(
        text.contains("/x/state.db-wal") && text.contains("could not be read"),
        "{text}"
    );
    assert_eq!(io_error.kind(), std::io::ErrorKind::PermissionDenied);
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

// ---- The check and SQLite must read the same files ----

/// `real.db` in `dir` with a log beside it that records another page size.
#[cfg(unix)]
fn crafted_in(dir: &Path) {
    fs::write(dir.join("real.db"), database(4096, 4096)).unwrap();
    fs::write(dir.join("real.db-wal"), log(32, 512)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_link_through_a_missing_directory_and_dotdot_is_refused() {
    // SQLite drops `nope/..` as text and opens `real.db`; the system cannot
    // get through the missing `nope`. The two must not be left to differ.
    let dir = Scratch::new("pair");
    crafted_in(dir.path());
    let link = dir.join("state.db");
    std::os::unix::fs::symlink("nope/../real.db", &link).unwrap();
    let message = refusal(&link);
    assert!(message.contains("nope"), "{message}");
    assert!(message.contains("cannot be resolved"), "{message}");
}

#[cfg(unix)]
#[test]
fn a_linked_directory_through_a_missing_one_is_refused() {
    let dir = Scratch::new("pair");
    let real = dir.join("realdir");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("state.db"), database(4096, 4096)).unwrap();
    fs::write(real.join("state.db-wal"), log(32, 512)).unwrap();
    std::os::unix::fs::symlink("nope/../realdir", dir.join(".branchyard")).unwrap();
    let message = refusal(&dir.join(".branchyard/state.db"));
    assert!(message.contains("cannot be resolved"), "{message}");
}

#[cfg(unix)]
#[test]
fn a_given_path_with_dotdot_after_a_missing_element_is_refused() {
    let dir = Scratch::new("pair");
    crafted_in(dir.path());
    let message = refusal(&dir.join("nope/../real.db"));
    assert!(message.contains("cannot be resolved"), "{message}");
}

#[cfg(unix)]
#[test]
fn a_dotdot_through_real_directories_is_resolved_and_the_log_beside_the_file_is_found() {
    let dir = Scratch::new("pair");
    crafted_in(dir.path());
    fs::create_dir(dir.join("sub")).unwrap();
    for given in [
        format!("{}/sub/../real.db", dir.path().display()),
        format!("{}/./real.db", dir.path().display()),
        format!("{}//real.db", dir.path().display()),
        format!("{}/real.db/", dir.path().display()),
        format!("{}/real.db/.", dir.path().display()),
    ] {
        let message = refusal(Path::new(&given));
        assert!(message.contains("512"), "{given}: {message}");
    }
}

#[cfg(unix)]
#[test]
fn a_path_without_a_file_name_is_refused() {
    let dir = Scratch::new("pair");
    for given in [dir.join("sub/.."), "/".into()] {
        let message = refusal(&given);
        assert!(message.contains("no file name"), "{message}");
    }
}

#[test]
fn the_empty_path_is_refused_saying_it_is_empty() {
    let message = refusal(Path::new(""));
    assert!(message.contains("the path is empty"), "{message}");
    assert!(!message.contains("`..`"), "{message}");
}

#[test]
fn the_name_memory_is_refused_as_sqlite_opens_no_file_for_it() {
    let message = refusal(Path::new(":memory:"));
    assert!(
        message.contains("in-memory database") && message.contains("not a file"),
        "{message}"
    );
    assert!(message.contains("./:memory:"), "{message}");
    // Only the exact name: a name of a file that merely contains or starts
    // with it, and the explicit relative name, are ordinary paths.
    let dir = Scratch::new("pair");
    accepted(&dir.join(":memory:"));
    accepted(Path::new("./:memory:"));
    accepted(Path::new(":memory:x"));
    accepted(Path::new("x:memory:"));
}

#[cfg(unix)]
#[test]
fn the_advice_for_a_directory_that_cannot_be_resolved_follows_the_cause() {
    let dir = Scratch::new("pair");
    // Not found: create it, or point elsewhere.
    let message = refusal(&dir.join("nope/state.db"));
    assert!(message.contains("create the directory"), "{message}");
    // A part of the path is a file: not the advice to create anything.
    fs::write(dir.join("afile"), b"x").unwrap();
    let message = refusal(&dir.join("afile/sub/state.db"));
    assert!(message.contains("cannot be resolved"), "{message}");
    assert!(message.contains("is a file, not a directory"), "{message}");
    assert!(!message.contains("create the directory"), "{message}");
    // Not searchable (a superuser searches anything, so skip it for one).
    if !rustix::process::geteuid().is_root() {
        use std::os::unix::fs::PermissionsExt as _;
        let locked = dir.join("locked");
        fs::create_dir_all(locked.join("sub")).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let message = refusal(&locked.join("sub/state.db"));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(message.contains("cannot be searched"), "{message}");
        assert!(!message.contains("create the directory"), "{message}");
    }
}

#[cfg(unix)]
#[test]
fn a_directory_that_came_from_a_link_is_named_with_the_link_and_its_target() {
    let dir = Scratch::new("pair");
    let link = dir.join("state.db");
    std::os::unix::fs::symlink("vault/missing/real.db", &link).unwrap();
    let message = refusal(&link);
    let target_dir = dir.join("vault/missing");
    assert!(
        message.contains(&format!(
            "{} is a symbolic link to vault/missing/real.db",
            link.display()
        )),
        "{message}"
    );
    assert!(
        message.contains(&format!(
            "the directory {} of that target",
            target_dir.display()
        )),
        "{message}"
    );
    assert!(message.contains("cannot be resolved"), "{message}");
    // A directory the person typed is named as theirs, not as a link's.
    let message = refusal(&dir.join("nope/state.db"));
    assert!(message.contains("its directory"), "{message}");
    assert!(!message.contains("symbolic link"), "{message}");
}

/// The state of a thread (`S` sleeping, `R` running ...), from `/proc`.
#[cfg(target_os = "linux")]
fn thread_state(id: &Path) -> Option<char> {
    let stat = fs::read_to_string(Path::new("/proc").join(id).join("stat")).ok()?;
    stat.rsplit(')').next()?.trim_start().chars().next()
}

#[cfg(target_os = "linux")]
#[test]
fn a_fifo_with_a_writer_waiting_is_refused_and_the_writer_stays_blocked() {
    use std::sync::mpsc;
    use std::time::Duration;
    let (dir, path) = pair(&database(4096, 4096), None);
    let pipe = dir.join("state.db-wal");
    fifo(&pipe);
    let (id_tx, id_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let writer_pipe = pipe.clone();
    let writer = std::thread::spawn(move || {
        id_tx.send(fs::read_link("/proc/thread-self").unwrap()).ok();
        // Blocks until a reader opens the pipe.
        let opened = fs::OpenOptions::new().write(true).open(&writer_pipe);
        done_tx.send(opened.is_ok()).ok();
    });
    let id = branchyard_testkit::wait::until_for(
        "the writer reports its thread",
        Duration::from_secs(10),
        || id_rx.try_recv().ok(),
    );
    branchyard_testkit::wait::until_for(
        "the writer sleeps in its open",
        Duration::from_secs(10),
        || (thread_state(&id) == Some('S')).then_some(()),
    );
    let outcome = within_a_while(&path);
    let still_asleep = thread_state(&id) == Some('S');
    let released_early = done_rx.try_recv().is_ok();
    // Let the writer go, whatever the check did, so the thread ends.
    if !released_early {
        let reader = rustix::fs::open(
            &pipe,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        branchyard_testkit::wait::until_for(
            "the writer is released",
            Duration::from_secs(10),
            || done_rx.try_recv().ok(),
        );
        drop(reader);
    }
    writer.join().unwrap();
    match outcome {
        Err(PairError::Refused(message)) => assert!(message.contains("FIFO"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(
        !released_early,
        "the check opened the pipe and freed its writer"
    );
    assert!(still_asleep, "the writer was woken by the check");
}

#[cfg(unix)]
fn socket(path: &Path) -> std::os::unix::net::UnixListener {
    std::os::unix::net::UnixListener::bind(path).unwrap()
}

#[cfg(unix)]
#[test]
fn a_socket_as_the_log_or_the_database_is_refused_as_a_socket() {
    let (dir, path) = pair(&database(4096, 4096), None);
    let _log = socket(&dir.join("state.db-wal"));
    let message = refusal_within_a_while(&path);
    assert!(
        message.contains("a socket") && message.contains("state.db-wal"),
        "{message}"
    );
    assert!(message.contains("not a regular file"), "{message}");

    let dir = Scratch::new("pair");
    let _db = socket(&dir.join("state.db"));
    fs::write(dir.join("state.db-wal"), log(32, 4096)).unwrap();
    let message = refusal_within_a_while(&dir.join("state.db"));
    assert!(message.contains("a socket"), "{message}");
}

#[cfg(unix)]
#[test]
fn control_characters_in_a_name_are_escaped_in_a_refusal() {
    let dir = Scratch::new("pair");
    let odd = dir.join("a\nby: all good\u{1b}[2K\tb");
    fs::create_dir(&odd).unwrap();
    fs::write(odd.join("state.db"), database(4096, 4096)).unwrap();
    fs::write(odd.join("state.db-wal"), log(32, 512)).unwrap();
    let message = refusal(&odd.join("state.db"));
    assert_eq!(message.lines().count(), 1, "{message:?}");
    assert!(!message.chars().any(char::is_control), "{message:?}");
    assert!(
        message.contains("a\\nby: all good\\u{1b}[2K\\tb"),
        "{message:?}"
    );
    // A refusal of a path below a file shows the name escaped too, as an
    // error and as io::Error.
    fs::write(dir.join("plain\nfile"), b"x").unwrap();
    let error = check_wal_pair(&dir.join("plain\nfile/state.db")).unwrap_err();
    assert!(!error.to_string().contains('\n'), "{error:?}");
    let text = std::io::Error::from(error).to_string();
    assert!(
        !text.contains('\n') && text.contains("plain\\nfile"),
        "{text:?}"
    );
    // A link's name is escaped in the chain refusal.
    let looped = dir.join("l\noop");
    std::os::unix::fs::symlink(&looped, &looped).unwrap();
    assert!(!refusal(&looped).contains('\n'));
}

// ---- The returned name ----

/// Whether `name` is plain: absolute, no `.`, `..` or empty element, no link.
#[cfg(unix)]
fn assert_plain(name: &Path) {
    assert!(name.is_absolute(), "{name:?}");
    let text = name.to_str().unwrap();
    assert!(!text.contains("//") && !text.ends_with('/'), "{text}");
    let mut prefix = std::path::PathBuf::new();
    for part in name.components() {
        prefix.push(part);
        assert!(
            !matches!(
                part,
                std::path::Component::CurDir | std::path::Component::ParentDir
            ),
            "{text}"
        );
        if let Ok(meta) = fs::symlink_metadata(&prefix) {
            assert!(!meta.file_type().is_symlink(), "{prefix:?} is a link");
        }
    }
}

#[cfg(unix)]
#[test]
fn the_returned_name_of_a_spelled_out_path_is_the_plain_name_of_the_real_file() {
    let dir = Scratch::new("pair");
    let real_dir = dir.path().canonicalize().unwrap();
    fs::create_dir(dir.join("sub")).unwrap();
    fs::write(dir.join("real.db"), database(4096, 4096)).unwrap();
    for given in [
        format!("{}/sub/../real.db", dir.path().display()),
        format!("{}/./real.db", dir.path().display()),
        format!("{}//real.db", dir.path().display()),
        format!("{}/real.db/", dir.path().display()),
        format!("{}/real.db/.", dir.path().display()),
    ] {
        let name = accepted(Path::new(&given));
        assert_eq!(name, real_dir.join("real.db"), "{given}");
        assert_plain(&name);
    }
    // A name that does not exist yet keeps its resolved directory.
    let name = accepted(&dir.join("sub/./../new.db"));
    assert_eq!(name, real_dir.join("new.db"));
}

#[cfg(unix)]
#[test]
fn a_relative_path_comes_back_absolute() {
    // The test's working directory is the crate's: `Cargo.toml` is a file
    // there, and a name beside it that does not exist is a new database.
    let name = accepted(Path::new("./no-such-database.db"));
    assert!(name.is_absolute(), "{name:?}");
    assert_eq!(
        name,
        std::env::current_dir()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join("no-such-database.db")
    );
    let name = accepted(Path::new("Cargo.toml"));
    assert_plain(&name);
    assert!(name.ends_with("Cargo.toml"));
}

#[cfg(unix)]
#[test]
fn a_database_behind_links_comes_back_as_the_name_of_its_target() {
    let (dir, real) = target_with_crafted_log();
    fs::write(dir.join("target/state.db-wal"), log(32, 4096)).unwrap();
    let real = real.canonicalize().unwrap();
    for count in [1, 3, 40] {
        let links = Scratch::new("pair");
        let name = accepted(&chain(&links, &real, count));
        assert_eq!(name, real, "{count} links");
        assert_plain(&name);
    }
    let links = Scratch::new("pair");
    let first = chain(&links, &real, 41);
    assert!(refusal(&first).contains("longer than"));
}

#[cfg(unix)]
#[test]
fn a_dangling_link_comes_back_as_its_target_and_a_log_beside_it_is_refused() {
    let dir = Scratch::new("pair");
    let real_dir = dir.path().canonicalize().unwrap();
    std::os::unix::fs::symlink("missing.db", dir.join("state.db")).unwrap();
    let name = accepted(&dir.join("state.db"));
    assert_eq!(name, real_dir.join("missing.db"));
    fs::write(dir.join("missing.db-wal"), log(32, 4096)).unwrap();
    let message = refusal(&dir.join("state.db"));
    assert!(message.contains("has no database"), "{message}");
    assert!(message.contains("missing.db-wal"), "{message}");
}

#[cfg(unix)]
#[test]
fn the_returned_name_in_a_linked_directory_has_no_link_in_it() {
    let dir = Scratch::new("pair");
    let real = dir.join("realdir");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("state.db"), database(4096, 4096)).unwrap();
    std::os::unix::fs::symlink("realdir", dir.join(".branchyard")).unwrap();
    // The database file in it is a link, too, to a sibling by a `..` path.
    fs::write(real.join("other.db"), database(4096, 4096)).unwrap();
    std::os::unix::fs::symlink("../realdir/other.db", real.join("linked.db")).unwrap();
    let name = accepted(&dir.join(".branchyard/state.db"));
    assert_eq!(name, real.canonicalize().unwrap().join("state.db"));
    assert_plain(&name);
    let name = accepted(&dir.join(".branchyard/linked.db"));
    assert_eq!(name, real.canonicalize().unwrap().join("other.db"));
    assert_plain(&name);
}
