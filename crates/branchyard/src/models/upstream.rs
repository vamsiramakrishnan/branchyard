//! The gateway's side toward a backend: one HTTP/1.1 request per
//! connection, over TCP or TLS (rustls with the Mozilla roots), and a
//! response whose body is read as it arrives. A [`Transport`] carries them
//! over the operator's HTTP proxy and root certificates when the
//! environment names them (`HTTPS_PROXY`, `SSL_CERT_FILE`): the gateway's
//! calls to its backends and the requests `by models list` makes alike.

use std::io::{self, BufReader, Read, Write};

use branchyard_wire as wire;
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// Largest response head accepted.
const MAX_HEAD: usize = 64 * 1024;
/// How long connecting may take.
const CONNECT: Duration = Duration::from_secs(10);
/// How long a response may go quiet: a model may think for minutes
/// between streamed events.
pub(crate) const QUIET: Duration = Duration::from_secs(600);

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

/// TLS trusting `roots` (and offering no client certificate).
fn client_config(roots: RootCertStore) -> Result<Arc<ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
        .map_err(|e| format!("TLS: {e}"))
}

fn tls() -> Result<Arc<ClientConfig>, String> {
    static CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            client_config(roots)
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

/// A TCP connection to `host:port`, answered within `read_timeout` per read.
fn dial(host: &str, port: u16, read_timeout: Duration) -> Result<TcpStream, Failure> {
    let addrs: Vec<_> = (host, port)
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
        tcp.set_read_timeout(Some(read_timeout))?;
        tcp.set_write_timeout(Some(Duration::from_secs(60)))?;
        tcp.set_nodelay(true)
    };
    setup(&tcp).map_err(|e| Failure::Connect(e.to_string()))?;
    Ok(tcp)
}

