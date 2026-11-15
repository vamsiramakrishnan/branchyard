//! `BRANCHYARD_REGISTRY=file:reg.db` reaches the registry as the path
//! `file:reg.db`, which SQLite would read as a URI, not as a path; and
//! `BRANCHYARD_REGISTRY=:memory:` as the name for which SQLite opens an
//! in-memory database (RUSTSEC-2026-0459 check). The registry refuses both,
//! and the empty name, and nothing is made.
//!
//! This is the only test of its binary: it changes the process's working
//! directory into a directory of its own, so that a refusal lost in a later
//! change would write there and not into the source tree.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::path::Path;

use branchyard::services::LocalRegistry;
use branchyard_testkit::Scratch;

#[test]
fn the_registry_refuses_a_name_sqlite_would_read_as_a_uri() {
    let dir = Scratch::new("wal-uri");
    std::env::set_current_dir(dir.path()).unwrap();
    let name = Path::new("file:reg.db");
    let message = LocalRegistry::open(name).err().unwrap().to_string();
    assert!(message.contains("file:reg.db"), "{message}");
    assert!(
        message.contains("URI") && message.contains("not as a path"),
        "{message}"
    );
    assert!(!message.contains("another file"), "{message}");
    assert!(message.contains("./"), "{message}");
    // Refused before anything was made, here or at the file the URI names.
    assert_nothing_made(dir.path());

    // `BRANCHYARD_REGISTRY=:memory:` reaches the registry as this name:
    // SQLite opens an in-memory database for it, so it is refused and no
    // file of that name is made in the working directory.
    let message = LocalRegistry::open(Path::new(":memory:"))
        .err()
        .unwrap()
        .to_string();
    assert!(
        message.contains("in-memory database") && message.contains("not a file"),
        "{message}"
    );
    assert!(message.contains("./:memory:"), "{message}");
    assert_nothing_made(dir.path());

    // The empty name is refused too, saying that the path is empty.
    let message = LocalRegistry::open(Path::new(""))
        .err()
        .unwrap()
        .to_string();
    assert!(message.contains("the path is empty"), "{message}");
    assert_nothing_made(dir.path());
}

fn assert_nothing_made(dir: &Path) {
    let made: Vec<_> = fs::read_dir(dir).unwrap().collect();
    assert!(made.is_empty(), "{made:?}");
}
