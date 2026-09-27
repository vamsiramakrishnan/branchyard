//! A minimal blocking HTTP/1.1 client: one connection per request, bodies
//! by `Content-Length` or chunked encoding, optional TLS through rustls.
//! Just enough for the Branchyard API; not a general HTTP client.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// Largest response head accepted.
const MAX_HEAD: usize = 64 * 1024;

/// Where the server is: `http://host:port/prefix` or `https://...`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Path prefix without a trailing slash, such as `""` or `/branchyard`.
    pub prefix: String,
}

impl Endpoint {
    pub fn parse(url: &str) -> Result<Endpoint, String> {
        let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(format!("{url:?} is not an http:// or https:// URL"));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.is_empty() || authority.contains('@') {
            return Err(format!("{url:?} has no usable host"));
        }
        if path.contains(['?', '#']) {
            return Err(format!("{url:?} must not have a query or fragment"));
        }
        let default_port = if tls { 443 } else { 80 };
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (host, after) = v6
                .split_once(']')
                .ok_or_else(|| format!("{url:?} has an unterminated IPv6 address"))?;
            let port = match after.strip_prefix(':') {
                Some(port) => parse_port(url, port)?,
                None if after.is_empty() => default_port,
                None => return Err(format!("{url:?} has a malformed host")),
            };
            (host.to_owned(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host.to_owned(), parse_port(url, port)?),
                None => (authority.to_owned(), default_port),
            }
        };
        Ok(Endpoint {
            tls,
            host,
            port,
            prefix: path.trim_end_matches('/').to_owned(),
        })
    }

    fn host_header(&self) -> String {
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

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.tls { "https" } else { "http" };
        write!(f, "{scheme}://{}{}", self.host_header(), self.prefix)
    }
}

fn parse_port(url: &str, port: &str) -> Result<u16, String> {
    port.parse()
        .map_err(|_| format!("{url:?} has an invalid port {port:?}"))
}

/// TLS settings: the Mozilla roots from `webpki-roots`, plus the
/// certificates in `ca_file` when given (for a private CA or a self-signed
/// server certificate).
pub fn tls_config(ca_file: Option<&Path>) -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = ca_file {
        let certs = CertificateDer::pem_file_iter(path)
            .map_err(|e| format!("CA file {}: {e}", path.display()))?;
        let mut added = 0;
        for cert in certs {
            let cert = cert.map_err(|e| format!("CA file {}: {e}", path.display()))?;
            roots
                .add(cert)
                .map_err(|e| format!("CA file {}: {e}", path.display()))?;
            added += 1;
        }
        if added == 0 {
            return Err(format!("CA file {} has no certificates", path.display()));
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
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

/// Open a connection, completing the TLS handshake for `https`.
pub fn connect(
    endpoint: &Endpoint,
    tls: Option<&Arc<ClientConfig>>,
    read_timeout: Duration,
) -> io::Result<Stream> {
    let addrs: Vec<_> = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()?
        .collect();
    let mut last = io::Error::new(io::ErrorKind::NotFound, "the host has no address");
    let mut tcp = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
            Ok(stream) => {
                tcp = Some(stream);
                break;
            }
            Err(e) => last = e,
        }
    }
    let tcp = tcp.ok_or(last)?;
    tcp.set_read_timeout(Some(read_timeout))?;
    tcp.set_write_timeout(Some(Duration::from_secs(30)))?;
    tcp.set_nodelay(true)?;
    if !endpoint.tls {
        return Ok(Stream::Plain(tcp));
    }
    let config = tls.ok_or_else(|| io::Error::other("no TLS configuration"))?;
    let name = ServerName::try_from(endpoint.host.clone())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let connection =
        ClientConnection::new(config.clone(), name).map_err(|e| io::Error::other(e.to_string()))?;
    let mut stream = StreamOwned::new(connection, tcp);
    // Handshake now, so certificate errors surface as connection errors.
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock)?;
    }
    Ok(Stream::Tls(Box::new(stream)))
}

pub struct Request<'a> {
    pub method: &'a str,
    /// Absolute path with query, including the endpoint's prefix.
    pub target: &'a str,
    pub headers: &'a [(&'a str, String)],
    pub body: Option<&'a [u8]>,
}

pub struct Response {
    pub status: u16,
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

