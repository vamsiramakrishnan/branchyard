//! Closing a harness that stops writing but does not exit.

#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use branchyard_harness::acp::Acp;
use branchyard_harness::SessionMode;
use branchyard_runtime::{Environment, Session};
use common::{open, workdir};

#[test]
fn close_enforces_the_grace_period_after_stdout_closes() {
    let dir = workdir("close-stdout-eof");
    // Closes its protocol stream at once, then stays alive.
    let driver = Acp::new(vec![
        "sh".into(),
        "-c".into(),
        "exec 1>&-; exec sleep 30".into(),
    ]);
    let session = Session::start(
        Box::new(driver),
        open(&dir, SessionMode::Fresh),
        &Environment::new(dir.join("home")),
        None,
    )
    .unwrap();
    let started = Instant::now();
    let closed = session.close(Duration::from_secs(1)).unwrap();
    assert!(
        closed.forced,
        "the harness outlived the grace period and must be killed"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "close took {:?}",
        started.elapsed()
    );
}
