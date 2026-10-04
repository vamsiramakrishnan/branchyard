//! What the egress tests share: local listeners, a probe the fake agent
//! runs, and the egress events of a branch.

#![allow(dead_code)]
#![allow(clippy::let_underscore_must_use, clippy::unwrap_used)] // tests: a panic is the failure report
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use branchyard::{Activity, EgressActivity, EgressEnforcement, RecordedEvent};

/// A listener on this host's loopback that answers every request with
/// `upstream <port>` and sends the request line to the test.
pub struct Upstream {
    pub port: u16,
    pub requests: Receiver<String>,
}

pub fn upstream() -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, requests) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let sender = sender.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let body = format!("upstream {port}\n");
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sender.send(first.trim_end().to_owned());
            });
        }
    });
    Upstream { port, requests }
}

/// `probe.py proxy PORT` fetches `http://127.0.0.1:PORT/x` the way a
/// proxy-aware tool does (through the proxy variables) and prints
/// `proxy PORT: <body or status>`; `probe.py direct PORT` connects without
/// the proxy and prints `direct PORT: reached|blocked`.
pub fn probe(dir: &Path) -> PathBuf {
    let path = dir.join("probe.py");
    std::fs::write(
        &path,
        r#"import socket, sys, urllib.error, urllib.request
mode, port = sys.argv[1], int(sys.argv[2])
if mode == "proxy":
    try:
        body = urllib.request.urlopen(f"http://127.0.0.1:{port}/x", timeout=10).read()
        print(f"proxy {port}: {body.decode().strip()}")
    except urllib.error.HTTPError as error:
        print(f"proxy {port}: {error.code}")
    except OSError as error:
        print(f"proxy {port}: failed {error}")
else:
    try:
        socket.create_connection(("127.0.0.1", port), timeout=5).close()
        print(f"direct {port}: reached")
    except OSError:
        print(f"direct {port}: blocked")
"#,
    )
    .unwrap();
    path
}

/// The prompt that runs the probe once per `(mode, port)`, then prints
/// the proxy variables.
pub fn prompt(probe: &Path, runs: &[(&str, u16)]) -> String {
    let mut text = String::new();
    for (mode, port) in runs {
        text.push_str(&format!("SH python3 {} {mode} {port}\n", probe.display()));
    }
    text.push_str("SH echo vars=$HTTPS_PROXY,$http_proxy,$ALL_PROXY,[$NO_PROXY]");
    text
}

/// The turn's `applied` events: how, and why not enforced.
pub fn applied(events: &[RecordedEvent]) -> Vec<(EgressEnforcement, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Egress(egress) => match egress.as_ref() {
                EgressActivity::Applied {
                    enforcement,
                    reason,
                    ..
                } => Some((*enforcement, reason.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Every decision: `(method, port, allowed, rule)`.
pub fn decisions(events: &[RecordedEvent]) -> Vec<(String, u16, bool, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Egress(egress) => match egress.as_ref() {
                EgressActivity::Decision {
                    method,
                    port,
                    allowed,
                    rule,
                    ..
                } => Some((method.clone(), *port, *allowed, rule.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}
