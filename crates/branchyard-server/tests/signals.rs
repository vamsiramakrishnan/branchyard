//! The `branchyard-server` binary stopped by SIGTERM (what `docker stop`
//! sends) and SIGINT: each starts the same graceful shutdown, and the
//! process exits within its grace period even with a running operation and
//! a connection that never finishes its request. Unix only; requires
//! `kill`.
#![cfg(unix)]
#![allow(clippy::let_underscore_must_use, clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use branchyard_client::{new_key, Client};
use branchyard_testkit::wait;
use common::{fake_agent, started, task, Fixture, TOKEN};

/// The server binary on an ephemeral port, serving the fixture's
/// repository with the fake agent, and its URL.
fn spawn(f: &Fixture, grace: &str) -> (Child, String) {
    let token = f.dir.join("token");
    std::fs::write(&token, format!("{TOKEN}\n")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_branchyard-server"))
        .args([
            "--listen",
            "127.0.0.1:0",
            "--quiet",
            "--shutdown-grace",
            grace,
        ])
        .arg("--repo")
        .arg(format!("app={}", f.root.display()))
        .arg("--data-dir")
        .arg(&f.data)
        .arg("--token-file")
        .arg(&token)
        .arg("--harness-command")
        .arg(format!("gemini-cli={}", fake_agent().display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line
        .trim()
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("server did not start: {line:?}"))
        .to_owned();
    (child, url)
}

/// Send `signal` and time the exit; kills the server after `limit`.
fn stop(child: &mut Child, signal: &str, limit: Duration) -> (Option<i32>, Duration, String) {
    let sent = Instant::now();
    let status = Command::new("kill")
        .args([&format!("-{signal}"), &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let exited = wait::try_until_for(limit, || child.try_wait().unwrap());
    let elapsed = sent.elapsed();
    match exited {
        Ok(status) => {
            let mut stderr = String::new();
            std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
            (status.code(), elapsed, stderr)
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("still running {limit:?} after SIG{signal}");
        }
    }
}

#[test]
fn sigterm_and_sigint_stop_an_idle_server_at_once() {
    for signal in ["TERM", "INT"] {
        let f = Fixture::new();
        let (mut child, url) = spawn(&f, "60");
        // An open event stream ends with the shutdown too.
        let mut stream = TcpStream::connect(url.strip_prefix("http://").unwrap()).unwrap();
        write!(
            stream,
            "GET /v1/repos/app/events/stream HTTP/1.1\r\nHost: x\r\n\
             Authorization: Bearer {TOKEN}\r\n\r\n"
        )
        .unwrap();
        wait::settle("the stream is established", Duration::from_millis(200));
        let (code, elapsed, stderr) = stop(&mut child, signal, Duration::from_secs(20));
        assert_eq!(code, Some(0), "SIG{signal}: {stderr}");
        assert!(
            elapsed < Duration::from_secs(3),
            "SIG{signal} took {elapsed:?}"
        );
    }
}

/// `--shutdown-grace 2` with a turn that never ends and a client that sent
/// half a request: the process is gone within the grace period (plus the
/// time to record the operation), not the grace period plus the ten
/// seconds connections used to get first, and the turn is recorded as
/// interrupted.
#[test]
fn sigterm_stops_the_server_within_its_grace_period() {
    let f = Fixture::new();
    let (mut child, url) = spawn(&f, "2");
    let client = Client::new(&url, TOKEN).unwrap();
    let op = client
        .repo("app")
        .submit_task(&task("HANG", "held"), &new_key())
        .unwrap();
    wait::until("the turn to start", || started(&client, "app", "held"));
    let addr = url.strip_prefix("http://").unwrap();
    let mut slow = TcpStream::connect(addr).unwrap();
    slow.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n")
        .unwrap();
    wait::settle("the slow request is read", Duration::from_millis(200));
    let (code, elapsed, stderr) = stop(&mut child, "TERM", Duration::from_secs(30));
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        elapsed >= Duration::from_millis(1900),
        "running operations get the grace period: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(4500),
        "SIGTERM took {elapsed:?}; the grace period is 2s"
    );
    drop(slow);
    // The next start finds the operation recorded as interrupted.
    let (mut again, url) = spawn(&f, "0");
    let client = Client::new(&url, TOKEN).unwrap();
    let state = client.operation(&op.id).unwrap().state;
    assert_eq!(
        state,
        branchyard_client::api::OperationState::Interrupted,
        "{stderr}"
    );
    let (code, _, _) = stop(&mut again, "INT", Duration::from_secs(20));
    assert_eq!(code, Some(0));
}
