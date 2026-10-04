//! A minimal blocking HTTP/1.1 client: one connection per request, bodies
//! by `Content-Length` or chunked encoding, optional TLS through rustls,
//! over TCP or, for `unix:/path` (a server's `--listen-unix` socket, such as
//! the one `by --remote ssh://` forwards), a Unix domain socket.
//! Just enough for the Branchyard API; not a general HTTP client.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// Largest response head accepted.
const MAX_HEAD: usize = 64 * 1024;

/// Where the server is: `http://host:port/prefix`, `https://...`, or
/// `unix:/absolute/path/to/socket` (plain HTTP over a Unix domain socket,
/// with `localhost` as the host and no prefix).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Path prefix without a trailing slash, such as `""` or `/branchyard`.
    pub prefix: String,
    /// The Unix domain socket to connect to instead of `host:port`.
    pub unix: Option<PathBuf>,
}

impl Endpoint {
    pub fn parse(url: &str) -> Result<Endpoint, String> {
        if let Some(path) = url.strip_prefix("unix:") {
            let path = Path::new(path);
            if !path.is_absolute() || path.as_os_str().to_string_lossy().contains(['?', '#']) {
                return Err(format!(
                    "{url:?} must name a socket by its absolute path, as unix:/path/to/socket"
                ));
            }
            return Ok(Endpoint {
                tls: false,
                host: "localhost".into(),
                port: 80,
                prefix: String::new(),
                unix: Some(path.to_path_buf()),
            });
        }
        let base = branchyard::models::BaseUrl::parse(url)?;
        Ok(Endpoint {
            tls: base.tls,
            host: base.host,
            port: base.port,
            prefix: base.prefix,
            unix: None,
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
        if let Some(path) = &self.unix {
            return write!(f, "unix:{}", path.display());
        }
        let scheme = if self.tls { "https" } else { "http" };
        write!(f, "{scheme}://{}{}", self.host_header(), self.prefix)
    }
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
    #[cfg(unix)]
    Unix(UnixStream),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
            #[cfg(unix)]
            Stream::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
            #[cfg(unix)]
            Stream::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
            #[cfg(unix)]
            Stream::Unix(s) => s.flush(),
        }
    }
}

/// Open a connection, completing the TLS handshake for `https`.
pub fn connect(
    endpoint: &Endpoint,
    tls: Option<&Arc<ClientConfig>>,
    read_timeout: Duration,
) -> io::Result<Stream> {
    if let Some(path) = &endpoint.unix {
        #[cfg(unix)]
        {
            let stream = UnixStream::connect(path)?;
            stream.set_read_timeout(Some(read_timeout))?;
            stream.set_write_timeout(Some(Duration::from_secs(30)))?;
            return Ok(Stream::Unix(stream));
        }
        #[cfg(not(unix))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{} needs Unix domain sockets", path.display()),
        ));
    }
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

/// Everything but `A-Z a-z 0-9 - . _ ~`.
const COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encode one path segment or query value.
pub fn encode(text: &str) -> String {
    percent_encoding::utf8_percent_encode(text, COMPONENT).to_string()
}

/// Undo [`encode`]: `%XX` escapes become bytes (a trailing `%41` too), a
/// malformed escape (`%zz`, a lone `%4`) stays as written, and bytes that
/// are not UTF-8 become U+FFFD. `+` is not a space; see [`decode_form`].
pub fn decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .into_owned()
}

/// [`decode`] for an `application/x-www-form-urlencoded` query value, where
/// a literal `+` is a space (an encoded one, `%2B`, stays a plus).
pub fn decode_form(text: &str) -> String {
    decode(&text.replace('+', " "))
}

/// [`decode_form`] on bytes, which need not be UTF-8 (a webhook's form body
/// carries a JSON payload as bytes).
pub fn decode_form_bytes(value: &[u8]) -> Vec<u8> {
    let spaced: Vec<u8> = value
        .iter()
        .map(|b| if *b == b'+' { b' ' } else { *b })
        .collect();
    percent_encoding::percent_decode(&spaced).collect()
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
        let e = Endpoint::parse("unix:/run/by/by.sock").unwrap();
        assert_eq!(e.unix.as_deref(), Some(Path::new("/run/by/by.sock")));
        assert_eq!((e.tls, e.host_header().as_str()), (false, "localhost"));
        assert_eq!(e.to_string(), "unix:/run/by/by.sock");
        for bad in ["unix:", "unix:relative/by.sock", "unix:/a?b"] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
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

    /// The decoding cases every percent-decoding call site shares: the
    /// server's query values, OTLP headers and storage routes all go
    /// through `decode`/`decode_form`.
    #[test]
    fn decoding_handles_the_edges() {
        // A trailing escape decodes (the hand-rolled decoders it replaced
        // left `%41` at the end of input as written).
        assert_eq!(decode("%41"), "A");
        assert_eq!(decode("a%41"), "aA");
        // Malformed escapes stay as written.
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("%4"), "%4");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%%41"), "%A");
        // Multi-byte UTF-8, whole and cut.
        assert_eq!(decode("%C3%A9"), "\u{e9}");
        assert_eq!(decode("%E2%82%AC"), "\u{20ac}");
        assert_eq!(decode("%C3"), "\u{fffd}");
        // `+` is a space only in a form value.
        assert_eq!(decode("a+b"), "a+b");
        assert_eq!(decode_form("a+b%2Bc%20d"), "a b+c d");
        assert_eq!(decode_form("%41"), "A");
        assert_eq!(decode_form_bytes(b"a+b%2Bc%FF%41"), b"a b+c\xffA");
    }

    #[test]
    fn encoding_round_trips() {
        for text in [
            "",
            "plain",
            "a b/c?d=e&f",
            "100%",
            "%41",
            "caf\u{e9} \u{20ac} \u{1f600}",
            "+ plus",
            "~-._",
        ] {
            assert_eq!(decode(&encode(text)), text, "{text:?}");
            assert_eq!(decode_form(&encode(text)), text, "{text:?}");
        }
    }
}