/// TLS to `host` over `tcp`, handshaken now so that a certificate the
/// roots do not vouch for is a connection error.
fn handshake(tcp: TcpStream, host: &str, config: Arc<ClientConfig>) -> Result<Stream, Failure> {
    let name =
        ServerName::try_from(host.to_owned()).map_err(|e| Failure::Connect(e.to_string()))?;
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

/// A direct connection to `target`: TCP, or TLS trusting the Mozilla roots.
fn connect(target: &Target, quiet: Duration) -> Result<Stream, Failure> {
    let tcp = dial(&target.host, target.port, quiet)?;
    if !target.tls {
        return Ok(Stream::Plain(tcp));
    }
    handshake(tcp, &target.host, tls().map_err(Failure::Connect)?)
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

/// Send `method target` with `headers` and `body` to `target`'s host over a
/// direct connection, and read the response head. For the effect ledger's
/// calls to MCP servers and proxies; the gateway's calls to its backends go
/// through a [`Transport`].
pub fn send(
    target: &Target,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Response, Failure> {
    send_within(target, method, path, headers, body, QUIET)
}

/// [`send`], giving up when the connection goes quiet for `quiet` (a read
/// that waits that long): for asking a port what it is, where a listener
/// that never answers must not hold the caller for [`QUIET`].
pub fn send_within(
    target: &Target,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
    quiet: Duration,
) -> Result<Response, Failure> {
    exchange(connect(target, quiet)?, target, method, path, headers, body)
}

/// Write the request on `stream`, and read the response head.
fn exchange(
    mut stream: Stream,
    target: &Target,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Response, Failure> {
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

/// The variables that name a proxy for `https` hosts, in order.
const PROXY_VARIABLES: [&str; 4] = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"];
/// The variables that name the hosts to reach without it.
const NO_PROXY_VARIABLES: [&str; 2] = ["NO_PROXY", "no_proxy"];
/// The variables that name a bundle of root certificates to trust beside
/// the Mozilla roots, in order (the first of
/// `crate::egress::TRUST_VARIABLES` a process is likely to have set).
const ROOT_VARIABLES: [&str; 3] = ["SSL_CERT_FILE", "CURL_CA_BUNDLE", "REQUESTS_CA_BUNDLE"];

/// An HTTP proxy to tunnel `https` through.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Proxy {
    host: String,
    port: u16,
    /// `Basic ...`, for a proxy URL that carries a user and password.
    authorization: Option<String>,
}

/// Read a proxy's address: `http://[user:password@]host[:port]` (or
/// without the scheme). An error never quotes the user or password.
fn parse_proxy(text: &str) -> Result<Proxy, String> {
    let text = text.trim();
    let rest = match text.split_once("://") {
        Some(("http", rest)) => rest,
        Some((scheme, _)) => {
            return Err(format!(
                "the proxy must be an http:// URL, not {scheme}:// (HTTPS_PROXY)"
            ))
        }
        None => text,
    };
    let (userinfo, address) = match rest.rsplit_once('@') {
        Some((userinfo, address)) => (Some(userinfo), address),
        None => (None, rest),
    };
    let address = address.trim_end_matches('/');
    let target = Target::parse(&format!("http://{address}"))
        .map_err(|e| format!("the proxy address (HTTPS_PROXY): {e}"))?;
    let authorization = userinfo.map(|userinfo| {
        let decoded = percent_encoding::percent_decode_str(userinfo).collect::<Vec<u8>>();
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(decoded)
        )
    });
    Ok(Proxy {
        host: target.host,
        port: target.port,
        authorization,
    })
}

/// Whether `host:port` is reached without the proxy: loopback always, and
/// whatever `NO_PROXY` lists (`*`, a host, a domain with or without a
/// leading dot, each with an optional `:port`; address ranges are not
/// read).
fn bypasses(no_proxy: &[String], host: &str, port: u16) -> bool {
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
        return true;
    }
    no_proxy.iter().any(|entry| {
        if entry == "*" {
            return true;
        }
        let (name, wanted) = match entry.rsplit_once(':') {
            Some((name, digits))
                if !name.contains(':')
                    && !digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit()) =>
            {
                (name, digits.parse::<u16>().ok())
            }
            _ => (entry.as_str(), None),
        };
        if wanted.is_some_and(|wanted| wanted != port) {
            return false;
        }
        let name = name.trim_start_matches("*.").trim_start_matches('.');
        !name.is_empty() && (host == name || host.ends_with(&format!(".{name}")))
    })
}

/// The Mozilla roots and every certificate in `pem` that parses.
fn tls_trusting(pem: &str) -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let certs = CertificateDer::pem_slice_iter(pem.as_bytes()).filter_map(Result::ok);
    let (added, _) = roots.add_parsable_certificates(certs);
    if added == 0 {
        return Err("the bundle holds no certificate".to_owned());
    }
    client_config(roots)
}

/// A `CONNECT` tunnel to `target` through `proxy`: the proxy's connection
/// once it has answered `2xx`, ready for the TLS handshake.
fn tunnel(proxy: &Proxy, target: &Target, timeout: Duration) -> Result<TcpStream, Failure> {
    let refused =
        |why: String| Failure::Connect(format!("the proxy {}:{}: {why}", proxy.host, proxy.port));
    let tcp = dial(&proxy.host, proxy.port, timeout).map_err(|e| match e {
        Failure::Connect(why) | Failure::Exchange(why) => refused(why),
    })?;
    let authority = match target.host.contains(':') {
        true => format!("[{}]:{}", target.host, target.port),
        false => format!("{}:{}", target.host, target.port),
    };
    let mut headers = vec![("Host", authority.as_str())];
    if let Some(authorization) = &proxy.authorization {
        headers.push(("Proxy-Authorization", authorization.as_str()));
    }
    let head = wire::request_head("CONNECT", &authority, headers, None)
        .map_err(|e| refused(e.to_string()))?;
    let mut writer = &tcp;
    writer
        .write_all(&head)
        .and_then(|()| writer.flush())
        .map_err(|e| refused(e.to_string()))?;
    let mut reader = BufReader::new(tcp.try_clone().map_err(|e| refused(e.to_string()))?);
    let answer = wire::read_response_head(&mut reader, MAX_HEAD)
        .map_err(|e| refused(format!("no answer to CONNECT {authority}: {e}")))?;
    if !(200..300).contains(&answer.status) {
        return Err(refused(format!(
            "answered {} to CONNECT {authority}",
            answer.status
        )));
    }
    if !reader.buffer().is_empty() {
        return Err(refused("sent data before the TLS handshake".to_owned()));
    }
    Ok(tcp)
}

