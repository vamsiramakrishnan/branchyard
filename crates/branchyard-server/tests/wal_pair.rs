//! A database whose write-ahead log has another page size is refused before
//! SQLite sees the pair (RUSTSEC-2026-0459): the operation registry, the
//! triggers and the companion stores (three connections to one file,
//! `<data_dir>/state.db`), the sync outbox (`<data_dir>/sync/<repo>.db`), and
//! a server refuses to start on such a data directory, naming the file. The
//! legitimate pair, a real database beside its real log, still opens.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use branchyard_server::companion::store::SqliteCompanion;
use branchyard_server::config::SyncSettings;
use branchyard_server::store::SqliteStore;
use branchyard_server::triggers::store::SqliteTriggers;
use branchyard_sync::SyncConfig;
use branchyard_testkit::Scratch;
use common::{Fixture, Server};

/// Leave a log beside `db` whose header says 512-byte pages.
fn plant(db: &Path) -> std::path::PathBuf {
    let wal = db.with_file_name(format!("{}-wal", db.file_name().unwrap().to_string_lossy()));
    let mut header = vec![0u8; 32];
    header[8..12].copy_from_slice(&512u32.to_be_bytes());
    fs::write(&wal, header).unwrap();
    wal
}

fn assert_refused(message: &str, wal: &Path) {
    assert!(message.contains(&wal.display().to_string()), "{message}");
    assert!(
        message.contains("512") && message.contains("4096"),
        "{message}"
    );
    assert!(message.contains("move both files away"), "{message}");
}

#[test]
fn a_real_pair_with_a_live_log_opens_in_every_store() {
    let dir = Scratch::new("wal-stores");
    let path = dir.join("state.db");
    let store = SqliteStore::open(&path, None).unwrap();
    let triggers = SqliteTriggers::open(&path).unwrap();
    let companion = SqliteCompanion::open(&path).unwrap();
    assert!(dir.join("state.db-wal").metadata().unwrap().len() >= 32);
    // A fourth open while the others hold the log.
    drop(SqliteStore::open(&path, None).unwrap());
    drop((store, triggers, companion));
}

#[test]
fn each_store_on_the_server_file_refuses_a_forged_log() {
    let dir = Scratch::new("wal-stores");
    let path = dir.join("state.db");
    drop(SqliteStore::open(&path, None).unwrap());
    let wal = plant(&path);
    let before = fs::read(&wal).unwrap();
    assert_refused(
        &SqliteStore::open(&path, None).err().unwrap().to_string(),
        &wal,
    );
    assert_refused(
        &SqliteTriggers::open(&path).err().unwrap().to_string(),
        &wal,
    );
    assert_refused(
        &SqliteCompanion::open(&path).err().unwrap().to_string(),
        &wal,
    );
    assert_eq!(fs::read(&wal).unwrap(), before);
    assert!(!dir.join("state.db-shm").exists());
}

#[test]
fn a_server_refuses_to_start_on_a_forged_data_directory_naming_the_file() {
    let f = Fixture::new();
    Server::start(f.config()).stop();
    let wal = plant(&f.data.join("state.db"));
    let error = Server::try_start(f.config()).err().unwrap();
    assert_refused(&error, &wal);
    assert!(error.contains("operation registry"), "{error}");
}

#[test]
fn the_server_sync_outbox_refuses_a_forged_log() {
    let f = Fixture::new();
    let bucket = f.dir.join("bucket");
    let mut config = f.config();
    config.sync = Some(SyncSettings {
        config: SyncConfig {
            remote: format!("file://{}", bucket.display()),
            interval: Some("1s".into()),
            device: Some("server".into()),
            ..SyncConfig::default()
        },
        lease: Duration::from_secs(30),
    });
    Server::start(config.clone()).stop();
    let wal = plant(&f.data.join("sync/app.db"));
    let error = Server::try_start(config).err().unwrap();
    assert_refused(&error, &wal);
    assert!(error.contains("sync: app"), "{error}");
}

