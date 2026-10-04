//! The gateway's side toward a backend: one HTTP/1.1 request per
//! connection, over TCP or TLS (rustls with the Mozilla roots), and a
//! response whose body is read as it arrives.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// Largest response head accepted.
const MAX_HEAD: usize = 64 * 1024;
/// How long connecting may take.
const CONNECT: Duration = Duration::from_secs(10);
/// How long a response may go quiet: a model may think for minutes
/// between streamed events.
const QUIET: Duration = Duration::from_secs(600);

/// A backend's base URL, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Its path, without a trailing slash, prefixed to every request's.
    pub prefix: String,
}

impl Target {
    pub fn parse(url: &str) -> Result<Target, String> {
        let tls = match url.split_once("://") {
            Some(("https", _)) => true,
            Some(("http", _)) => false,
            _ => return Err(format!("{url:?} is not an http:// or https:// URL")),
        };
        let not_base = || format!("{url:?} must be a base URL: a host and a path");
        // `url` reads `http:///x` as host `x`; a base URL names its host.
        if url
            .split_once("://")
            .is_some_and(|(_, rest)| rest.starts_with(['/', '\\']))
        {
            return Err(not_base());
        }
        let parsed = url::Url::parse(url).map_err(|e| format!("{url:?} is not a URL: {e}"))?;
        let host = match parsed.host() {
            Some(url::Host::Domain(domain)) => domain.to_owned(),
            Some(url::Host::Ipv4(addr)) => addr.to_string(),
            Some(url::Host::Ipv6(addr)) => addr.to_string(),
            None => return Err(not_base()),
        };
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(not_base());
        }
        Ok(Target {
            tls,
            host,
            port: parsed
                .port_or_known_default()
                .unwrap_or(if tls { 443 } else { 80 }),
            prefix: parsed.path().trim_end_matches('/').to_owned(),
        })
    }

    /// The `Host` header.
    pub fn authority(&self) -> String {
        let host = match self.host.contains(':') {
            true => format!("[{}]", self.host),
            false => self.host.clone(),
        };
        match (self.tls, self.port) {
            (true, 443) | (false, 80) => host,
            _ => format!("{host}:{}", self.port),
        }
    }
}

fn tls() -> Result<Arc<ClientConfig>, String> {
    static CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
                .map_err(|e| format!("TLS: {e}"))
        })
        .clone()
}

pub enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// Why a request got no response: the connection (which the next backend
/// may be tried after), or anything after it was made.
#[derive(Debug)]
pub enum Failure {
    Connect(String),
    Exchange(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Connect(why) => write!(f, "could not connect: {why}"),
            Failure::Exchange(why) => write!(f, "{why}"),
        }
    }
}

fn connect(target: &Target) -> Result<Stream, Failure> {
    let addrs: Vec<_> = (target.host.as_str(), target.port)
        .to_socket_addrs()
        .map_err(|e| Failure::Connect(e.to_string()))?
        .collect();
    let mut last = "the host has no address".to_owned();
    let mut tcp = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT) {
            Ok(stream) => {
                tcp = Some(stream);
                break;
            }
            Err(e) => last = e.to_string(),
        }
    }
    let tcp = tcp.ok_or(Failure::Connect(last))?;
    let setup = |tcp: &TcpStream| -> io::Result<()> {
        tcp.set_read_timeout(Some(QUIET))?;
        tcp.set_write_timeout(Some(Duration::from_secs(60)))?;
        tcp.set_nodelay(true)
    };
    setup(&tcp).map_err(|e| Failure::Connect(e.to_string()))?;
    if !target.tls {
        return Ok(Stream::Plain(tcp));
    }
    let config = tls().map_err(Failure::Connect)?;
    let name =
        ServerName::try_from(target.host.clone()).map_err(|e| Failure::Connect(e.to_string()))?;
    let connection =
        ClientConnection::new(config, name).map_err(|e| Failure::Connect(e.to_string()))?;
    let mut stream = StreamOwned::new(connection, tcp);
    while stream.conn.is_handshaking() {
        stream
            .conn
            .complete_io(&mut stream.sock)
            .map_err(|e| Failure::Connect(format!("TLS: {e}")))?;
    }
    Ok(Stream::Tls(Box::new(stream)))
}

