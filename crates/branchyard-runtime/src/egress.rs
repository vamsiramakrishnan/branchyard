//! [`Proxy`]: an allowlisting HTTP and HTTPS forward proxy for a harness's
//! egress.
//!
//! It speaks the two forms a proxy-aware client sends: `CONNECT host:port`
//! for TLS (and anything else tunnelled), and an absolute URI
//! (`GET http://host/path`) for plain HTTP. Each request's host and port go
//! to the caller's decision; an allowed one is connected to and relayed, a
//! denied one gets `403 Forbidden` with a body saying why. Every decision,
//! either way, goes to the caller's report, once.
//!
//! What it guarantees:
//!
//! - Nothing is connected to before the decision allows it, and a host
//!   name the decision allows is resolved here, on this host: a name that
//!   resolves to a loopback or unspecified address is refused unless the
//!   decision says the rule names such an address itself.
//! - A plain HTTP request goes to the host its URI names, with that host
//!   in its `Host` header and `Connection: close`, so one connection
//!   cannot carry a request for another host. `Proxy-*` headers are
//!   dropped.
//! - Dropping the proxy stops it accepting; connections already relayed
//!   end when either side closes.
//!
//! What it does not guarantee:
//!
//! - Anything about what flows through an allowed tunnel. It never sees
//!   inside TLS: a client that names an allowed host in `CONNECT` and
//!   another in its TLS server name (domain fronting) is not detected.
//! - Anything about a client that ignores the proxy. Whether such a client
//!   can reach the network is the caller's business: see
//!   [`crate::LocalProvider::spawn_confined`].

#![allow(clippy::let_underscore_must_use, clippy::map_unwrap_or)] // ratchet: branchyard-runtime
use branchyard_support::LockExt as _;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Largest request head read before a request is refused.
const HEAD_MAX: usize = 64 * 1024;
/// How long a client may take to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How long connecting to an allowed upstream may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What the caller's decision says about one destination.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Whether the destination may be reached.
    pub allowed: bool,
    /// The rule that allowed it, for the report.
    pub rule: Option<String>,
    /// The rule names a loopback address (or `localhost`) itself, so the
    /// destination may resolve to one.
    pub loopback: bool,
}

/// One request's outcome, as reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// `CONNECT`, or the plain HTTP request's method.
    pub method: String,
    /// The destination host.
    pub host: String,
    /// The destination port.
    pub port: u16,
    /// Whether the request was allowed.
    pub allowed: bool,
    /// The rule that allowed it, when one did.
    pub rule: Option<String>,
    /// Why it was refused, or why an allowed one failed.
    pub reason: Option<String>,
}

/// Decides a destination: `(host, port)`.
pub type Decide = dyn Fn(&str, u16) -> Verdict + Send + Sync;
/// Receives every decision once.
pub type Report = dyn Fn(&Decision) + Send + Sync;

struct Shared {
    decide: Box<Decide>,
    report: Box<Report>,
    stop: AtomicBool,
}

struct Listening {
    /// A handle on the listening socket, to wake its accept.
    socket: TcpListener,
    /// Where it listens, when this host can connect to it.
    local: Option<SocketAddr>,
    thread: Option<JoinHandle<()>>,
}

/// An allowlisting forward proxy. See the module documentation.
pub struct Proxy {
    shared: Arc<Shared>,
    listening: Mutex<Vec<Listening>>,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy").finish_non_exhaustive()
    }
}

impl Proxy {
    /// A proxy that asks `decide` about every destination and tells
    /// `report` every outcome. It serves nothing until given a listener.
    pub fn new(
        decide: impl Fn(&str, u16) -> Verdict + Send + Sync + 'static,
        report: impl Fn(&Decision) + Send + Sync + 'static,
    ) -> Proxy {
        Proxy {
            shared: Arc::new(Shared {
                decide: Box::new(decide),
                report: Box::new(report),
                stop: AtomicBool::new(false),
            }),
            listening: Mutex::new(Vec::new()),
        }
    }