#[cfg(unix)]
#[test]
fn the_stores_open_what_was_checked_through_a_linked_directory_and_a_linked_file() {
    let dir = Scratch::new("wal-stores");
    fs::create_dir_all(dir.join("real")).unwrap();
    fs::create_dir_all(dir.join("other")).unwrap();
    std::os::unix::fs::symlink("real", dir.join("data")).unwrap();
    std::os::unix::fs::symlink("../other/server.db", dir.join("real/state.db")).unwrap();
    let path = dir.join("data/./state.db");
    let store = SqliteStore::open(&path, None).unwrap();
    let triggers = SqliteTriggers::open(&path).unwrap();
    let companion = SqliteCompanion::open(&path).unwrap();
    // The files are where the target is, the log beside the database.
    assert!(dir.join("other/server.db").metadata().unwrap().len() > 0);
    assert!(dir.join("other/server.db-wal").exists());
    assert!(!dir.join("real/state.db-wal").exists());
    // The store keeps the path it was given, not the target's name.
    assert_eq!(store.path(), path);
    drop((store, triggers, companion));
    let wal = plant(&dir.join("other/server.db"));
    assert_refused(
        &SqliteStore::open(&path, None).err().unwrap().to_string(),
        &wal,
    );
    assert_refused(
        &SqliteTriggers::open(&path).err().unwrap().to_string(),
        &wal,
    );
    assert_refused(
        &SqliteCompanion::open(&path).err().unwrap().to_string(),
        &wal,
    );
}

#[cfg(unix)]
#[test]
fn the_stores_refuse_a_link_through_a_missing_directory_and_dotdot() {
    let dir = Scratch::new("wal-stores");
    let real = dir.join("real.db");
    drop(SqliteStore::open(&real, None).unwrap());
    plant(&real);
    std::os::unix::fs::symlink("nope/../real.db", dir.join("state.db")).unwrap();
    let path = dir.join("state.db");
    for message in [
        SqliteStore::open(&path, None).err().unwrap().to_string(),
        SqliteTriggers::open(&path).err().unwrap().to_string(),
        SqliteCompanion::open(&path).err().unwrap().to_string(),
    ] {
        assert!(message.contains("cannot be resolved"), "{message}");
    }
}

/// `<dir>/<250 a>/<250 b>/<250 c>/../../../state.db`: the text is longer than the 512
/// bytes in which SQLite builds a name, so SQLite's own resolution of it opens
/// no file, while the name the check returns is short.
#[cfg(unix)]
fn long_way_to_state(dir: &Scratch) -> std::path::PathBuf {
    let deep = dir
        .join("a".repeat(250))
        .join("b".repeat(250))
        .join("c".repeat(250));
    fs::create_dir_all(&deep).unwrap();
    let path = deep.join("../../../state.db");
    assert!(path.as_os_str().len() > 600, "{path:?}");
    path
}

#[cfg(unix)]
#[test]
fn the_operation_store_opens_through_a_path_longer_than_sqlite_resolves() {
    let dir = Scratch::new("wal-long-store");
    let store = SqliteStore::open(long_way_to_state(&dir), None).unwrap();
    assert!(dir.join("state.db").metadata().unwrap().len() > 0);
    drop(store);
}

#[cfg(unix)]
#[test]
fn the_trigger_store_opens_through_a_path_longer_than_sqlite_resolves() {
    let dir = Scratch::new("wal-long-triggers");
    let store = SqliteTriggers::open(&long_way_to_state(&dir)).unwrap();
    assert!(dir.join("state.db").metadata().unwrap().len() > 0);
    drop(store);
}

#[cfg(unix)]
#[test]
fn the_companion_store_opens_through_a_path_longer_than_sqlite_resolves() {
    let dir = Scratch::new("wal-long-companion");
    let store = SqliteCompanion::open(&long_way_to_state(&dir)).unwrap();
    assert!(dir.join("state.db").metadata().unwrap().len() > 0);
    drop(store);
}

#[cfg(target_os = "linux")]
#[test]
fn the_three_stores_on_one_file_keep_sqlite_s_lock() {
    let dir = Scratch::new("wal-stores");
    let path = dir.join("state.db");
    let wal = dir.join("state.db-wal");
    let held = || branchyard_testkit::posix_locks_on(&path).unwrap();
    let store = SqliteStore::open(&path, None).unwrap();
    assert!(!held().is_empty(), "SQLite's lock after the first open");
    let triggers = SqliteTriggers::open(&path).unwrap();
    assert!(
        wal.metadata().unwrap().len() >= 32,
        "a log at the second open"
    );
    assert!(!held().is_empty(), "SQLite's lock after the trigger store");
    let companion = SqliteCompanion::open(&path).unwrap();
    assert!(
        wal.metadata().unwrap().len() >= 32,
        "a log at the third open"
    );
    assert!(
        !held().is_empty(),
        "SQLite's lock after the companion store"
    );
    drop((store, triggers, companion));
}