/// A response: its head, and its body as a reader.
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Send `method target` with `headers` and `body` to `target`'s host, and
/// read the response head.
pub fn send(
    target: &Target,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Response, Failure> {
    let mut stream = connect(target)?;
    let mut head = format!(
        "{method} {}{path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        target.prefix,
        target.authority()
    );
    for (name, value) in headers {
        if name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
            return Err(Failure::Exchange(format!("header {name} is malformed")));
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() || !matches!(method, "GET" | "HEAD" | "DELETE" | "OPTIONS") {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    let exchange = |e: io::Error| Failure::Exchange(e.to_string());
    stream.write_all(head.as_bytes()).map_err(exchange)?;
    stream.write_all(body).map_err(exchange)?;
    stream.flush().map_err(exchange)?;
    read_response(BufReader::new(stream), method == "HEAD")
}

fn read_response(mut reader: BufReader<Stream>, head_only: bool) -> Result<Response, Failure> {
    let bad = |why: &str| Failure::Exchange(why.to_owned());
    let mut raw = Vec::new();
    loop {
        let before = raw.len();
        let n = reader
            .read_until(b'\n', &mut raw)
            .map_err(|e| Failure::Exchange(e.to_string()))?;
        if n == 0 {
            return Err(bad("the backend closed the connection before responding"));
        }
        if raw.len() > MAX_HEAD {
            return Err(bad("the backend's response head is too large"));
        }
        let line = &raw[before..];
        if line == b"\r\n" || line == b"\n" {
            // An interim response (100 Continue) comes before the real one.
            if raw.starts_with(b"HTTP/1.1 1") || raw.starts_with(b"HTTP/1.0 1") {
                raw.clear();
                continue;
            }
            break;
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 96];
    let mut parsed = httparse::Response::new(&mut headers);
    match parsed.parse(&raw) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(bad("the backend's response head is malformed")),
    }
    let status = parsed.code.ok_or_else(|| bad("no status code"))?;
    let reason = parsed.reason.unwrap_or("").to_owned();
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
    let find = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let chunked = find("transfer-encoding").is_some_and(|v| {
        v.to_ascii_lowercase()
            .split(',')
            .any(|t| t.trim() == "chunked")
    });
    let body = if head_only || status == 204 || status == 304 {
        Body::Empty
    } else if chunked {
        Body::Chunked {
            reader,
            remaining: 0,
            done: false,
        }
    } else if let Some(length) = find("content-length") {
        let length: u64 = length.parse().map_err(|_| bad("invalid Content-Length"))?;
        Body::Length(reader.take(length))
    } else {
        Body::Eof(reader)
    };
    Ok(Response {
        status,
        reason,
        headers,
        body,
    })
}

/// A response body, its transfer encoding undone.
pub enum Body {
    Empty,
    Length(io::Take<BufReader<Stream>>),
    Chunked {
        reader: BufReader<Stream>,
        remaining: u64,
        done: bool,
    },
    Eof(BufReader<Stream>),
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Body::Empty => Ok(0),
            Body::Length(r) => r.read(buf),
            Body::Eof(r) => r.read(buf),
            Body::Chunked {
                reader,
                remaining,
                done,
            } => {
                if *done || buf.is_empty() {
                    return Ok(0);
                }
                if *remaining == 0 {
                    let mut line = String::new();
                    if reader.read_line(&mut line)? == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "chunked body ended early",
                        ));
                    }
                    let size = line.trim().split(';').next().unwrap_or("").trim();
                    *remaining = u64::from_str_radix(size, 16).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid chunk size")
                    })?;
                    if *remaining == 0 {
                        loop {
                            line.clear();
                            if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                                break;
                            }
                        }
                        *done = true;
                        return Ok(0);
                    }
                }
                let want = buf.len().min(*remaining as usize);
                let n = reader.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "chunk ended early",
                    ));
                }
                *remaining -= n as u64;
                if *remaining == 0 {
                    let mut crlf = String::new();
                    reader.read_line(&mut crlf)?;
                }
                Ok(n)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_parse_into_host_port_and_prefix() {
        let t = Target::parse("https://api.anthropic.com").unwrap();
        assert_eq!(
            (t.tls, t.host.as_str(), t.port, t.prefix.as_str()),
            (true, "api.anthropic.com", 443, "")
        );
        assert_eq!(t.authority(), "api.anthropic.com");
        let t = Target::parse("http://127.0.0.1:8080/proxy/").unwrap();
        assert_eq!((t.port, t.prefix.as_str()), (8080, "/proxy"));
        assert_eq!(t.authority(), "127.0.0.1:8080");
        let t = Target::parse("http://[::1]:9").unwrap();
        assert_eq!(
            (t.host.as_str(), t.authority()),
            ("::1", "[::1]:9".to_owned())
        );
        for bad in [
            "ftp://x",
            "http://",
            "http://u@h",
            "http://h/?q",
            "http://h:x",
        ] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
    }
}
