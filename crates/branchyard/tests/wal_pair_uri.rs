//! `BRANCHYARD_REGISTRY=file:reg.db` reaches the registry as the path
//! `file:reg.db`, which SQLite would read as a URI and open as `reg.db`
//! (RUSTSEC-2026-0459 check). The registry refuses it, and nothing is made.
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
        message.contains("URI") && message.contains("another file"),
        "{message}"
    );
    assert!(message.contains("./"), "{message}");
    // Refused before anything was made, here or at the file the URI names.
    let made: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
    assert!(made.is_empty(), "{made:?}");
}
