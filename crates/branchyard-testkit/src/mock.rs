//! A mock HTTP server that cannot hide its own failures.
//!
//! The hand-rolled servers it replaces ignored `read_exact` and `write`
//! errors (`let _ = reader.read_exact(&mut body)`), so a client that sent a
//! truncated body was answered with a zeroed one and the test passed while
//! the thing under test was broken. Here every connection runs on a thread
//! whose `Result` is joined when the server is finished or dropped, and any
//! I/O error, truncated request, malformed request or handler panic fails
//! the test that owns the server.
//!
//! One thing is deliberately not an error: a connection that closes before
//! sending a single byte (a port probe).

#![allow(clippy::expect_used, clippy::panic)] // tests: a panic is the failure report
use std::collections::BTreeMap;
use std::io::{BufReader, ErrorKind, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use branchyard_support::LockExt;

use serde_json::Value;

use crate::wait;

/// Largest request head the mock accepts.
const MAX_HEAD: usize = 64 * 1024;
/// Largest request body the mock accepts.
const MAX_BODY: usize = 64 * 1024 * 1024;
/// How long a stopped server's accept queue must stay empty before it
/// stops accepting: long enough for a handshake already under way to land.
const DRAIN: Duration = Duration::from_millis(50);

/// One request the server received.
#[derive(Clone, Debug)]
pub struct Request {
    /// The request method, as sent.
    pub method: String,
    /// The request target: the path and any query.
    pub path: String,
    /// Header names are lower-cased; of a header sent more than once, the
    /// last value.
    pub headers: BTreeMap<String, String>,
    /// Every header as it was sent, in order, its name in the sender's
    /// letter case: to see a header sent twice, or in another case.
    pub sent_headers: Vec<(String, String)>,
    /// The request body, its framing undone.
    pub body: Vec<u8>,
}

impl Request {
    /// The value of header `name`, matched without regard to case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Every value of header `name` as sent, in order, matched without
    /// regard to case.
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.sent_headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The body as text, invalid UTF-8 replaced.
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body as JSON; fails the test if it is not.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "{} {} sent a body that is not JSON ({e}): {:?}",
                self.method,
                self.path,
                self.body_text()
            )
        })
    }
}

/// What the handler answers.
#[derive(Clone, Debug)]
pub struct Response {
    /// The status code to answer with.
    pub status: u16,
    /// Headers to send, in order, besides `Content-Length` and `Connection`.
    pub headers: Vec<(String, String)>,
    /// The body to send.
    pub body: Vec<u8>,
    reply: Reply,
    /// What happens to the connection after the answer.
    after: After,
}

/// What happens to a connection once an answer is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum After {
    /// Answer `Connection: close`, then close the connection.
    Close,
    /// Answer with no `Connection` header, and keep serving the connection.
    KeepAlive,
    /// Answer with no `Connection` header, then close the connection without
    /// telling the client, as a server does when an idle connection times out.
    SilentClose,
    /// Answer `Connection: close`, but keep the socket open and keep serving
    /// whatever the client sends on it.
    CloseKeepOpen,
}

/// Whether and how the server answers once the request is read.
#[derive(Clone, Copy, Debug)]
enum Reply {
    Answer,
    /// Hold the connection open, silent, until the server is finished.
    Hang,
    /// Close the connection without writing a byte.
    Close,
}

