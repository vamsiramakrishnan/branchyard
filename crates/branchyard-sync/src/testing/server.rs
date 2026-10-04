//! A small HTTP/1.1 server on loopback for the stand-ins: one thread per
//! connection, `Content-Length` bodies, `Connection: close`.

use branchyard_support::LockExt as _;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::http::{Request, Url};
use crate::util::{query_pairs, uri_decode};

/// A received request.
#[derive(Clone, Debug)]
pub struct MockRequest {
    pub method: String,
    /// The path as sent (still percent-encoded).
    pub path: String,
    /// The query as sent, without `?`.
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl MockRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// A query parameter, decoded.
    pub fn param(&self, name: &str) -> Option<String> {
        query_pairs(&self.query)
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    pub fn has_param(&self, name: &str) -> bool {
        query_pairs(&self.query).iter().any(|(k, _)| k == name)
    }

    /// The decoded path.
    pub fn decoded_path(&self) -> String {
        uri_decode(&self.path, false)
    }

    /// As the client's request type, for checking signatures.
    pub fn as_request(&self) -> Request {
        let host = self.header("host").unwrap_or("localhost").to_owned();
        let (h, port) = match host.rsplit_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse().unwrap_or(80)),
            None => (host.clone(), 80),
        };
        Request {
            method: self.method.clone(),
            url: Url {
                tls: false,
                host: h,
                port,
                path: self.path.clone(),
                query: self.query.clone(),
            },
            headers: self
                .headers
                .iter()
                .filter(|(n, _)| {
                    !n.eq_ignore_ascii_case("content-length")
                        && !n.eq_ignore_ascii_case("user-agent")
                        && !n.eq_ignore_ascii_case("connection")
                })
                .cloned()
                .collect(),
            body: (!self.body.is_empty()).then(|| self.body.clone()),
        }
    }

    pub fn line(&self) -> String {
        match self.query.is_empty() {
            true => format!("{} {}", self.method, self.path),
            false => format!("{} {}?{}", self.method, self.path, self.query),
        }
    }
}

/// A response to send.
#[derive(Clone, Debug, Default)]
pub struct MockResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl MockResponse {
    pub fn new(status: u16) -> MockResponse {
        MockResponse {
            status,
            ..MockResponse::default()
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> MockResponse {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> MockResponse {
        self.body = body.into();
        self
    }

    pub fn json(status: u16, value: &serde_json::Value) -> MockResponse {
        MockResponse::new(status)
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(value).unwrap_or_default())
    }

    pub fn xml(status: u16, text: String) -> MockResponse {
        MockResponse::new(status)
            .header("Content-Type", "application/xml")
            .body(text.into_bytes())
    }
}

/// Requests made to fail on purpose: the next `times` requests whose
/// method matches and whose line contains `contains`, after `skip` such
/// requests pass.
#[derive(Default)]
pub struct Faults {
    rules: Mutex<VecDeque<Rule>>,
}

struct Rule {
    method: String,
    contains: String,
    status: u16,
    skip: usize,
    times: usize,
}

impl Faults {
    pub fn fail(&self, method: &str, contains: &str, status: u16, skip: usize, times: usize) {
        self.rules.lock_recovering("rules").push_back(Rule {
            method: method.to_owned(),
            contains: contains.to_owned(),
            status,
            skip,
            times,
        });
    }

    pub fn clear(&self) {
        self.rules.lock_recovering("rules").clear();
    }

    /// The injected failure for `request`, if one applies.
    pub fn check(&self, request: &MockRequest) -> Option<MockResponse> {
        let mut rules = self.rules.lock_recovering("rules");
        let line = request.line();
        for rule in rules.iter_mut() {
            if rule.times == 0
                || !(rule.method == "*" || rule.method == request.method)
                || !line.contains(&rule.contains)
            {
                continue;
            }
            if rule.skip > 0 {
                rule.skip -= 1;
                return None;
            }
            rule.times -= 1;
            return Some(
                MockResponse::new(rule.status).body(format!("injected failure {}", rule.status)),
            );
        }
        None
    }
}

pub type Handler = Arc<dyn Fn(&MockRequest) -> MockResponse + Send + Sync>;

/// A server running until dropped.
pub struct MockServer {
    pub url: String,
    stop: Arc<AtomicBool>,
    port: u16,
    thread: Option<JoinHandle<()>>,
    /// Every request line, in order.
    pub log: Arc<Mutex<Vec<String>>>,
}

impl MockServer {
    pub fn start(handler: Handler) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let port = listener.local_addr().expect("local address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let log = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let (stop, log) = (stop.clone(), log.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let (handler, log) = (handler.clone(), log.clone());
                    std::thread::spawn(move || serve(stream, &handler, &log));
                }
            })
        };
        MockServer {
            url: format!("http://127.0.0.1:{port}"),
            stop,
            port,
            thread: Some(thread),
            log,
        }
    }

    pub fn requests(&self) -> Vec<String> {
        self.log.lock_recovering("log").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(t) = self.thread.take() {
            branchyard_support::join_reporting("mock server", t);
        }
    }
}

fn serve(stream: TcpStream, handler: &Handler, log: &Mutex<Vec<String>>) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut head = Vec::new();
    loop {
        let before = head.len();
        match reader.read_until(b'\n', &mut head) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line = &head[before..];
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    if !matches!(parsed.parse(&head), Ok(httparse::Status::Complete(_))) {
        return;
    }
    let method = parsed.method.unwrap_or("GET").to_owned();
    let target = parsed.path.unwrap_or("/").to_owned();
    let headers: Vec<(String, String)> = parsed
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_owned(),
                String::from_utf8_lossy(h.value).trim().to_owned(),
            )
        })
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (target.clone(), String::new()),
    };
    let request = MockRequest {
        method,
        path,
        query,
        headers,
        body,
    };
    log.lock_recovering("log").push(request.line());
    let response = handler(&request);
    let mut out = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", response.status);
    let mut has_length = false;
    for (name, value) in &response.headers {
        has_length |= name.eq_ignore_ascii_case("content-length");
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if !has_length {
        out.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    if request.method != "HEAD" {
        bytes.extend_from_slice(&response.body);
    }
    let mut stream = stream;
    let _ = stream.write_all(&bytes);
    let _ = stream.flush();
}
