//! The kit's own tests: a helper that can pass while broken is worse than
//! none, so each failure mode is shown to fail.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use branchyard_testkit::wait;
use branchyard_testkit::{MockHttp, Response, Scratch};

/// The message of the panic `f` raises; fails if it does not panic.
fn panic_message(f: impl FnOnce()) -> String {
    let panic = catch_unwind(AssertUnwindSafe(f)).expect_err("expected a failure");
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default()
}

fn send(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    reply
}

#[test]
fn a_condition_that_already_holds_returns_without_waiting() {
    let started = Instant::now();
    wait::until("a true condition", || true);
    assert_eq!(wait::until("a value", || Some(3)), 3);
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_timeout_names_what_was_awaited_and_the_last_value_observed() {
    let mut polls = 0;
    let message = panic_message(|| {
        wait::until_for(
            "the counter to reach 1000",
            Duration::from_millis(200),
            || {
                polls += 1;
                if polls < 1000 {
                    Err(polls)
                } else {
                    Ok(())
                }
            },
        )
    });
    assert!(
        message.contains("the counter to reach 1000"),
        "no subject: {message}"
    );
    assert!(
        message.contains(&format!("last observed: {polls}")),
        "no last value ({polls} polls): {message}"
    );
    assert!(polls > 1, "never polled again");
}

#[test]
fn until_value_returns_the_value_that_satisfied_it() {
    let mut n = 0;
    let seen = wait::until_value(
        "n to reach 3",
        || {
            n += 1;
            n
        },
        |n| *n >= 3,
    );
    assert_eq!(seen, 3);
}

#[test]
fn a_bool_check_reports_false_and_an_option_check_reports_none() {
    let message = panic_message(|| wait::until_for("never", Duration::from_millis(30), || false));
    assert!(message.contains("last observed: false"), "{message}");
    let message = panic_message(|| {
        wait::until_for("nothing", Duration::from_millis(30), || None::<u8>);
    });
    assert!(message.contains("last observed: None"), "{message}");
}

#[test]
fn scaling_multiplies_the_timeout() {
    // Only the pure part: the environment is shared by every test thread.
    assert_eq!(
        wait::scaled(Duration::from_secs(2)),
        Duration::from_secs(2).mul_f64(wait::scale())
    );
}

#[test]
fn gone_and_exec_follow_a_real_process() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    wait::exec(pid, "sleep");
    assert!(wait::alive(pid));
    child.kill().unwrap();
    child.wait().unwrap();
    wait::gone(pid);
}

#[test]
fn the_mock_answers_and_records_a_request() {
    let mock = MockHttp::start(|request| {
        Response::json(200, &serde_json::json!({"echo": request.body_text()}))
    });
    let reply = send(
        mock.addr(),
        "POST /v1/x HTTP/1.1\r\nHost: x\r\nX-Key: k1\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.contains(r#"{"echo":"hello"}"#), "{reply}");
    let seen = mock.await_requests(1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/v1/x");
    assert_eq!(seen[0].header("x-key"), Some("k1"));
    assert_eq!(seen[0].body_text(), "hello");
    mock.finish();
}

#[test]
fn a_client_that_sends_a_truncated_body_fails_the_owning_test() {
    let message = panic_message(|| {
        let mock = MockHttp::start(|_| Response::new(200, "fine"));
        let mut stream = TcpStream::connect(mock.addr()).unwrap();
        // Promises ten bytes, sends four, and hangs up.
        stream
            .write_all(b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nabcd")
            .unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        drop(stream);
        mock.finish();
    });
    assert!(message.contains("body was cut off"), "{message}");
    assert!(message.contains("expected 10 bytes"), "{message}");
}

#[test]
fn a_mock_dropped_without_finish_still_fails_the_test() {
    let message = panic_message(|| {
        let mock = MockHttp::start(|_| Response::new(200, "fine"));
        let mut stream = TcpStream::connect(mock.addr()).unwrap();
        stream
            .write_all(b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nab")
            .unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        // `mock` is dropped here.
    });
    assert!(message.contains("body was cut off"), "{message}");
}

#[test]
fn a_panicking_handler_fails_the_owning_test() {
    let message = panic_message(|| {
        let mock = MockHttp::start(|_| panic!("the handler broke"));
        let mut stream = TcpStream::connect(mock.addr()).unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut sink = String::new();
        let _ = stream.read_to_string(&mut sink);
        mock.finish();
    });
    assert!(message.contains("the handler broke"), "{message}");
}

#[test]
fn a_connection_that_sends_nothing_is_not_an_error() {
    let mock = MockHttp::start(|_| Response::new(200, "fine"));
    drop(TcpStream::connect(mock.addr()).unwrap());
    mock.finish();
}

#[test]
fn the_ipv6_mock_serves_or_is_skipped() {
    let Some(mock) = MockHttp::start_v6(|_| Response::new(200, "six")) else {
        return;
    };
    assert!(mock.url().starts_with("http://[::1]:"), "{}", mock.url());
    let reply = send(mock.addr(), "GET / HTTP/1.1\r\n\r\n");
    assert!(reply.ends_with("six"), "{reply}");
    mock.finish();
}

#[test]
fn a_scratch_directory_is_removed_on_drop_and_close() {
    let scratch = Scratch::new("kit");
    let path = scratch.path().to_path_buf();
    std::fs::write(scratch.join("f"), "x").unwrap();
    drop(scratch);
    assert!(!path.exists());
    let scratch = Scratch::new("kit");
    let path = scratch.path().to_path_buf();
    scratch.close();
    assert!(!path.exists());
}

#[test]
fn a_hanging_response_holds_the_connection_open_until_the_server_is_finished() {
    let mock = MockHttp::start(|_| Response::hang());
    let mut stream = TcpStream::connect(mock.addr()).unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
    mock.await_requests(1);
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut buf = [0u8; 16];
    let err = stream.read(&mut buf).expect_err("nothing may be answered");
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "{err}"
    );
    mock.finish();
    stream.set_read_timeout(None).unwrap();
    assert_eq!(stream.read(&mut buf).unwrap(), 0, "closed once finished");
}

#[test]
fn a_closing_response_ends_the_connection_without_a_byte() {
    let mock = MockHttp::start(|_| Response::close());
    let reply = send(mock.addr(), "GET / HTTP/1.1\r\n\r\n");
    assert_eq!(reply, "");
    assert_eq!(mock.requests().len(), 1);
    mock.finish();
}
