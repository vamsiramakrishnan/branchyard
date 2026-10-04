//! A minimal blocking HTTP/1.1 client: one connection per request, heads,
//! framing, chunked decoding and URLs from `branchyard-wire` (the
//! workspace's one codec), optional TLS through rustls,
//! over TCP or, for `unix:/path` (a server's `--listen-unix` socket, such as
//! the one `by --remote ssh://` forwards), a Unix domain socket.
//! Just enough for the Branchyard API; not a general HTTP client.

use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use branchyard_wire as wire;
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
        let url = wire::HttpUrl::parse(url)
            .and_then(wire::HttpUrl::without_query)
            .map_err(|e| e.to_string())?;
        Ok(Endpoint {
            tls: url.tls,
            prefix: url.prefix().to_owned(),
            host: url.host,
            port: url.port,
            unix: None,
        })
    }

    fn host_header(&self) -> String {
        wire::host_header(self.tls, &self.host, self.port)
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
    let host = endpoint.host_header();
    let agent = concat!("branchyard-client/", env!("CARGO_PKG_VERSION"));
    let head = wire::request_head(
        request.method,
        request.target,
        [
            ("Host", host.as_str()),
            ("Connection", "close"),
            ("User-Agent", agent),
        ]
        .into_iter()
        .chain(request.headers.iter().map(|(n, v)| (*n, v.as_str()))),
        request.body.map(|body| body.len() as u64),
    )?;
    let mut bytes = head;
    if let Some(body) = request.body {
        bytes.extend_from_slice(body);
    }
    stream.write_all(&bytes)?;
    stream.flush()?;
    read_response(BufReader::new(stream), request.method == "HEAD")
}

fn read_response(mut reader: BufReader<Stream>, head_only: bool) -> io::Result<Response> {
    let head = wire::read_response_head(&mut reader, MAX_HEAD)?;
    let framing = wire::response_framing(head.status, &head.headers, head_only)?;
    Ok(Response {
        status: head.status,
        headers: head.headers,
        body: Body::new(reader, framing),
    })
}

/// A response body, its framing undone by the wire codec.
pub type Body = wire::Body<BufReader<Stream>>;

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

    /// A server that sends `bytes` and closes, read through `read_response`.
    fn answer(bytes: &[u8], head_only: bool) -> io::Result<(u16, Vec<u8>)> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut server = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (client, _) = listener.accept().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let _ = server.write_all(bytes);
        drop(server);
        let mut response = read_response(BufReader::new(Stream::Plain(client)), head_only)?;
        let mut body = Vec::new();
        response.body.read_to_end(&mut body)?;
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
    fn a_framing_failure_carries_its_typed_cause() {
        let error = answer(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            false,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(matches!(
            wire::wire_error(&error),
            Some(wire::WireError::BadChunkSize(_))
        ));
        let error = answer(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabc", false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_request_head_refuses_what_could_split_it() {
        let headers = [("X-Evil", "a\r\nInjected: 1".to_owned())];
        let request = Request {
            method: "GET",
            target: "/",
            headers: &headers,
            body: None,
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let endpoint = Endpoint::parse("http://127.0.0.1:1").unwrap();
        let Err(error) = send(Stream::Plain(tcp), &endpoint, &request) else {
            panic!("a header with a line break was sent");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
