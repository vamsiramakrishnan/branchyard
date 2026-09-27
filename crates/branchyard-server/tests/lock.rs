//! One server per data directory.

mod common;

use common::{Fixture, Server};

#[test]
fn a_second_server_on_the_same_data_directory_fails_fast() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let started = std::time::Instant::now();
    let error = Server::try_start(f.config()).err().unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert!(
        error.contains("already in use by a Branchyard server"),
        "{error}"
    );
    assert!(
        error.contains(&format!("(pid {})", std::process::id())),
        "{error}"
    );
    // The first server is unaffected, and once it stops the directory is
    // free again.
    assert_eq!(server.client().repos().unwrap()[0].name, "app");
    server.stop();
    let again = Server::start(f.config());
    assert_eq!(again.client().repos().unwrap()[0].name, "app");
}