/// What a [`Transport`] was set up with, or why it could not be.
#[derive(Clone)]
struct Setup {
    proxy: Option<Proxy>,
    no_proxy: Vec<String>,
    tls: Arc<ClientConfig>,
}

/// How a request gets to a provider's host: through the operator's HTTP
/// proxy for `https` hosts when `HTTPS_PROXY` names one (a `CONNECT`
/// tunnel; `NO_PROXY` and loopback are reached directly), trusting the
/// bundle `SSL_CERT_FILE` names beside the Mozilla roots. The gateway's
/// calls to its backends go this way ([`Transport::from_env`], read once
/// when the yard's gateway is built), as do the calls the operator makes
/// by hand, such as `by models list`. A problem with the proxy or the
/// bundle fails the `https` requests it would have affected, and no
/// others.
#[derive(Clone)]
pub struct Transport {
    setup: Result<Setup, String>,
    /// How long one read may wait.
    timeout: Duration,
}

impl Transport {
    /// A transport through `proxy` (an `http://` URL), not using it for
    /// the hosts `no_proxy` lists (comma separated), trusting the
    /// certificates in `roots_pem` too.
    pub fn new(
        proxy: Option<&str>,
        no_proxy: &str,
        roots_pem: Option<&str>,
        timeout: Duration,
    ) -> Transport {
        let roots = match roots_pem {
            Some(pem) => tls_trusting(pem),
            None => tls(),
        };
        Transport::assemble(proxy, no_proxy, roots, timeout)
    }

    /// A transport whose TLS settings are `roots`, or why there are none.
    fn assemble(
        proxy: Option<&str>,
        no_proxy: &str,
        roots: Result<Arc<ClientConfig>, String>,
        timeout: Duration,
    ) -> Transport {
        let setup = roots.and_then(|tls| {
            Ok(Setup {
                proxy: proxy.map(parse_proxy).transpose()?,
                no_proxy: no_proxy
                    .split(',')
                    .map(|entry| entry.trim().to_ascii_lowercase())
                    .filter(|entry| !entry.is_empty())
                    .collect(),
                tls,
            })
        });
        Transport { setup, timeout }
    }

    /// Direct connections trusting the Mozilla roots only.
    pub fn direct(timeout: Duration) -> Transport {
        Transport::new(None, "", None, timeout)
    }

    /// As this process's environment says: `HTTPS_PROXY` (else
    /// `https_proxy`, `ALL_PROXY`), `NO_PROXY`, and the bundle `SSL_CERT_FILE`
    /// (else `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`) names.
    pub fn from_env(timeout: Duration) -> Transport {
        let first = |names: &[&str]| {
            names
                .iter()
                .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
        };
        let proxy = first(&PROXY_VARIABLES);
        let no_proxy = first(&NO_PROXY_VARIABLES).unwrap_or_default();
        let named = ROOT_VARIABLES.iter().find_map(|name| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(|path| (*name, std::path::PathBuf::from(path)))
        });
        let roots = match named {
            None => tls(),
            Some((name, path)) => std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|pem| tls_trusting(&pem))
                .map_err(|why| format!("{name} names {}: {why}", path.display())),
        };
        Transport::assemble(proxy.as_deref(), &no_proxy, roots, timeout)
    }

    /// Whether requests to `target` would go through a proxy.
    pub fn proxied(&self, target: &Target) -> bool {
        match &self.setup {
            Ok(setup) => {
                target.tls
                    && setup.proxy.is_some()
                    && !bypasses(&setup.no_proxy, &target.host, target.port)
            }
            Err(_) => false,
        }
    }

    fn open(&self, target: &Target) -> Result<Stream, Failure> {
        if !target.tls {
            return dial(&target.host, target.port, self.timeout).map(Stream::Plain);
        }
        let setup = self
            .setup
            .as_ref()
            .map_err(|why| Failure::Connect(why.clone()))?;
        let tcp = match (&setup.proxy, self.proxied(target)) {
            (Some(proxy), true) => tunnel(proxy, target, self.timeout)?,
            _ => dial(&target.host, target.port, self.timeout)?,
        };
        handshake(tcp, &target.host, setup.tls.clone())
    }

    /// Send `method path` (after the target's own path prefix) with
    /// `headers` and `body` to `target`'s host over this transport, and
    /// read the response head.
    pub fn send(
        &self,
        target: &Target,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Response, Failure> {
        exchange(self.open(target)?, target, method, path, headers, body)
    }
}