    /// Listen on a new port of this host's loopback and serve it; returns
    /// its address.
    pub fn listen_loopback(&self) -> io::Result<SocketAddr> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let local = listener.local_addr()?;
        self.serve_at(listener, Some(local))?;
        Ok(local)
    }

    /// Serve connections accepted on `listener`, such as one in a confined
    /// process's own network namespace
    /// ([`crate::LocalProvider::spawn_confined`]).
    pub fn serve(&self, listener: TcpListener) -> io::Result<()> {
        self.serve_at(listener, None)
    }

    fn serve_at(&self, listener: TcpListener, local: Option<SocketAddr>) -> io::Result<()> {
        let socket = listener.try_clone()?;
        let shared = self.shared.clone();
        let thread = thread::Builder::new()
            .name("by-egress".into())
            .spawn(move || accept_loop(listener, shared))?;
        self.listening.lock_recovering("listening").push(Listening {
            socket,
            local,
            thread: Some(thread),
        });
        Ok(())
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        let mut listening = self.listening.lock_recovering("listening");
        for listener in listening.iter_mut() {
            wake(listener);
            if let Some(thread) = listener.thread.take() {
                branchyard_support::join_reporting("egress proxy accept", thread);
            }
        }
    }
}

/// Wake a thread blocked in `accept` on `listener`: shutting a listening
/// socket down does that on Linux; connecting to it does it anywhere this
/// host can reach it.
fn wake(listener: &Listening) {
    let _ = rustix::net::shutdown(&listener.socket, rustix::net::Shutdown::Both);
    if let Some(local) = listener.local {
        let _ = TcpStream::connect_timeout(&local, Duration::from_secs(1));
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let accepted = listener.accept();
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        match accepted {
            Ok((stream, _)) => {
                let shared = shared.clone();
                let _ = thread::Builder::new()
                    .name("by-egress-conn".into())
                    .spawn(move || handle(stream, &shared));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if is_transient(&error) => {}
            Err(_) => return,
        }
    }
}

/// Accept errors that concern one connection, not the listener.
fn is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
    )
}

/// A parsed request head.
struct Head {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
}

