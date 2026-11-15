//! A database whose write-ahead log has another page size is refused before
//! SQLite sees the pair (RUSTSEC-2026-0459): through every way this crate
//! opens one, the store read-write (`Yard::open`), its two read-only readers
//! (`glance::read`, `doctor::store_schema`) and the service registry
//! (including the file `BRANCHYARD_REGISTRY` names). The legitimate pair, a
//! real database beside its real log, still opens.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::path::Path;

use branchyard::services::LocalRegistry;
use branchyard::Yard;
use branchyard_testkit::Scratch;

fn repository() -> Scratch {
    let dir = Scratch::new("wal-pair");
    let status = branchyard_support::child::command("git")
        .args(["init", "-q"])
        .arg(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    dir
}

/// A log header (32 bytes) recording page size `size`.
fn forged_log(size: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 32];
    bytes[8..12].copy_from_slice(&size.to_be_bytes());
    bytes
}

/// Leave a log beside `db` that SQLite did not write for it.
fn plant(db: &Path) -> std::path::PathBuf {
    let wal = db.with_file_name(format!("{}-wal", db.file_name().unwrap().to_string_lossy()));
    fs::write(&wal, forged_log(512)).unwrap();
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
fn a_real_pair_is_accepted_by_every_site() {
    let repo = repository();
    let yard = Yard::open(repo.path()).unwrap();
    // The first yard is still open: its log is on disk beside the database.
    let db = repo.join(".branchyard/state.db");
    let wal = repo.join(".branchyard/state.db-wal");
    assert!(
        wal.metadata().unwrap().len() >= 32,
        "a log beside the store"
    );
    let second = Yard::open(repo.path()).unwrap();
    assert!(branchyard::glance::read(repo.path(), None)
        .unwrap()
        .is_some());
    assert!(branchyard::doctor::store_schema(repo.path())
        .unwrap()
        .is_some());
    yard.services().unwrap();
    assert!(db.is_file());
    drop((yard, second));
}

#[test]
fn the_store_refuses_a_log_with_another_page_size() {
    let repo = repository();
    drop(Yard::open(repo.path()).unwrap());
    let wal = plant(&repo.join(".branchyard/state.db"));
    let before = fs::read(&wal).unwrap();
    let message = Yard::open(repo.path()).err().unwrap().to_string();
    assert_refused(&message, &wal);
    assert_eq!(fs::read(&wal).unwrap(), before);
    assert!(!repo.join(".branchyard/state.db-shm").exists());
}

#[test]
fn glance_refuses_it_for_the_hooks() {
    let repo = repository();
    drop(Yard::open(repo.path()).unwrap());
    let wal = plant(&repo.join(".branchyard/state.db"));
    let message = branchyard::glance::read(repo.path(), None)
        .err()
        .unwrap()
        .to_string();
    assert_refused(&message, &wal);
    assert!(!repo.join(".branchyard/state.db-shm").exists());
}

#[test]
fn doctor_refuses_it() {
    let repo = repository();
    drop(Yard::open(repo.path()).unwrap());
    let wal = plant(&repo.join(".branchyard/state.db"));
    let message = branchyard::doctor::store_schema(repo.path())
        .err()
        .unwrap()
        .to_string();
    assert_refused(&message, &wal);
    assert!(!repo.join(".branchyard/state.db-shm").exists());
}

#[test]
fn the_registry_refuses_it_at_its_own_path() {
    let dir = Scratch::new("wal-registry");
    let path = dir.join("shared-registry.db");
    drop(LocalRegistry::open(&path).unwrap());
    let wal = plant(&path);
    let message = LocalRegistry::open(&path).err().unwrap().to_string();
    assert_refused(&message, &wal);
    assert!(!dir.join("shared-registry.db-shm").exists());
}

#[test]
fn a_store_left_by_a_killed_writer_still_opens() {
    // A writer killed without closing leaves the log: copy a live pair.
    let repo = repository();
    let live = Yard::open(repo.path()).unwrap();
    let copy = repository();
    fs::create_dir_all(copy.join(".branchyard")).unwrap();
    for name in ["state.db", "state.db-wal"] {
        fs::copy(
            repo.join(".branchyard").join(name),
            copy.join(".branchyard").join(name),
        )
        .unwrap();
    }
    assert!(
        copy.join(".branchyard/state.db-wal")
            .metadata()
            .unwrap()
            .len()
            >= 32
    );
    Yard::open(copy.path()).unwrap();
    drop(live);
}

#[cfg(unix)]
#[test]
fn the_registry_names_a_file_it_cannot_read() {
    // The registry is a link into a path below a regular file: its log's
    // name fails with "not a directory", which must come out with the path.
    let dir = Scratch::new("wal-registry");
    fs::write(dir.join("plain"), b"x").unwrap();
    let link = dir.join("registry.db");
    std::os::unix::fs::symlink(dir.join("plain/registry.db"), &link).unwrap();
    let error = LocalRegistry::open(&link).err().unwrap();
    let message = error.to_string();
    assert!(
        message.contains("plain/registry.db-wal") && message.contains("could not be read"),
        "{message}"
    );
}

/// A database file SQLite made: a store that was opened and closed, moved to
/// `to`.
#[cfg(unix)]
fn real_database_at(to: &Path) {
    let source = repository();
    drop(Yard::open(source.path()).unwrap());
    fs::copy(source.join(".branchyard/state.db"), to).unwrap();
}

#[cfg(unix)]
#[test]
fn the_store_opens_what_was_checked_through_a_linked_directory_and_a_linked_database() {
    let repo = repository();
    let elsewhere = Scratch::new("wal-target");
    fs::create_dir_all(elsewhere.join("real")).unwrap();
    fs::create_dir_all(elsewhere.join("other")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("real"), repo.join(".branchyard")).unwrap();
    std::os::unix::fs::symlink("../other/store.db", elsewhere.join("real/state.db")).unwrap();
    let yard = Yard::open(repo.path()).unwrap();
    yard.services().unwrap();
    // The files are where the targets are, the log beside the database's.
    assert!(elsewhere.join("other/store.db").metadata().unwrap().len() > 0);
    assert!(elsewhere.join("other/store.db-wal").exists());
    assert!(!elsewhere.join("real/state.db-wal").exists());
    assert!(elsewhere.join("real/registry.db").exists());
    assert!(branchyard::glance::read(repo.path(), None)
        .unwrap()
        .is_some());
    assert!(branchyard::doctor::store_schema(repo.path())
        .unwrap()
        .is_some());
    drop(yard);
    // A forged log beside the target is the one refused.
    let wal = plant(&elsewhere.join("other/store.db"));
    assert_refused(&Yard::open(repo.path()).err().unwrap().to_string(), &wal);
}

#[cfg(unix)]
#[test]
fn the_store_refuses_a_link_through_a_missing_directory_and_dotdot() {
    let repo = repository();
    fs::create_dir_all(repo.join(".branchyard")).unwrap();
    real_database_at(&repo.join(".branchyard/real.db"));
    plant(&repo.join(".branchyard/real.db"));
    std::os::unix::fs::symlink("nope/../real.db", repo.join(".branchyard/state.db")).unwrap();
    let message = Yard::open(repo.path()).err().unwrap().to_string();
    assert!(message.contains("cannot be resolved"), "{message}");
    assert!(message.contains("nope"), "{message}");
}

#[cfg(unix)]
#[test]
fn the_registry_opens_what_was_checked_and_refuses_the_dotdot_trick() {
    let dir = Scratch::new("wal-registry");
    fs::create_dir(dir.join("real")).unwrap();
    std::os::unix::fs::symlink("real", dir.join("linked")).unwrap();
    std::os::unix::fs::symlink("real/reg.db", dir.join("registry.db")).unwrap();
    let registry = LocalRegistry::open(dir.join("linked/../registry.db")).unwrap();
    assert!(dir.join("real/reg.db").metadata().unwrap().len() > 0);
    // The registry keeps the path it was given; the files are at the target.
    assert_eq!(registry.path(), dir.join("linked/../registry.db"));
    drop(registry);
    // The trick: the link's text drops `nope/..`; the system cannot.
    plant(&dir.join("real/reg.db"));
    std::os::unix::fs::symlink("nope/../real/reg.db", dir.join("tricky.db")).unwrap();
    let message = LocalRegistry::open(dir.join("tricky.db"))
        .err()
        .unwrap()
        .to_string();
    assert!(message.contains("cannot be resolved"), "{message}");
}
