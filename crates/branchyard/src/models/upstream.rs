//! The gateway's side toward a backend: one HTTP/1.1 request per
//! connection, over TCP or TLS (rustls with the Mozilla roots), and a
//! response whose body is read as it arrives.

use std::io::{self, BufReader, Read, Write};

use branchyard_wire as wire;
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
    /// `https` (true) or `http` (false).
    pub tls: bool,
    /// The host, without the brackets of an IPv6 literal.
    pub host: String,
    /// The port, the scheme's default when none is written.
    pub port: u16,
    /// Its path, without a trailing slash, prefixed to every request's.
    pub prefix: String,
}

impl Target {
    /// Parse a backend's base URL: `http` or `https`, a host, an optional port
    /// and path, and no query, fragment or userinfo.
    pub fn parse(url: &str) -> Result<Target, String> {
        let url = wire::HttpUrl::parse(url)
            .and_then(wire::HttpUrl::without_query)
            .map_err(|e| e.to_string())?;
        Ok(Target {
            tls: url.tls,
            host: url.host.clone(),
            port: url.port,
            prefix: url.prefix().to_owned(),
        })
    }

    /// The `Host` header.
    pub fn authority(&self) -> String {
        wire::host_header(self.tls, &self.host, self.port)
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
    let exchange = |e: io::Error| Failure::Exchange(e.to_string());
    let authority = target.authority();
    let line = format!("{}{path}", target.prefix);
    let content_length = (!body.is_empty()
        || !matches!(method, "GET" | "HEAD" | "DELETE" | "OPTIONS"))
    .then_some(body.len() as u64);
    let head = wire::request_head(
        method,
        &line,
        [("Host", authority.as_str()), ("Connection", "close")]
            .into_iter()
            .chain(headers.iter().map(|(n, v)| (n.as_str(), v.as_str()))),
        content_length,
    )
    .map_err(|e| Failure::Exchange(e.to_string()))?;
    stream.write_all(&head).map_err(exchange)?;
    stream.write_all(body).map_err(exchange)?;
    stream.flush().map_err(exchange)?;
    read_response(BufReader::new(stream), method == "HEAD")
}

fn read_response(mut reader: BufReader<Stream>, head_only: bool) -> Result<Response, Failure> {
    let bad = |e: wire::WireError| match e {
        wire::WireError::ConnectionClosed => {
            Failure::Exchange("the backend closed the connection before responding".into())
        }
        e => Failure::Exchange(format!("the backend's response is malformed: {e}")),
    };
    let head = wire::read_response_head(&mut reader, MAX_HEAD).map_err(bad)?;
    let framing = wire::response_framing(head.status, &head.headers, head_only).map_err(bad)?;
    Ok(Response {
        status: head.status,
        reason: head.reason,
        headers: head.headers,
        body: Body::new(reader, framing),
    })
}

/// A response body, its transfer encoding undone.
pub type Body = wire::Body<BufReader<Stream>>;

#[allow(clippy::let_underscore_must_use, clippy::unwrap_in_result)] // tests: a panic is the failure report
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

    /// A backend that sends `bytes` and closes, read through `read_response`.
    fn answer(bytes: &[u8], head_only: bool) -> Result<(u16, Vec<u8>), String> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut backend = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (client, _) = listener.accept().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let _ = backend.write_all(bytes);
        drop(backend);
        let mut response = read_response(BufReader::new(Stream::Plain(client)), head_only)
            .map_err(|e| e.to_string())?;
        let mut body = Vec::new();
        response
            .body
            .read_to_end(&mut body)
            .map_err(|e| e.to_string())?;
        Ok((response.status, body))
    }

    #[test]
    fn every_valid_response_in_the_wire_corpus_is_read() {
        for (name, bytes, body) in wire::corpus::responses_valid() {
            let got = answer(&bytes, false).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(got.1, body, "{name}");
        }
    }

    #[test]
    fn every_malformed_response_in_the_wire_corpus_fails_cleanly() {
        for (name, bytes) in wire::corpus::responses_malformed() {
            let got = answer(&bytes, false);
            assert!(got.is_err(), "{name}: read as {got:?}");
        }
    }

    #[test]
    fn a_response_cut_off_mid_chunk_is_an_error_not_a_short_body() {
        let got = answer(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n5\r\nhe",
            false,
        );
        assert!(got.is_err(), "{got:?}");
    }

    #[test]
    fn a_head_request_has_no_body_whatever_the_head_says() {
        let got = answer(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n", true);
        assert_eq!(got, Ok((200, Vec::new())));
    }
}