fn handle(stream: TcpStream, shared: &Shared) {
    let _ = stream.set_read_timeout(Some(HEAD_TIMEOUT));
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let mut client = BufReader::new(stream);
    let mut out = writer;
    let head = match read_head(&mut client) {
        Ok(Some(head)) => head,
        Ok(None) => return,
        Err(why) => {
            let _ = respond(&mut out, 400, "Bad Request", &why);
            return;
        }
    };
    let (host, port, path) = match parse_target(&head) {
        Ok(target) => target,
        Err(why) => {
            let _ = respond(&mut out, 400, "Bad Request", &why);
            return;
        }
    };
    let verdict = (shared.decide)(&host, port);
    let mut decision = Decision {
        method: head.method.clone(),
        host: host.clone(),
        port,
        allowed: verdict.allowed,
        rule: verdict.rule.clone(),
        reason: None,
    };
    if !verdict.allowed {
        decision.reason = Some("no rule allows it".into());
        (shared.report)(&decision);
        let _ = respond(
            &mut out,
            403,
            "Forbidden",
            &format!(
                "Branchyard's egress policy does not allow {} for this branch \
                 (docs/egress.md)",
                authority(&host, port)
            ),
        );
        return;
    }
    let upstream = match connect(&host, port, verdict.loopback) {
        Ok(upstream) => upstream,
        Err(Refused::Policy(why)) => {
            decision.allowed = false;
            decision.reason = Some(why.clone());
            (shared.report)(&decision);
            let _ = respond(
                &mut out,
                403,
                "Forbidden",
                &format!("Branchyard's egress policy does not allow {why}"),
            );
            return;
        }
        Err(Refused::Unreachable(why)) => {
            decision.reason = Some(why.clone());
            (shared.report)(&decision);
            let _ = respond(&mut out, 502, "Bad Gateway", &why);
            return;
        }
    };
    (shared.report)(&decision);
    let _ = client.get_ref().set_read_timeout(None);
    let mut upstream_writer = match upstream.try_clone() {
        Ok(writer) => writer,
        Err(_) => return,
    };
    if head.method == "CONNECT" {
        if out
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .is_err()
        {
            return;
        }
    } else {
        let mut request = format!("{} {} {}\r\n", head.method, path, head.version);
        for (name, value) in &head.headers {
            let lower = name.to_ascii_lowercase();
            if lower.starts_with("proxy-")
                || matches!(lower.as_str(), "host" | "connection" | "keep-alive")
            {
                continue;
            }
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        let default_port = port == 80;
        let host_header = match default_port {
            true => bracketed(&host),
            false => authority(&host, port),
        };
        request.push_str(&format!("Host: {host_header}\r\nConnection: close\r\n\r\n"));
        if upstream_writer.write_all(request.as_bytes()).is_err() {
            return;
        }
    }
    // Whatever the client sent after its head is the upstream's.
    let early = client.buffer().to_vec();
    if !early.is_empty() && upstream_writer.write_all(&early).is_err() {
        return;
    }
    let client_stream = client.into_inner();
    relay(client_stream, out, upstream, upstream_writer);
}

/// Copy both ways until the upstream is done, then close both.
fn relay(
    mut client_read: TcpStream,
    mut client_write: TcpStream,
    mut upstream_read: TcpStream,
    mut upstream_write: TcpStream,
) {
    let up = thread::Builder::new()
        .name("by-egress-up".into())
        .spawn(move || {
            let _ = io::copy(&mut client_read, &mut upstream_write);
            let _ = upstream_write.shutdown(Shutdown::Write);
        });
    let _ = io::copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(Shutdown::Both);
    let _ = upstream_read.shutdown(Shutdown::Both);
    if let Ok(up) = up {
        branchyard_support::join_reporting("egress upstream relay", up);
    }
}

enum Refused {
    /// The policy does not allow where the name leads.
    Policy(String),
    /// It could not be reached.
    Unreachable(String),
}

fn connect(host: &str, port: u16, loopback: bool) -> Result<TcpStream, Refused> {
    let addresses: Vec<SocketAddr> = match (host, port).to_socket_addrs() {
        Ok(addresses) => addresses.collect(),
        Err(error) => {
            return Err(Refused::Unreachable(format!(
                "could not resolve {host}: {error}"
            )))
        }
    };
    let reachable: Vec<SocketAddr> = addresses
        .iter()
        .copied()
        .filter(|a| loopback || !local_only(a.ip()))
        .collect();
    if reachable.is_empty() && !addresses.is_empty() {
        return Err(Refused::Policy(format!(
            "{}: it resolves to a loopback address, which only a rule naming the address \
             reaches",
            authority(host, port)
        )));
    }
    let mut last = None;
    for address in reachable {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(Refused::Unreachable(match last {
        Some(error) => format!("could not connect to {}: {error}", authority(host, port)),
        None => format!("{host} has no address"),
    }))
}

/// Loopback and unspecified addresses: this host's own services.
fn local_only(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    ip.is_loopback() || ip.is_unspecified()
}

fn bracketed(host: &str) -> String {
    match host.contains(':') {
        true => format!("[{host}]"),
        false => host.to_owned(),
    }
}

fn authority(host: &str, port: u16) -> String {
    format!("{}:{port}", bracketed(host))
}

/// Read a request head: `None` when the client closed before sending one.
fn read_head(client: &mut BufReader<TcpStream>) -> Result<Option<Head>, String> {
    let mut lines = Vec::new();
    let mut total = 0;
    loop {
        let mut line = Vec::new();
        let read = client
            .by_ref()
            .take((HEAD_MAX - total + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|e| format!("could not read the request: {e}"))?;
        if read == 0 {
            if lines.is_empty() {
                return Ok(None);
            }
            return Err("the request head ended early".into());
        }
        total += read;
        if total > HEAD_MAX {
            return Err("the request head is too large".into());
        }
        let text = String::from_utf8(line).map_err(|_| "the request head is not UTF-8")?;
        let text = text.trim_end_matches(['\r', '\n']).to_owned();
        if text.is_empty() {
            if lines.is_empty() {
                // Tolerate a blank line before the request line.
                continue;
            }
            break;
        }
        lines.push(text);
    }
    let mut parts = lines[0].split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("the request line is not METHOD TARGET VERSION".into());
    };
    if !version.starts_with("HTTP/1.") {
        return Err(format!("{version} is not HTTP/1"));
    }
    let mut headers = Vec::new();
    for line in &lines[1..] {
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("{line:?} is not a header"));
        };
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    Ok(Some(Head {
        method: method.to_owned(),
        target: target.to_owned(),
        version: version.to_owned(),
        headers,
    }))
}

/// The destination and, for plain HTTP, the origin-form path.
fn parse_target(head: &Head) -> Result<(String, u16, String), String> {
    if head.method == "CONNECT" {
        let (host, port) = split_authority(&head.target)?;
        let port = port.ok_or("CONNECT needs host:port")?;
        return Ok((host, port, String::new()));
    }
    if head.target.starts_with('/') {
        return Err("this is an egress proxy: send an absolute URI or CONNECT".into());
    }
    let rest = match head.target.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
        Some((scheme, _)) => {
            return Err(format!(
                "{scheme} through this proxy needs CONNECT; only http URIs are forwarded"
            ))
        }
        None => return Err("the target is not an absolute http URI".into()),
    };
    let (authority, path) = match rest.find(['/', '?']) {
        Some(at) => (&rest[..at], rest[at..].to_owned()),
        None => (rest, "/".to_owned()),
    };
    let path = match path.starts_with('?') {
        true => format!("/{path}"),
        false => path,
    };
    let (host, port) = split_authority(authority)?;
    Ok((host, port.unwrap_or(80), path))
}

