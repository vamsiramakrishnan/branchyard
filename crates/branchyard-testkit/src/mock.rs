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

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use branchyard_support::LockExt;

use serde_json::Value;

use crate::wait;

/// One request the server received.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Header names are lower-cased.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

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
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    reply: Reply,
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
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: body.into(),
            reply: Reply::Answer,
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

    pub fn header(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.to_owned(), value.to_owned()));
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
        let accept = {
            let (requests, stop) = (requests.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut connections = Vec::new();
                // Not `incoming()`: stopping must not need a wake-up
                // connection that could be mistaken for a client's. The
                // queue is drained before the stop flag is honoured.
                listener
                    .set_nonblocking(true)
                    .expect("make the listener non-blocking");
                loop {
                    let stream = match listener.accept() {
                        Ok((stream, _)) => stream.set_nonblocking(false).map(|()| stream),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            if stop.load(Ordering::SeqCst) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(e) => Err(e),
                    };
                    let (handler, requests, stop) =
                        (handler.clone(), requests.clone(), stop.clone());
                    connections.push(std::thread::spawn(move || match stream {
                        Ok(stream) => connection(stream, &*handler, &requests, &stop),
                        Err(e) => Err(format!("accept: {e}")),
                    }));
                }
                connections
            })
        };
        MockHttp {
            addr,
            requests,
            stop,
            accept: Some(accept),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// `http://host:port`, without a trailing slash.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
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
) -> Result<(), String> {
    // A client that connects and then says nothing must not hang the test.
    stream
        .set_read_timeout(Some(wait::scaled(wait::DEFAULT_TIMEOUT)))
        .map_err(|e| format!("set the read timeout: {e}"))?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| format!("clone: {e}"))?);
    let mut stream = stream;

    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .map_err(|e| format!("read the request line: {e}"))?;
    if n == 0 {
        return Ok(());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    if method.is_empty() || path.is_empty() {
        return Err(format!("malformed request line {line:?}"));
    }

    let mut headers = BTreeMap::new();
    loop {
        let mut header = String::new();
        let n = reader
            .read_line(&mut header)
            .map_err(|e| format!("{method} {path}: read a header: {e}"))?;
        if n == 0 {
            return Err(format!("{method} {path}: the headers were cut off"));
        }
        if header.trim().is_empty() {
            break;
        }
        let (name, value) = header
            .trim_end()
            .split_once(':')
            .ok_or_else(|| format!("{method} {path}: malformed header {header:?}"))?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }

    let length: usize = match headers.get("content-length") {
        None => 0,
        Some(text) => text
            .parse()
            .map_err(|_| format!("{method} {path}: bad content-length {text:?}"))?,
    };
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).map_err(|e| {
        format!("{method} {path}: the body was cut off (expected {length} bytes): {e}")
    })?;

    let request = Request {
        method,
        path,
        headers,
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

    let mut head = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
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
        .map_err(|e| format!("{} {}: flush: {e}", request.method, request.path))
}
