//! Closing a harness that stops writing but does not exit.

#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use branchyard_harness::acp::Acp;
use branchyard_harness::claude_code::ClaudeCode;
use branchyard_harness::{Event, SessionMode};
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

/// A stand-in for Claude Code 2.1.293 with a background task: it does not
/// exit when its input closes (the real CLI waits for its tasks, then
/// runs another model turn), only when asked to end its session.
const CLAUDE_WITH_A_BACKGROUND_TASK: &str = r#"
read -r line
echo '{"type":"control_response","response":{"subtype":"success","request_id":"branchyard-1","response":{}}}'
while read -r line; do
  case "$line" in
    *end_session*)
      echo '{"type":"control_response","response":{"subtype":"success","request_id":"branchyard-2"}}'
      exit 0;;
  esac
done
exec sleep 30
"#;

#[test]
fn close_asks_the_harness_to_end_its_session_before_closing_its_input() {
    let dir = workdir("close-end-session");
    let driver = ClaudeCode::new(vec![
        "sh".into(),
        "-c".into(),
        CLAUDE_WITH_A_BACKGROUND_TASK.into(),
        "claude".into(),
    ]);
    let mut session = Session::start(
        Box::new(driver),
        open(&dir, SessionMode::Fresh),
        &Environment::new(dir.join("home")),
        None,
    )
    .unwrap();
    session.wait_ready(Duration::from_secs(10)).unwrap();
    let started = Instant::now();
    let closed = session.close(Duration::from_secs(10)).unwrap();
    assert!(
        !closed.forced,
        "asked to end its session, the harness exits on its own"
    );
    assert!(closed.survivors.is_empty(), "{:?}", closed.survivors);
    assert!(
        !closed
            .events
            .iter()
            .any(|e| matches!(e, Event::Warning { .. } | Event::ProtocolViolation { .. })),
        "{:?}",
        closed.events
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "close took {:?}",
        started.elapsed()
    );
}