impl Response {
    /// Answer `status` with `body` and no other headers.
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: body.into(),
            reply: Reply::Answer,
            after: After::Close,
        }
    }

    /// Read the request, then never answer: the connection stays open until
    /// the server is finished. A receiver that hangs.
    pub fn hang() -> Response {
        Response {
            reply: Reply::Hang,
            ..Response::new(0, "")
        }
    }

    /// Read the request, then close the connection without a response: a
    /// receiver that resets.
    pub fn close() -> Response {
        Response {
            reply: Reply::Close,
            ..Response::new(0, "")
        }
    }

    /// `status` with `value` as a JSON body.
    pub fn json(status: u16, value: &Value) -> Response {
        Response::new(status, value.to_string()).header("Content-Type", "application/json")
    }

    /// Add a response header.
    pub fn header(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Keep the connection open after this answer, for the client's next
    /// request on it (HTTP/1.1 keep-alive). It closes when the client does,
    /// so drop the client before the server is finished.
    pub fn keep_alive(mut self) -> Response {
        self.after = After::KeepAlive;
        self
    }

    /// Answer with no `Connection` header, then close the connection once the
    /// answer is written. The client is not told, so a client that keeps the
    /// connection finds it closed the next time it uses it. The server counts
    /// these closes in [`MockHttp::closed_silently`].
    pub fn close_silently(mut self) -> Response {
        self.after = After::SilentClose;
        self
    }

    /// Answer `Connection: close` but leave the socket open: a client that
    /// reuses the connection anyway gets its next request answered on it.
    /// The server closes nothing; the client closes its end when it is done.
    pub fn announce_close_keep_open(mut self) -> Response {
        self.after = After::CloseKeepOpen;
        self
    }
}

type Handler = dyn Fn(&Request) -> Response + Send + Sync;
type Connection = JoinHandle<Result<(), String>>;

/// A mock HTTP/1.1 server on a loopback port, answering every request with
/// the handler. Finished (or dropped) at the end of the test, it fails the
/// test if any connection hit an I/O error.
pub struct MockHttp {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    accepted: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<Vec<Connection>>>,
}

impl MockHttp {
    /// Serve on `127.0.0.1:0`.
    pub fn start(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> MockHttp {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind 127.0.0.1:0");
        MockHttp::serve(listener, Arc::new(handler))
    }

    /// Serve on `[::1]:0`, or `None` where the host has no IPv6 loopback:
    /// the test returns early, saying why.
    pub fn start_v6(
        handler: impl Fn(&Request) -> Response + Send + Sync + 'static,
    ) -> Option<MockHttp> {
        match TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) {
            Ok(listener) => Some(MockHttp::serve(listener, Arc::new(handler))),
            Err(e) => {
                eprintln!("skipped: no IPv6 loopback on this host ({e})");
                None
            }
        }
    }

