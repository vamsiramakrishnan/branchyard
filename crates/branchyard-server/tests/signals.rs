//! The `branchyard-server` binary stopped by SIGTERM (what `docker stop`
//! sends) and SIGINT: each starts the same graceful shutdown, and the
//! process exits within its grace period even with a running operation and
//! a connection that never finishes its request; and what it logs about
//! its token, closed connections and a shutdown. Unix only; requires
//! `kill`.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use branchyard_client::api::OperationState;
use branchyard_client::{new_key, Client};
use branchyard_testkit::wait;
use common::{fake_agent, started, task, Fixture, TOKEN};

/// The server binary on an ephemeral port, serving the fixture's
/// repository with the fake agent, and its URL.
fn spawn(f: &Fixture, grace: &str) -> (Child, String) {
    let token = f.dir.join("token");
    std::fs::write(&token, format!("{TOKEN}\n")).unwrap();
    let mut child =
        branchyard_testkit::hermetic(&mut Command::new(env!("CARGO_BIN_EXE_branchyard-server")))
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
            // Empty when the log went to a file instead.
            let mut stderr = String::new();
            if let Some(mut piped) = child.stderr.take() {
                std::io::Read::read_to_string(&mut piped, &mut stderr).unwrap();
            }
            (status.code(), elapsed, stderr)
        }
        Err(_) => {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("still running {limit:?} after SIG{signal}");
        }
    }
}

/// What `branchyard-server` (and `by serve`) logs when started as
/// docs/server.md starts it, with no token: where the token it made is, in
/// one sentence; nothing above debug when a client closes the event stream
/// it was reading (every `by --remote run` does once its work is done: on
/// 27 October 2026 each left `WARN best-effort step failed: serve a
/// connection: connection closed before message completed`); and, at a
/// shutdown, what runs and gets the grace period, what stays queued, and
/// what became of them, where it said only `shutting down; running
/// operations may finish`.
#[test]
fn the_log_says_where_the_token_is_and_what_a_shutdown_did() {
    let f = Fixture::new();
    let log_path = f.dir.join("server.log");
    let mut child =
        branchyard_testkit::hermetic(&mut Command::new(env!("CARGO_BIN_EXE_branchyard-server")))
            .env("BRANCHYARD_LOG", "info,branchyard_server::serve=debug")
            .args([
                "--listen",
                "127.0.0.1:0",
                "--shutdown-grace",
                "1",
                "--max-running",
                "1",
                "--no-inventory",
            ])
            .arg("--repo")
            .arg(format!("app={}", f.root.display()))
            .arg("--data-dir")
            .arg(&f.data)
            .arg("--harness-command")
            .arg(format!("gemini-cli={}", fake_agent().display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log_path).unwrap())
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
    let log = || std::fs::read_to_string(&log_path).unwrap();
    let token_file = f.data.join("token");
    let token = std::fs::read_to_string(&token_file).unwrap();
    assert!(
        log().contains(&format!(
            "created a token in {}; clients pass --token-file with it",
            token_file.display()
        )),
        "{}",
        log()
    );
    let client = Client::new(&url, token.trim()).unwrap();

    // A client reads the event stream, then goes away.
    let mut stream = TcpStream::connect(url.strip_prefix("http://").unwrap()).unwrap();
    write!(
        stream,
        "GET /v1/repos/app/events/stream HTTP/1.1\r\nHost: x\r\n\
         Authorization: Bearer {}\r\n\r\n",
        token.trim()
    )
    .unwrap();
    let mut status = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut status)
        .unwrap();
    assert!(status.contains(" 200 "), "the stream: {status:?}");
    drop(stream);

    // One operation runs (and never ends by itself); with --max-running 1
    // the next waits in the queue. The first one's events go to the
    // closed stream.
    let repo = client.repo("app");
    repo.submit_task(&task("HANG", "held"), &new_key()).unwrap();
    wait::until("the turn to start", || started(&client, "app", "held"));
    let queued = repo
        .submit_task(&task("WRITE w.txt=1", "waits"), &new_key())
        .unwrap();
    wait::until("the server to see the stream's client gone", || {
        log().contains("a client closed its connection")
    });

    let (code, _, _) = stop(&mut child, "TERM", Duration::from_secs(30));
    assert_eq!(code, Some(0), "{}", log());
    let log = log();
    assert!(!log.contains("wrote a secret file"), "{log}");
    assert!(!log.contains("serve a connection"), "{log}");
    assert!(
        log.contains(
            "shutting down: 1 running operation may finish within 1s (signal again to stop \
             waiting); 1 queued stay queued for the next start"
        ),
        "{log}"
    );
    assert!(
        log.contains(
            "stopped: 0 finished within the grace period; 1 recorded interrupted; 1 left in the \
             queue for the next start, or another server on the database"
        ),
        "{log}"
    );
    // The queued one runs after the next start.
    let (mut again, url) = spawn(&f, "0");
    let restarted = Client::new(&url, TOKEN).unwrap();
    let ran = restarted.wait(&queued.id).unwrap();
    assert_eq!(ran.state, OperationState::Succeeded, "{ran:?}");
    let (code, _, _) = stop(&mut again, "INT", Duration::from_secs(20));
    assert_eq!(code, Some(0));
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
        // The stream is established once the server has answered with its
        // headers; the signal goes after that, not after a guess.
        let mut headers = BufReader::new(stream.try_clone().unwrap());
        stream
            .set_read_timeout(Some(wait::scaled(Duration::from_secs(30))))
            .unwrap();
        let mut status = String::new();
        headers.read_line(&mut status).unwrap();
        assert!(status.contains(" 200 "), "the stream: {status:?}");
        loop {
            let mut line = String::new();
            headers.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
        }
        let (code, elapsed, stderr) = stop(&mut child, signal, Duration::from_secs(20));
        assert_eq!(code, Some(0), "SIG{signal}: {stderr}");
        // Contract: an idle server does not wait out its grace period (60 s
        // here). The bound is a sixth of it: room for the spawn of `kill` and
        // a coverage-instrumented shutdown on a loaded runner, and still far
        // under the 60 s a server that waited would take.
        assert!(
            elapsed < wait::scaled(Duration::from_secs(10)),
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
    // Connections are accepted in the order they arrive: a request that
    // gets its answer on a later connection means the slow one was
    // accepted before it and is being read.
    let mut probe = TcpStream::connect(addr).unwrap();
    probe
        .set_read_timeout(Some(wait::scaled(Duration::from_secs(30))))
        .unwrap();
    probe
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut answer = String::new();
    std::io::Read::read_to_string(&mut probe, &mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer:?}");
    let (code, elapsed, stderr) = stop(&mut child, "TERM", Duration::from_secs(30));
    assert_eq!(code, Some(0), "{stderr}");
    // Contract: a running operation gets the whole grace period (2 s). The
    // clock is the server's, so this bound is not scaled.
    assert!(
        elapsed >= Duration::from_millis(1900),
        "running operations get the grace period: {elapsed:?}"
    );
    // Contract: the process is gone within the grace period plus the time
    // to record the operation, not the grace period plus the ten seconds
    // connections used to get first. The bound is the grace period plus a
    // margin of 5 s (half of that regression's ten) for the spawn of `kill`
    // and a slow, instrumented shutdown.
    assert!(
        elapsed < wait::scaled(Duration::from_secs(7)),
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