#[allow(clippy::unwrap_in_result)] // tests: a panic is the failure report
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
        backend.write_all(bytes).unwrap();
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

    #[test]
    fn a_proxy_is_an_http_address_with_an_optional_login_that_no_error_quotes() {
        let plain = parse_proxy("http://127.0.0.1:46229").unwrap();
        assert_eq!(
            (plain.host.as_str(), plain.port, plain.authorization),
            ("127.0.0.1", 46229, None)
        );
        let bare = parse_proxy("proxy.corp:3128/").unwrap();
        assert_eq!((bare.host.as_str(), bare.port), ("proxy.corp", 3128));
        // "ann:p@ss" is a login with an escaped @; base64 of the decoded text.
        let login = parse_proxy("http://ann:p%40ss@proxy.corp:8080").unwrap();
        assert_eq!(login.host, "proxy.corp");
        assert_eq!(login.authorization.as_deref(), Some("Basic YW5uOnBAc3M="));
        let refused = parse_proxy("socks5://ann:secret@proxy.corp:1080").unwrap_err();
        assert!(refused.contains("socks5"), "{refused}");
        assert!(!refused.contains("secret"), "{refused}");
        let broken = parse_proxy("http://ann:secret@:x").unwrap_err();
        assert!(!broken.contains("secret"), "{broken}");
    }

    #[test]
    fn no_proxy_and_loopback_are_reached_directly_and_the_rest_through_the_proxy() {
        let list = |text: &str| -> Vec<String> {
            text.split(',')
                .map(|e| e.trim().to_ascii_lowercase())
                .collect()
        };
        let none = list("");
        // Loopback is never sent to a proxy.
        for host in ["localhost", "127.0.0.1", "::1", "LocalHost"] {
            assert!(bypasses(&none, host, 443), "{host}");
        }
        assert!(!bypasses(&none, "api.openai.com", 443));
        let rules = list("api.anthropic.com, .internal.example,corp.test:8443,10.0.0.0/8");
        assert!(bypasses(&rules, "api.anthropic.com", 443));
        assert!(bypasses(&rules, "API.anthropic.com", 443));
        assert!(bypasses(&rules, "llm.internal.example", 443));
        assert!(bypasses(&rules, "internal.example", 443));
        assert!(!bypasses(&rules, "notinternal.example", 443));
        assert!(!bypasses(&rules, "evil-api.anthropic.com.example", 443));
        // A port limits its entry to that port.
        assert!(bypasses(&rules, "corp.test", 8443));
        assert!(!bypasses(&rules, "corp.test", 443));
        // Address ranges are not read.
        assert!(!bypasses(&rules, "10.1.2.3", 443));
        assert!(bypasses(&list("*"), "api.openai.com", 443));
    }

    #[test]
    fn a_transport_proxies_only_https_hosts_it_is_not_told_to_skip() {
        let timeout = Duration::from_secs(1);
        let through = Transport::new(
            Some("http://127.0.0.1:3128"),
            "api.anthropic.com",
            None,
            timeout,
        );
        let https = |host: &str| Target::parse(&format!("https://{host}")).unwrap();
        assert!(through.proxied(&https("api.openai.com")));
        assert!(!through.proxied(&https("api.anthropic.com")));
        assert!(!through.proxied(&https("127.0.0.1")));
        assert!(!through.proxied(&Target::parse("http://api.openai.com").unwrap()));
        assert!(!Transport::direct(timeout).proxied(&https("api.openai.com")));
        // A proxy that cannot be read fails https and leaves http alone.
        let broken = Transport::new(Some("ftp://proxy"), "", None, timeout);
        assert!(!broken.proxied(&https("api.openai.com")));
        assert!(broken.setup.is_err());
    }
}