    fn serve(listener: TcpListener, handler: Arc<Handler>) -> MockHttp {
        let addr = listener.local_addr().expect("the listener's address");
        let requests: Arc<Mutex<Vec<Request>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let accepted: Arc<AtomicUsize> = Arc::default();
        let closed: Arc<AtomicUsize> = Arc::default();
        let accept = {
            let (requests, stop, accepted, closed) = (
                requests.clone(),
                stop.clone(),
                accepted.clone(),
                closed.clone(),
            );
            std::thread::spawn(move || {
                let mut connections = Vec::new();
                // Not `incoming()`: stopping must not need a wake-up
                // connection that could be mistaken for a client's. The
                // queue is drained before the stop flag is honoured.
                listener
                    .set_nonblocking(true)
                    .expect("make the listener non-blocking");
                // A client's `connect` can return before its connection is
                // in the accept queue: the handshake ends asynchronously,
                // even on loopback. So once stopped, the queue must stay
                // empty for `DRAIN` before the loop ends, or a client that
                // connected just before `finish` would never be read.
                let mut empty_since: Option<Instant> = None;
                loop {
                    let stream = match listener.accept() {
                        Ok((stream, _)) => {
                            empty_since = None;
                            accepted.fetch_add(1, Ordering::SeqCst);
                            stream.set_nonblocking(false).map(|()| stream)
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            if stop.load(Ordering::SeqCst) {
                                let since = *empty_since.get_or_insert_with(Instant::now);
                                if since.elapsed() >= DRAIN {
                                    break;
                                }
                            }
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(e) => Err(e),
                    };
                    let (handler, requests, stop, closed) = (
                        handler.clone(),
                        requests.clone(),
                        stop.clone(),
                        closed.clone(),
                    );
                    connections.push(std::thread::spawn(move || match stream {
                        Ok(stream) => connection(stream, &*handler, &requests, &stop, &closed),
                        Err(e) => Err(format!("accept: {e}")),
                    }));
                }
                connections
            })
        };
        MockHttp {
            addr,
            requests,
            accepted,
            closed,
            stop,
            accept: Some(accept),
        }
    }

    /// The address the server listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The port the server listens on.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// `http://host:port`, without a trailing slash.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// How many TCP connections the server has accepted so far: one for
    /// every request when none is kept alive.
    pub fn connections(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    /// How many connections the server closed right after an answer sent
    /// without a `Connection` header ([`Response::close_silently`]). The close
    /// is complete (both ends of the socket are dropped) when this counts it,
    /// so a test can wait on it before the client tries the connection again.
    pub fn closed_silently(&self) -> usize {
        self.closed.load(Ordering::SeqCst)
    }

    /// The requests answered so far, oldest first.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock_recovering("mock http requests").clone()
    }

    /// Wait until at least `n` requests were answered; returns them all.
    #[track_caller]
    pub fn await_requests(&self, n: usize) -> Vec<Request> {
        wait::until_value(
            &format!("{n} request(s) to reach the mock server"),
            || self.requests().len(),
            |seen| *seen >= n,
        );
        self.requests()
    }

    /// Stop the server and fail the test if any connection hit an error.
    /// Dropping does the same; call this to fail at a known line.
    #[track_caller]
    pub fn finish(mut self) {
        let errors = self.shutdown();
        assert!(errors.is_empty(), "mock HTTP server: {}", errors.join("; "));
    }

    fn shutdown(&mut self) -> Vec<String> {
        let Some(accept) = self.accept.take() else {
            return Vec::new();
        };
        self.stop.store(true, Ordering::SeqCst);
        let mut errors = Vec::new();
        match accept.join() {
            Ok(connections) => {
                for connection in connections {
                    match connection.join() {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => errors.push(e),
                        Err(panic) => errors.push(format!("handler panicked: {}", payload(&panic))),
                    }
                }
            }
            Err(panic) => errors.push(format!("accept loop panicked: {}", payload(&panic))),
        }
        errors
    }
}

/// A listener that is not an HTTP server: it answers each of the first
/// `connections` connections with `banner` and closes it, as an SSH daemon
/// or a database would. For tests of code that must tell such a port from
/// one of ours.
pub struct Banner {
    addr: SocketAddr,
    thread: Option<JoinHandle<()>>,
}

impl Banner {
    /// Start listening on a free loopback port.
    pub fn start(banner: &'static [u8], connections: usize) -> Banner {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a loopback port");
        let addr = listener.local_addr().expect("the bound address");
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming().take(connections) {
                let Ok(mut stream) = stream else { return };
                branchyard_support::best_effort("write a banner", stream.write_all(banner));
            }
        });
        Banner {
            addr,
            thread: Some(thread),
        }
    }

    /// The address a client connects to.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Wait until the connections have been served.
    pub fn finish(mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the banner thread");
        }
    }
}

impl Drop for MockHttp {
    fn drop(&mut self) {
        let errors = self.shutdown();
        if !errors.is_empty() && !std::thread::panicking() {
            panic!("mock HTTP server: {}", errors.join("; "));
        }
    }
}

fn payload(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "(not a string)".into())
}