    /// The whole body, up to `limit` bytes.
    pub fn read_body(mut self, limit: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        (&mut self.body)
            .take(limit as u64 + 1)
            .read_to_end(&mut out)?;
        if out.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("response body larger than {limit} bytes"),
            ));
        }
        Ok(out)
    }
}

/// Send one request on a fresh connection and read the response head.
pub fn send(
    mut stream: Stream,
    endpoint: &Endpoint,
    request: &Request<'_>,
) -> io::Result<Response> {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: branchyard-client/{}\r\n",
        request.method,
        request.target,
        endpoint.host_header(),
        env!("CARGO_PKG_VERSION")
    );
    for (name, value) in request.headers {
        if value.contains(['\r', '\n']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("header {name} contains a line break"),
            ));
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = request.body {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    if let Some(body) = request.body {
        bytes.extend_from_slice(body);
    }
    stream.write_all(&bytes)?;
    stream.flush()?;
    read_response(BufReader::new(stream), request.method == "HEAD")
}

fn read_response(mut reader: BufReader<Stream>, head_only: bool) -> io::Result<Response> {
    let mut raw = Vec::new();
    loop {
        let before = raw.len();
        let n = reader.read_until(b'\n', &mut raw)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the server closed the connection before responding",
            ));
        }
        if raw.len() > MAX_HEAD {
            return Err(invalid("response head too large"));
        }
        let line = &raw[before..];
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut headers);
    match parsed.parse(&raw) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(invalid("incomplete response head")),
        Err(e) => return Err(invalid(&format!("malformed response head: {e}"))),
    }
    let status = parsed.code.ok_or_else(|| invalid("no status code"))?;
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
        Body::Chunked(Chunked {
            reader,
            remaining: 0,
            done: false,
        })
    } else if let Some(length) = find("content-length") {
        let length: u64 = length
            .parse()
            .map_err(|_| invalid("invalid Content-Length"))?;
        Body::Length(reader.take(length))
    } else {
        Body::Eof(reader)
    };
    Ok(Response {
        status,
        headers,
        body,
    })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

pub enum Body {
    Empty,
    Length(io::Take<BufReader<Stream>>),
    Chunked(Chunked),
    Eof(BufReader<Stream>),
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Body::Empty => Ok(0),
            Body::Length(r) => r.read(buf),
            Body::Chunked(r) => r.read(buf),
            Body::Eof(r) => r.read(buf),
        }
    }
}

/// Decodes `Transfer-Encoding: chunked`, ignoring extensions and trailers.
pub struct Chunked {
    reader: BufReader<Stream>,
    remaining: u64,
    done: bool,
}

impl Read for Chunked {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "chunked body ended early",
                ));
            }
            let size = line.trim().split(';').next().unwrap_or("").trim();
            self.remaining =
                u64::from_str_radix(size, 16).map_err(|_| invalid("invalid chunk size"))?;
            if self.remaining == 0 {
                // Trailers, then the final blank line.
                loop {
                    line.clear();
                    if self.reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf.len().min(self.remaining as usize);
        let n = self.reader.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "chunk ended early",
            ));
        }
        self.remaining -= n as u64;
        if self.remaining == 0 {
            let mut crlf = String::new();
            self.reader.read_line(&mut crlf)?;
        }
        Ok(n)
    }
}

/// Percent-encode one path segment or query value.
pub fn encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_parse() {
        let e = Endpoint::parse("http://127.0.0.1:8421").unwrap();
        assert_eq!(
            (e.tls, e.host.as_str(), e.port, e.prefix.as_str()),
            (false, "127.0.0.1", 8421, "")
        );
        let e = Endpoint::parse("https://by.example/api/").unwrap();
        assert_eq!((e.tls, e.port, e.prefix.as_str()), (true, 443, "/api"));
        assert_eq!(e.to_string(), "https://by.example/api");
        let e = Endpoint::parse("http://[::1]:9000/").unwrap();
        assert_eq!((e.host.as_str(), e.port), ("::1", 9000));
        assert_eq!(e.host_header(), "[::1]:9000");
        for bad in [
            "ftp://x",
            "http://",
            "http://h:port",
            "http://u@h",
            "http://h/?q",
            "http://[::1",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn encoding_keeps_unreserved_bytes() {
        assert_eq!(encode("fix-it_1.2~"), "fix-it_1.2~");
        assert_eq!(encode("a b/c"), "a%20b%2Fc");
    }
}