/// `host[:port]` or `[v6][:port]`, without user information.
fn split_authority(authority: &str) -> Result<(String, Option<u16>), String> {
    if authority.contains('@') {
        return Err("user information in the target is not forwarded".into());
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(rest) => {
            let (host, after) = rest.split_once(']').ok_or("an IPv6 host needs a ]")?;
            match after {
                "" => (host, None),
                p => (host, Some(p.strip_prefix(':').ok_or("bad port")?)),
            }
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'_'));
    if !ok {
        return Err(format!("{host:?} is not a host"));
    }
    let port = match port {
        None => None,
        Some(p) => match p.parse::<u16>() {
            Ok(n) if n > 0 => Some(n),
            _ => return Err(format!("{p:?} is not a port")),
        },
    };
    Ok((host, port))
}

fn respond(out: &mut TcpStream, code: u16, reason: &str, body: &str) -> io::Result<()> {
    let body = format!("{body}\n");
    let denied = match code {
        403 => "X-Branchyard-Egress: denied\r\n",
        _ => "",
    };
    let text = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\n{denied}\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    out.write_all(text.as_bytes())?;
    out.flush()?;
    let _ = out.shutdown(Shutdown::Both);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(method: &str, target: &str) -> Head {
        Head {
            method: method.into(),
            target: target.into(),
            version: "HTTP/1.1".into(),
            headers: Vec::new(),
        }
    }

    #[test]
    fn targets_parse_into_host_port_and_path() {
        let parse = |m: &str, t: &str| parse_target(&head(m, t));
        assert_eq!(
            parse("CONNECT", "GitHub.com:443").unwrap(),
            ("github.com".into(), 443, String::new())
        );
        assert_eq!(
            parse("CONNECT", "[::1]:8443").unwrap(),
            ("::1".into(), 8443, String::new())
        );
        assert_eq!(
            parse("GET", "http://example.com/a?b=1").unwrap(),
            ("example.com".into(), 80, "/a?b=1".into())
        );
        assert_eq!(
            parse("GET", "http://example.com:8080").unwrap(),
            ("example.com".into(), 8080, "/".into())
        );
        assert_eq!(
            parse("GET", "HTTP://example.com?x").unwrap(),
            ("example.com".into(), 80, "/?x".into())
        );
        for (method, target, why) in [
            ("CONNECT", "github.com", "host:port"),
            ("CONNECT", "github.com:0", "not a port"),
            ("GET", "/index.html", "absolute URI"),
            ("GET", "https://github.com/", "needs CONNECT"),
            ("GET", "http://user@github.com/", "user information"),
            ("GET", "http://bad host/", "not a host"),
            ("GET", "ftp://x/", "needs CONNECT"),
        ] {
            let error = parse(method, target).unwrap_err();
            assert!(error.contains(why), "{target}: {error}");
        }
    }

    #[test]
    fn loopback_is_this_hosts_own() {
        for ip in [
            "127.0.0.1",
            "127.8.9.1",
            "::1",
            "0.0.0.0",
            "::ffff:127.0.0.1",
        ] {
            assert!(local_only(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["10.0.0.1", "192.168.1.1", "2001:db8::1"] {
            assert!(!local_only(ip.parse().unwrap()), "{ip}");
        }
    }
}