fn connection(
    stream: TcpStream,
    handler: &Handler,
    requests: &Mutex<Vec<Request>>,
    stop: &AtomicBool,
    closed: &AtomicUsize,
) -> Result<(), String> {
    // A client that connects and then says nothing must not hang the test.
    stream
        .set_read_timeout(Some(wait::scaled(wait::DEFAULT_TIMEOUT)))
        .map_err(|e| format!("set the read timeout: {e}"))?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| format!("clone: {e}"))?);
    let mut stream = stream;

    // A connection serves requests until one is answered with its socket
    // closed (`After::Close`, `After::SilentClose`), or until the client
    // closes it.
    let mut served = false;
    loop {
        // The request is read with the one HTTP/1.1 codec, so a malformed or
        // cut-off request fails the test instead of being guessed at.
        let head = match branchyard_wire::read_request_head(&mut reader, MAX_HEAD) {
            Ok(Some(head)) => head,
            Ok(None) => return Ok(()),
            // A client that drops a connection while part of a response is still unread resets it
            // (the kernel sends RST, not FIN). On a connection that has already served a request
            // that is the client leaving, not a failure; anywhere else it still fails the test.
            Err(branchyard_wire::WireError::Io { kind, .. })
                if served
                    && matches!(
                        kind,
                        ErrorKind::ConnectionReset
                            | ErrorKind::ConnectionAborted
                            | ErrorKind::BrokenPipe
                    ) =>
            {
                return Ok(());
            }
            Err(e) => return Err(format!("read the request head: {e}")),
        };
        let method = head.method;
        let path = head.target;
        let framing = branchyard_wire::request_framing(&head.headers)
            .map_err(|e| format!("{method} {path}: {e}"))?;
        let sent_headers = head.headers;
        let headers: BTreeMap<String, String> = sent_headers
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
            .collect();
        let expected = match framing {
            branchyard_wire::Framing::Length(n) => format!(" (expected {n} bytes)"),
            _ => String::new(),
        };
        let body = branchyard_wire::read_body(&mut reader, framing, MAX_BODY).map_err(|e| {
            format!("{method} {path}: the body was cut off or malformed{expected}: {e}")
        })?;

        let request = Request {
            method,
            path,
            headers,
            sent_headers,
            body,
        };
        let response = handler(&request);
        requests
            .lock_recovering("mock http requests")
            .push(request.clone());

        match response.reply {
            Reply::Answer => {}
            Reply::Close => return Ok(()),
            Reply::Hang => {
                wait::until("the mock server to be finished", || {
                    stop.load(Ordering::SeqCst)
                });
                return Ok(());
            }
        }

        let connection_header = match response.after {
            After::Close | After::CloseKeepOpen => "Connection: close\r\n",
            After::KeepAlive | After::SilentClose => "",
        };
        let mut head = format!(
            "HTTP/1.1 {} X\r\nContent-Length: {}\r\n{connection_header}",
            response.status,
            response.body.len()
        );
        for (name, value) in &response.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let write = |stream: &mut TcpStream, bytes: &[u8]| {
            stream.write_all(bytes).map_err(|e| {
                format!(
                    "{} {}: write the response: {e}",
                    request.method, request.path
                )
            })
        };
        write(&mut stream, head.as_bytes())?;
        write(&mut stream, &response.body)?;
        stream
            .flush()
            .map_err(|e| format!("{} {}: flush: {e}", request.method, request.path))?;
        match response.after {
            After::Close => return Ok(()),
            After::SilentClose => {
                // Both descriptors must go before the count moves: the close
                // the test waits for is the one the client sees as end of file.
                drop(reader);
                drop(stream);
                closed.fetch_add(1, Ordering::SeqCst);
                return Ok(());
            }
            After::KeepAlive | After::CloseKeepOpen => {}
        }
        served = true;
    }
}

/// A listener held on a free loopback port, for a stand-in that speaks a
/// protocol [`MockHttp`] does not (a tonic gRPC server needs HTTP/2). The
/// port is bound before anyone learns it and stays bound while the caller
/// holds the listener, so nothing races for it. The listener is
/// non-blocking, as `tokio::net::TcpListener::from_std` requires.
pub fn loopback_listener() -> TcpListener {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a loopback port");
    listener
        .set_nonblocking(true)
        .expect("make the listener non-blocking");
    listener
}

#[cfg(test)]
mod loopback_tests {
    use super::*;

    #[test]
    fn a_loopback_listener_holds_its_port_and_does_not_block() {
        let listener = loopback_listener();
        let addr = listener.local_addr().expect("the bound address");
        assert!(addr.ip().is_loopback() && addr.port() != 0);
        assert!(TcpListener::bind(addr).is_err(), "the port stays held");
        let refused = listener.accept().expect_err("no client yet");
        assert_eq!(refused.kind(), std::io::ErrorKind::WouldBlock);
    }
}
