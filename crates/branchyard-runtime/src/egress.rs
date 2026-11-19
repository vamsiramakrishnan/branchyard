//! [`Proxy`]: an allowlisting HTTP and HTTPS forward proxy for a harness's
//! egress, which looks inside a connection when its rule asks it to.
//!
//! It speaks the two forms a proxy-aware client sends: `CONNECT host:port`
//! for TLS (and anything else tunnelled), and an absolute URI
//! (`GET http://host/path`) for plain HTTP. Each request's host and port,
//! and its method and path when the proxy sees them, go to the caller's
//! decision; an allowed one is connected to and relayed, a denied one gets
//! `403 Forbidden` with a body saying why. Every decision, either way, goes
//! to the caller's report, once.
//!
//! A `CONNECT` whose verdict says to inspect is not tunnelled blind. The
//! proxy answers it, then terminates the client's TLS with a certificate
//! for the host signed by the [`Authority`] its [`Interceptor`] holds (one
//! the harness is told to trust), reads one HTTP/1.1 request, asks the
//! decision about its method and path, and, allowed, sends it to the origin
//! over TLS of its own, verified against the Mozilla roots and any extra
//! roots, relaying the response until the origin closes. A client that
//! speaks plain HTTP inside such a tunnel is read the same way.
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
//! - Inside an inspected connection, exactly one request is read and sent,
//!   in origin form, to the `CONNECT` host; a `Host` header naming another
//!   host is refused; the certificate presented names the `CONNECT` host
//!   only, so a client naming another in its TLS server name fails its own
//!   verification; nothing sent after the request is forwarded.
//! - Dropping the proxy stops it accepting; connections already relayed
//!   end when either side closes.
//!
//! What it does not guarantee:
//!
//! - Anything about what flows through a tunnel it was not told to
//!   inspect: a client that names an allowed host in `CONNECT` and another
//!   in its TLS server name (domain fronting) is not detected there.
//! - HTTP/2 or WebSockets inside an inspected connection: ALPN offers
//!   HTTP/1.1 only, and the connection closes after its one response.
//! - Anything about a client that ignores the proxy. Whether such a client
//!   can reach the network is the caller's business: see
//!   [`crate::LocalProvider::spawn_confined`].

use branchyard_support::best_effort;
use branchyard_support::LockExt as _;
use branchyard_wire as wire;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
    KeyIdMethod, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};

/// Largest request head read before a request is refused.
const HEAD_MAX: usize = 64 * 1024;
/// How long a client may take to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How long connecting to an allowed upstream may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The one protocol offered inside an inspected connection.
const ALPN_HTTP1: &[u8] = b"http/1.1";
/// The first byte of a TLS record: a handshake.
const TLS_HANDSHAKE: u8 = 0x16;

/// What the caller's decision says about one destination or request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Whether the destination, or the request, may be reached.
    pub allowed: bool,
    /// The rule that allowed it, for the report.
    pub rule: Option<String>,
    /// The rule names a loopback address (or `localhost`) itself, so the
    /// destination may resolve to one.
    pub loopback: bool,
    /// Look inside the connection: every rule allowing the host names a
    /// method or a path, which only the requests inside can be held to.
    /// Read for a `CONNECT`, which the decision sees before any request.
    pub inspect: bool,
}

/// One request's outcome, as reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// `CONNECT`, or the method of a request the proxy saw.
    pub method: String,
    /// The destination host.
    pub host: String,
    /// The destination port.
    pub port: u16,
    /// The request's path, with its query, when the proxy saw one: a plain
    /// HTTP request's, or one inside an inspected connection. `None` for a
    /// `CONNECT`.
    pub path: Option<String>,
    /// Whether the request was allowed.
    pub allowed: bool,
    /// The rule that allowed it, when one did.
    pub rule: Option<String>,
    /// Why it was refused, or why an allowed one failed; `inspected` for a
    /// `CONNECT` the proxy looked inside.
    pub reason: Option<String>,
}

/// Decides a request: the destination `(host, port)` and, when the proxy
/// sees one, its `(method, path)`: a plain HTTP request's, or a request
/// inside an inspected connection. `None` is a `CONNECT` before anything
/// inside it is seen; its verdict says whether to look inside.
pub type Decide = dyn Fn(&str, u16, Option<(&str, &str)>) -> Verdict + Send + Sync;
/// Receives every decision once.
pub type Report = dyn Fn(&Decision) + Send + Sync;

/// A certificate authority that signs the certificates an [`Interceptor`]
/// presents: one per branch, whose certificate the branch's harness is told
/// to trust and whose key stays with the engine. It signs under
/// Branchyard's name for its owner, and the certificates it signs name its
/// key by the Subject Key Identifier its certificate carries, or name no key
/// when that certificate carries none (see [`Authority::from_pem`]).
pub struct Authority {
    issuer: Issuer<'static, KeyPair>,
    /// The Subject Key Identifier read from `cert_pem`, which the
    /// certificates it signs carry as their Authority Key Identifier.
    key_id: Option<Vec<u8>>,
    cert_pem: String,
    key_pem: String,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authority").finish_non_exhaustive()
    }
}

impl Authority {
    /// A new authority for `owner` (a branch's name), with a fresh key. Its
    /// certificate carries a Subject Key Identifier, read back from the
    /// certificate as written; it is an error if it cannot be.
    pub fn generate(owner: &str) -> Result<Authority, String> {
        let key = KeyPair::generate()
            .map_err(|e| format!("could not generate the authority's key: {e}"))?;
        let mut params = Authority::params(owner)?;
        let cert = params
            .self_signed(&key)
            .map_err(|e| format!("could not sign the authority's certificate: {e}"))?;
        let key_id = subject_key_identifier(cert.der())
            .ok_or("the authority's certificate has no readable Subject Key Identifier")?;
        params.key_identifier_method = KeyIdMethod::PreSpecified(key_id.clone());
        let cert_pem = cert.pem();
        let key_pem = key.serialize_pem();
        Ok(Authority {
            issuer: Issuer::new(params, key),
            key_id: Some(key_id),
            cert_pem,
            key_pem,
        })
    }

    /// The authority [`Authority::generate`] made for `owner`, from the
    /// certificate and key it was kept as. The certificate is kept as
    /// written, for clients' trust stores; what signs is the key, under
    /// Branchyard's name for `owner`, as [`Authority::generate`] gives it.
    ///
    /// It is an error when the key is not a PEM private key or no PEM
    /// certificate block can be read from the certificate's text; nothing
    /// else is checked, the key against the certificate included. Of the
    /// first certificate block, only the Subject Key Identifier is read: the
    /// certificates this authority signs carry it as their Authority Key
    /// Identifier, and carry none when the block holds none or is not a
    /// certificate that can be read.
    pub fn from_pem(owner: &str, cert_pem: &str, key_pem: &str) -> Result<Authority, String> {
        let key = KeyPair::from_pem(key_pem)
            .map_err(|e| format!("the authority's key is not a PEM private key: {e}"))?;
        let cert = CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .map_err(|_| "the authority's certificate is not a PEM certificate".to_owned())?;
        let key_id = subject_key_identifier(&cert);
        let mut params = Authority::params(owner)?;
        if let Some(key_id) = &key_id {
            params.key_identifier_method = KeyIdMethod::PreSpecified(key_id.clone());
        }
        Ok(Authority {
            issuer: Issuer::new(params, key),
            key_id,
            cert_pem: cert_pem.to_owned(),
            key_pem: key_pem.to_owned(),
        })
    }

    /// The authority's certificate as described: the same for every
    /// authority of `owner`, so a key written once keeps signing after a
    /// restart.
    fn params(owner: &str) -> Result<CertificateParams, String> {
        let mut params = CertificateParams::new(Vec::<String>::new())
            .map_err(|e| format!("could not describe the authority: {e}"))?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, format!("Branchyard egress {owner}"));
        params
            .distinguished_name
            .push(DnType::OrganizationName, "Branchyard");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2124, 1, 1);
        Ok(params)
    }

    /// The certificate, PEM, for a client's trust store.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// The private key, PEM. It signs certificates for every host the
    /// client trusts, so it is for the engine alone.
    pub fn key_pem(&self) -> &str {
        &self.key_pem
    }
}

/// The Subject Key Identifier of the DER certificate `der`; `None` when it
/// carries none or cannot be read as a certificate.
fn subject_key_identifier(der: &[u8]) -> Option<Vec<u8>> {
    use x509_cert::der::Decode as _;
    let cert = x509_cert::Certificate::from_der(der).ok()?;
    let (_, key_id) = cert
        .tbs_certificate
        .get::<x509_cert::ext::pkix::SubjectKeyIdentifier>()
        .ok()??;
    Some(key_id.0.as_bytes().to_vec())
}

/// Looks inside the connections a verdict asks about: terminates the
/// client's TLS with a certificate for the host signed by its
/// [`Authority`], and speaks TLS of its own to the origin.
pub struct Interceptor {
    authority: Authority,
    /// One key for every host's certificate; the certificate is what
    /// differs.
    leaf_key: KeyPair,
    leaf_signing: Arc<dyn rustls::sign::SigningKey>,
    provider: Arc<rustls::crypto::CryptoProvider>,
    /// The server configuration per host, made on first use.
    servers: Mutex<HashMap<String, Arc<ServerConfig>>>,
    client: Arc<ClientConfig>,
}

impl std::fmt::Debug for Interceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interceptor").finish_non_exhaustive()
    }
}

impl Interceptor {
    /// An interceptor signing with `authority` and, toward origins,
    /// trusting the Mozilla roots plus the certificates in
    /// `extra_roots_pem`: an environment's own authorities, such as those
    /// of a corporate proxy.
    pub fn new(authority: Authority, extra_roots_pem: Option<&str>) -> Result<Interceptor, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let leaf_key =
            KeyPair::generate().map_err(|e| format!("could not generate the TLS key: {e}"))?;
        let der = PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into());
        let leaf_signing = rustls::crypto::ring::sign::any_supported_type(&der)
            .map_err(|e| format!("the TLS key is not usable: {e}"))?;
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for cert in CertificateDer::pem_slice_iter(extra_roots_pem.unwrap_or_default().as_bytes()) {
            let cert =
                cert.map_err(|e| format!("the extra roots are not PEM certificates: {e}"))?;
            roots
                .add(cert)
                .map_err(|e| format!("an extra root is not a certificate authority: {e}"))?;
        }
        let mut client = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        client.alpn_protocols = vec![ALPN_HTTP1.to_vec()];
        Ok(Interceptor {
            authority,
            leaf_key,
            leaf_signing,
            provider,
            servers: Mutex::new(HashMap::new()),
            client: Arc::new(client),
        })
    }

    /// The authority's certificate, PEM.
    pub fn authority_pem(&self) -> &str {
        self.authority.cert_pem()
    }

    /// The server configuration presenting a certificate for `host` (a
    /// DNS name or an IP address), made once per host.
    fn server(&self, host: &str) -> Result<Arc<ServerConfig>, String> {
        if let Some(config) = self
            .servers
            .lock_recovering("egress certificates")
            .get(host)
        {
            return Ok(config.clone());
        }
        let cert = self.leaf(host)?;
        let certified = CertifiedKey::new(vec![cert.der().clone()], self.leaf_signing.clone());
        let mut config = ServerConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("TLS: {e}"))?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Fixed(Arc::new(certified))));
        config.alpn_protocols = vec![ALPN_HTTP1.to_vec()];
        let config = Arc::new(config);
        self.servers
            .lock_recovering("egress certificates")
            .insert(host.to_owned(), config.clone());
        Ok(config)
    }

    /// The certificate presented for `host`, signed by the authority.
    fn leaf(&self, host: &str) -> Result<rcgen::Certificate, String> {
        let mut params = CertificateParams::new(vec![host.to_owned()])
            .map_err(|e| format!("{host} cannot be a certificate's name: {e}"))?;
        params.distinguished_name.push(DnType::CommonName, host);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        // Strict verifiers (Python 3.13 and later by default) refuse a
        // certificate that does not name its issuer's key. The identifier
        // is the Subject Key Identifier read from the authority's
        // certificate, which the issuer holds as a pre-specified identifier.
        // An authority whose certificate carries none (only a kept one can)
        // names no key, as before: an identifier no certificate carries would
        // not help a client find the issuer.
        params.use_authority_key_identifier_extension = self.authority.key_id.is_some();
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2124, 1, 1);
        params
            .signed_by(&self.leaf_key, &self.authority.issuer)
            .map_err(|e| format!("could not sign a certificate for {host}: {e}"))
    }
}

/// Presents one certificate whatever the client's server name: the
/// `CONNECT` host's. A client naming another host fails its own check.
#[derive(Debug)]
struct Fixed(Arc<CertifiedKey>);

impl ResolvesServerCert for Fixed {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

struct Shared {
    decide: Box<Decide>,
    report: Box<Report>,
    interceptor: OnceLock<Arc<Interceptor>>,
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
    /// A proxy that asks `decide` about every destination and request and
    /// tells `report` every outcome. It serves nothing until given a
    /// listener, and looks inside nothing until given an interceptor.
    pub fn new(
        decide: impl Fn(&str, u16, Option<(&str, &str)>) -> Verdict + Send + Sync + 'static,
        report: impl Fn(&Decision) + Send + Sync + 'static,
    ) -> Proxy {
        Proxy {
            shared: Arc::new(Shared {
                decide: Box::new(decide),
                report: Box::new(report),
                interceptor: OnceLock::new(),
                stop: AtomicBool::new(false),
            }),
            listening: Mutex::new(Vec::new()),
        }
    }

    /// Give the proxy what it needs to look inside a connection whose
    /// verdict asks for it. Without one, such a `CONNECT` is refused. Once.
    pub fn intercept(&self, interceptor: Interceptor) -> Result<(), String> {
        self.shared
            .interceptor
            .set(Arc::new(interceptor))
            .map_err(|_| "the proxy already has an interceptor".to_owned())
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
    // Both are expected to fail somewhere: on Linux the shutdown ends the
    // listening, so the connect is then refused. Logged once per process,
    // not on every stop.
    branchyard_support::best_effort_once(
        "shut down the egress listener",
        rustix::net::shutdown(&listener.socket, rustix::net::Shutdown::Both),
    );
    if let Some(local) = listener.local {
        branchyard_support::best_effort_once(
            "wake the egress listener",
            TcpStream::connect_timeout(&local, Duration::from_secs(1)),
        );
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
                best_effort(
                    "start the connection thread",
                    thread::Builder::new()
                        .name("by-egress-conn".into())
                        .spawn(move || handle(stream, &shared)),
                );
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

/// A `CONNECT` the proxy will look inside.
struct Opened {
    host: String,
    port: u16,
    decision: Decision,
}

fn handle(stream: TcpStream, shared: &Shared) {
    best_effort(
        "restore the stream's read timeout",
        stream.set_read_timeout(Some(HEAD_TIMEOUT)),
    );
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let mut client = BufReader::new(stream);
    let mut out = writer;
    let head = match read_request(&mut client) {
        Ok(Some(head)) => head,
        Ok(None) => return,
        Err(why) => {
            branchyard_support::best_effort(
                "answer the client a refusal",
                respond(&mut out, 400, "Bad Request", &why),
            );
            return;
        }
    };
    let (host, port, path) = match parse_target(&head) {
        Ok(target) => target,
        Err(why) => {
            branchyard_support::best_effort(
                "answer the client a refusal",
                respond(&mut out, 400, "Bad Request", &why),
            );
            return;
        }
    };
    let tunnel = head.method == "CONNECT";
    let request = match tunnel {
        true => None,
        false => Some((head.method.as_str(), path.as_str())),
    };
    let verdict = (shared.decide)(&host, port, request);
    let mut decision = Decision {
        method: head.method.clone(),
        host: host.clone(),
        port,
        path: request.map(|(_, path)| path.to_owned()),
        allowed: verdict.allowed,
        rule: verdict.rule.clone(),
        reason: None,
    };
    if !verdict.allowed {
        decision.reason = Some("no rule allows it".into());
        (shared.report)(&decision);
        let what = match request {
            Some((method, path)) => format!("{method} {}{path}", authority(&host, port)),
            None => authority(&host, port),
        };
        branchyard_support::best_effort(
            "answer the client a refusal",
            respond(
                &mut out,
                403,
                "Forbidden",
                &format!(
                "Branchyard's egress policy does not allow {what} for this branch (docs/egress.md)"
            ),
            ),
        );
        return;
    }
    if tunnel && verdict.inspect {
        let Some(interceptor) = shared.interceptor.get() else {
            decision.allowed = false;
            decision.reason = Some(
                "the rule inspects requests, but this proxy has no certificate authority to look \
                 inside the connection"
                    .into(),
            );
            (shared.report)(&decision);
            branchyard_support::best_effort(
                "answer the client a refusal",
                respond(
                    &mut out,
                    403,
                    "Forbidden",
                    &format!(
                    "Branchyard's egress policy allows only some requests to {}, and this proxy \
                     cannot look inside the connection (docs/egress.md)",
                    authority(&host, port)
                ),
                ),
            );
            return;
        };
        let opened = Opened {
            host,
            port,
            decision,
        };
        // Whatever ends the connection early (the client or the origin
        // going away) is the connection's business, and already reported
        // where it mattered.
        branchyard_support::best_effort(
            "inspect a tunnelled connection",
            inspect(client, out, opened, shared, interceptor),
        );
        return;
    }
    let upstream = match connect(&host, port, verdict.loopback) {
        Ok(upstream) => upstream,
        Err(Refused::Policy(why)) => {
            decision.allowed = false;
            decision.reason = Some(why.clone());
            (shared.report)(&decision);
            branchyard_support::best_effort(
                "answer the client a refusal",
                respond(
                    &mut out,
                    403,
                    "Forbidden",
                    &format!("Branchyard's egress policy does not allow {why}"),
                ),
            );
            return;
        }
        Err(Refused::Unreachable(why)) => {
            decision.reason = Some(why.clone());
            (shared.report)(&decision);
            branchyard_support::best_effort(
                "answer the client a refusal",
                respond(&mut out, 502, "Bad Gateway", &why),
            );
            return;
        }
    };
    (shared.report)(&decision);
    best_effort(
        "restore the client stream's read timeout",
        client.get_ref().set_read_timeout(None),
    );
    let mut upstream_writer = match upstream.try_clone() {
        Ok(writer) => writer,
        Err(_) => return,
    };
    if tunnel {
        if out
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .is_err()
        {
            return;
        }
    } else {
        let mut request = format!("{} {} HTTP/1.{}\r\n", head.method, path, head.minor);
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

/// Shut `stream` down. `NotConnected` means the peer went first, which is
/// routine and not logged; any other failure is.
fn shut_down_quietly(stream: &TcpStream, how: Shutdown, what: &str) {
    best_effort(
        what,
        stream.shutdown(how).or_else(|error| match error.kind() {
            io::ErrorKind::NotConnected => Ok(()),
            _ => Err(error),
        }),
    );
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
            branchyard_support::best_effort(
                "relay the client's bytes upstream",
                io::copy(&mut client_read, &mut upstream_write),
            );
            shut_down_quietly(&upstream_write, Shutdown::Write, "half-close upstream");
        });
    branchyard_support::best_effort(
        "relay upstream's bytes to the client",
        io::copy(&mut upstream_read, &mut client_write),
    );
    shut_down_quietly(&client_write, Shutdown::Both, "close the client");
    // ENOTCONN when the client closed first: routine, not worth a log line.
    shut_down_quietly(&upstream_read, Shutdown::Both, "close upstream");
    if let Ok(up) = up {
        branchyard_support::join_reporting("egress upstream relay", up);
    }
}

/// A stream that first yields what was already read from it.
struct Prefixed {
    early: Vec<u8>,
    at: usize,
    stream: TcpStream,
}

impl Read for Prefixed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at < self.early.len() {
            let n = buf.len().min(self.early.len() - self.at);
            buf[..n].copy_from_slice(&self.early[self.at..self.at + n]);
            self.at += n;
            return Ok(n);
        }
        self.stream.read(buf)
    }
}

impl Write for Prefixed {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// The client's side of an inspected connection.
enum Inner {
    Plain(Prefixed),
    Tls(Box<StreamOwned<ServerConnection, Prefixed>>),
}

impl Inner {
    fn socket(&self) -> &TcpStream {
        match self {
            Inner::Plain(p) => &p.stream,
            Inner::Tls(s) => &s.sock.stream,
        }
    }

    /// End the connection cleanly: a TLS close notice, then the socket.
    fn close(&mut self) -> io::Result<()> {
        if let Inner::Tls(s) = self {
            s.conn.send_close_notify();
            s.flush()?;
        }
        self.socket().shutdown(Shutdown::Both)
    }
}

impl Read for Inner {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Inner::Plain(p) => p.read(buf),
            Inner::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Inner {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Inner::Plain(p) => p.write(buf),
            Inner::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Inner::Plain(p) => p.flush(),
            Inner::Tls(s) => s.flush(),
        }
    }
}

/// The origin's side of an inspected connection.
enum Upstream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Upstream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Upstream::Plain(s) => s.read(buf),
            Upstream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Upstream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Upstream::Plain(s) => s.write(buf),
            Upstream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Upstream::Plain(s) => s.flush(),
            Upstream::Tls(s) => s.flush(),
        }
    }
}

/// Answer an inspected `CONNECT`, read the one request inside it, decide
/// it, and relay it and its response. An `Err` is an I/O failure after
/// everything worth reporting was reported.
fn inspect(
    client: BufReader<TcpStream>,
    mut out: TcpStream,
    mut opened: Opened,
    shared: &Shared,
    interceptor: &Interceptor,
) -> io::Result<()> {
    out.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
    let early = client.buffer().to_vec();
    let stream = client.into_inner();
    // TLS, or plain HTTP inside the tunnel: the first byte says.
    let first = match early.first() {
        Some(byte) => Some(*byte),
        None => {
            let mut byte = [0u8; 1];
            match stream.peek(&mut byte) {
                Ok(1) => Some(byte[0]),
                _ => None,
            }
        }
    };
    let Some(first) = first else {
        opened.decision.reason = Some("inspected, but nothing was sent inside".into());
        (shared.report)(&opened.decision);
        return Ok(());
    };
    let tls = first == TLS_HANDSHAKE;
    let prefixed = Prefixed {
        early,
        at: 0,
        stream,
    };
    let inner = match tls {
        false => Inner::Plain(prefixed),
        true => {
            let config = interceptor
                .server(&opened.host)
                .and_then(|config| ServerConnection::new(config).map_err(|e| e.to_string()));
            let conn = match config {
                Ok(conn) => conn,
                Err(why) => {
                    opened.decision.reason = Some(format!(
                        "inspected, but no certificate could be made: {why}"
                    ));
                    (shared.report)(&opened.decision);
                    return Ok(());
                }
            };
            let mut stream = StreamOwned::new(conn, prefixed);
            while stream.conn.is_handshaking() {
                if let Err(error) = stream.conn.complete_io(&mut stream.sock) {
                    opened.decision.reason = Some(format!(
                        "inspected, but the TLS handshake with the harness failed: {error}; does \
                         it trust the branch's certificate authority? (docs/egress.md)"
                    ));
                    (shared.report)(&opened.decision);
                    return Ok(());
                }
            }
            Inner::Tls(Box::new(stream))
        }
    };
    opened.decision.reason = Some("inspected".into());
    (shared.report)(&opened.decision);
    let host = opened.host;
    let port = opened.port;
    let mut reader = BufReader::new(inner);
    let head = match read_request(&mut reader) {
        Ok(Some(head)) => head,
        Ok(None) => return reader.get_mut().close(),
        Err(why) => return refuse(reader.get_mut(), 400, "Bad Request", &why),
    };
    let path = match request_path(&head.target) {
        Ok(path) => path,
        Err(why) => return refuse(reader.get_mut(), 400, "Bad Request", &why),
    };
    let verdict = (shared.decide)(&host, port, Some((&head.method, &path)));
    let mut decision = Decision {
        method: head.method.clone(),
        host: host.clone(),
        port,
        path: Some(path.clone()),
        allowed: verdict.allowed,
        rule: verdict.rule.clone(),
        reason: None,
    };
    let what = format!("{} {}{path}", head.method, authority(&host, port));
    if let Some(other) = host_mismatch(&head, &host) {
        decision.allowed = false;
        decision.reason = Some(format!(
            "its Host header names {other}, not the connection's host"
        ));
        (shared.report)(&decision);
        return refuse(
            reader.get_mut(),
            403,
            "Forbidden",
            &format!(
                "Branchyard's egress proxy refuses {what}: its Host header names {other}, not the \
                 connection's host (docs/egress.md)"
            ),
        );
    }
    if !verdict.allowed {
        decision.reason = Some("no rule allows it".into());
        (shared.report)(&decision);
        return refuse(
            reader.get_mut(),
            403,
            "Forbidden",
            &format!(
                "Branchyard's egress policy does not allow {what} for this branch (docs/egress.md)"
            ),
        );
    }
    let upstream = match connect(&host, port, verdict.loopback) {
        Ok(upstream) => upstream,
        Err(Refused::Policy(why)) => {
            decision.allowed = false;
            decision.reason = Some(why.clone());
            (shared.report)(&decision);
            return refuse(
                reader.get_mut(),
                403,
                "Forbidden",
                &format!("Branchyard's egress policy does not allow {why}"),
            );
        }
        Err(Refused::Unreachable(why)) => {
            decision.reason = Some(why.clone());
            (shared.report)(&decision);
            return refuse(reader.get_mut(), 502, "Bad Gateway", &why);
        }
    };
    let mut upstream = match tls {
        false => Upstream::Plain(upstream),
        true => match tls_to(interceptor, upstream, &host) {
            Ok(upstream) => upstream,
            Err(why) => {
                decision.reason = Some(why.clone());
                (shared.report)(&decision);
                return refuse(reader.get_mut(), 502, "Bad Gateway", &why);
            }
        },
    };
    (shared.report)(&decision);
    best_effort(
        "restore the client stream's read timeout",
        reader.get_ref().socket().set_read_timeout(None),
    );
    // The request head in origin form, to this host, closing after; its
    // body framed as the client framed it (the wire crate decides what
    // that is, and refuses what it cannot frame).
    let framing = match wire::request_framing(&head.headers) {
        Ok(framing) => framing,
        Err(why) => return refuse(reader.get_mut(), 400, "Bad Request", &why.to_string()),
    };
    let mut request = format!("{} {path} HTTP/1.{}\r\n", head.method, head.minor);
    let mut expects_continue = false;
    for (name, value) in &head.headers {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "connection" | "keep-alive" | "proxy-connection" => continue,
            "expect" if value.eq_ignore_ascii_case("100-continue") => {
                expects_continue = true;
                continue;
            }
            _ if lower.starts_with("proxy-") => continue,
            _ => {}
        }
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if wire::header(&head.headers, "host").is_none() {
        let default = tls && port == 443 || !tls && port == 80;
        let value = match default {
            true => bracketed(&host),
            false => authority(&host, port),
        };
        request.push_str(&format!("Host: {value}\r\n"));
    }
    request.push_str("Connection: close\r\n\r\n");
    upstream.write_all(request.as_bytes())?;
    if expects_continue {
        reader
            .get_mut()
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    forward_body(&mut reader, framing, &mut upstream)?;
    upstream.flush()?;
    // The response, until the origin closes.
    io::copy(&mut upstream, reader.get_mut())?;
    reader.get_mut().close()
}

/// Copy a request body of `framing` from the client to the origin, the
/// framing undone by the wire crate and put back chunk by chunk, so a
/// malformed body is an error here rather than the origin's problem.
fn forward_body(
    from: &mut impl BufRead,
    framing: wire::Framing,
    upstream: &mut impl Write,
) -> io::Result<()> {
    let mut body = wire::Body::new(from, framing);
    if framing != wire::Framing::Chunked {
        io::copy(&mut body, upstream)?;
        return Ok(());
    }
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        wire::write_chunk(upstream, &buf[..n])?;
    }
    upstream.write_all(wire::LAST_CHUNK)?;
    upstream.flush()
}

/// TLS to the origin at `host`, verified against the interceptor's roots.
fn tls_to(interceptor: &Interceptor, stream: TcpStream, host: &str) -> Result<Upstream, String> {
    let name = ServerName::try_from(host.to_owned())
        .map_err(|e| format!("{host} is not a TLS server name: {e}"))?;
    let conn = ClientConnection::new(interceptor.client.clone(), name)
        .map_err(|e| format!("TLS to {host}: {e}"))?;
    let mut stream = StreamOwned::new(conn, stream);
    while stream.conn.is_handshaking() {
        stream
            .conn
            .complete_io(&mut stream.sock)
            .map_err(|e| format!("the TLS handshake with {host} failed: {e}"))?;
    }
    Ok(Upstream::Tls(Box::new(stream)))
}

/// The path, with its query, a request inside an inspected connection
/// asks for: its target in origin form, or the path of an absolute one.
fn request_path(target: &str) -> Result<String, String> {
    if target.starts_with('/') || target == "*" {
        return Ok(target.to_owned());
    }
    match target.split_once("://") {
        Some((_, rest)) => Ok(match rest.find(['/', '?']) {
            Some(at) if rest.as_bytes()[at] == b'/' => rest[at..].to_owned(),
            Some(at) => format!("/{}", &rest[at..]),
            None => "/".to_owned(),
        }),
        None => Err(format!("{target:?} is not a request path")),
    }
}

/// The host a request's `Host` header names when it is not `host`, the
/// connection's.
fn host_mismatch(head: &wire::RequestHead, host: &str) -> Option<String> {
    let value = wire::header(&head.headers, "host")?;
    match read_authority(value) {
        Ok((named, _)) if named == host => None,
        Ok((named, _)) => Some(named),
        Err(_) => Some(value.to_owned()),
    }
}

/// Answer inside an inspected connection and close it.
fn refuse(inner: &mut Inner, code: u16, reason: &str, body: &str) -> io::Result<()> {
    write_response(inner, code, reason, body)?;
    inner.close()
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
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
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

/// Read one request head with the wire crate: `None` when the client closed
/// before sending one. Blank lines before the request line are skipped, and
/// count against the same `HEAD_MAX` as the head itself.
fn read_request<R: Read>(client: &mut BufReader<R>) -> Result<Option<wire::RequestHead>, String> {
    let mut used = 0;
    let raw = loop {
        let raw = match wire::read_head(client, HEAD_MAX.saturating_sub(used)) {
            Ok(Some(raw)) => raw,
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(match error {
                    wire::WireError::Io { .. } => format!("could not read the request: {error}"),
                    _ => error.to_string(),
                })
            }
        };
        used += raw.len();
        if matches!(raw.as_slice(), b"\r\n" | b"\n") {
            continue;
        }
        break raw;
    };
    if std::str::from_utf8(&raw).is_err() {
        return Err("the request head is not UTF-8".into());
    }
    wire::parse_request_head(&raw).map(Some).map_err(|error| {
        // The parser says what is malformed, not which line: the request
        // line on its own tells a bad request line from a bad header.
        let mut first = raw
            .split_inclusive(|&b| b == b'\n')
            .next()
            .unwrap_or_default()
            .to_vec();
        first.extend_from_slice(b"\r\n");
        match wire::parse_request_head(&first) {
            Err(_) => "the request line is not METHOD TARGET VERSION".to_owned(),
            Ok(_) => error.to_string(),
        }
    })
}

/// The destination and, for plain HTTP, the origin-form path.
fn parse_target(head: &wire::RequestHead) -> Result<(String, u16, String), String> {
    if head.method == "CONNECT" {
        let (host, port) = read_authority(&head.target)?;
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
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let (host, port) = read_authority(authority)?;
    let url = wire::HttpUrl::parse(&format!("http://{rest}")).map_err(|e| e.to_string())?;
    Ok((host, port.unwrap_or(80), url.target()))
}

/// `host[:port]` or `[v6][:port]` and nothing more, read by the wire
/// crate's URL parser. The host is as the decision sees it (lower case, no
/// trailing dot); the port is `None` when the authority names none.
fn read_authority(authority: &str) -> Result<(String, Option<u16>), String> {
    if authority.contains('@') {
        return Err("user information in the target is not forwarded".into());
    }
    let read = |scheme: &str| {
        wire::HttpUrl::parse(&format!("{scheme}://{authority}"))
            .map_err(|_| format!("{authority:?} is not a host and port"))
    };
    let (plain, secure) = (read("http")?, read("https")?);
    if !plain.path.is_empty() || plain.query.is_some() {
        return Err(format!("{authority:?} is not a host and port"));
    }
    // The parser fills in each scheme's default port, so the authority names
    // a port exactly when both schemes read the same one.
    let port = (plain.port == secure.port).then_some(plain.port);
    if port == Some(0) {
        return Err("0 is not a port".into());
    }
    Ok((host_name(&plain.host)?, port))
}

/// A host name the proxy forwards: letters, digits, `-`, `.`, `_` and `:`
/// (for an IPv6 literal), at most 253 bytes, lower-cased without a trailing dot.
fn host_name(host: &str) -> Result<String, String> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'_'));
    match ok {
        true => Ok(host),
        false => Err(format!("{host:?} is not a host")),
    }
}

/// An HTTP/1.1 response with a text body, closing the connection.
fn write_response(out: &mut impl Write, code: u16, reason: &str, body: &str) -> io::Result<()> {
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
    out.flush()
}

fn respond(out: &mut TcpStream, code: u16, reason: &str, body: &str) -> io::Result<()> {
    write_response(out, code, reason, body)?;
    shut_down_quietly(out, Shutdown::Both, "close a refused connection");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(method: &str, target: &str) -> wire::RequestHead {
        wire::RequestHead {
            method: method.into(),
            target: target.into(),
            minor: 1,
            headers: Vec::new(),
        }
    }

    /// The request head `bytes` holds, read the way a client's is.
    fn read(bytes: &[u8]) -> Result<Option<wire::RequestHead>, String> {
        read_request(&mut BufReader::new(bytes))
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
            ("GET", "http://bad!host/", "not a host"),
            ("GET", "ftp://x/", "needs CONNECT"),
        ] {
            let error = parse(method, target).unwrap_err();
            assert!(error.contains(why), "{target}: {error}");
        }
    }

    #[test]
    fn connect_authorities_are_host_and_port_with_brackets_for_v6() {
        let parse = |t: &str| parse_target(&head("CONNECT", t));
        assert_eq!(
            parse("[2001:db8::1]:443").unwrap(),
            ("2001:db8::1".into(), 443, String::new())
        );
        assert_eq!(
            parse("Example.COM.:8443").unwrap(),
            ("example.com".into(), 8443, String::new())
        );
        // Without brackets an IPv6 address is ambiguous with a port: refused.
        for target in [
            "::1:443",
            "[::1]",
            "[::1]x",
            "github.com:443/x",
            "github.com:443?x",
        ] {
            assert!(parse(target).is_err(), "{target}");
        }
        assert!(parse("[::1]").unwrap_err().contains("host:port"));
        assert!(parse("[::1]:0").unwrap_err().contains("not a port"));
    }

    #[test]
    fn a_plain_http_target_keeps_its_path_and_query_as_written() {
        let parse = |t: &str| parse_target(&head("GET", t)).unwrap();
        assert_eq!(parse("http://[::1]:8080/a?b").1, 8080);
        assert_eq!(parse("http://[::1]/a?b").2, "/a?b");
        assert_eq!(parse("http://example.com/a/b?c/d").2, "/a/b?c/d");
        assert_eq!(parse("http://example.com?q/r").2, "/?q/r");
        assert_eq!(parse("http://example.com:80").1, 80);
    }

    #[test]
    fn a_request_head_is_read_after_blank_lines() {
        let request = b"\r\n\nGET http://example.com/ HTTP/1.0\r\nHost: example.com\r\n\r\n";
        let head = read(request).unwrap().unwrap();
        assert_eq!(head.method, "GET");
        assert_eq!(head.target, "http://example.com/");
        assert_eq!(head.minor, 0);
        assert_eq!(wire::header(&head.headers, "host"), Some("example.com"));
        assert_eq!(
            read(b"").unwrap(),
            None,
            "a client that closed sends nothing"
        );
        assert_eq!(
            read(b"\r\n\r\n").unwrap(),
            None,
            "blank lines alone are not a head"
        );
    }

    #[test]
    fn a_malformed_request_line_or_header_is_refused() {
        for bytes in [
            &b"GET /only-two\r\n\r\n"[..],
            &b"GET / HTTP/1.1 extra\r\n\r\n"[..],
            &b"GET / HTTP/2.0\r\n\r\n"[..],
            &b"GET / HTTP/1.1\r\nno colon here\r\n\r\n"[..],
            &b"GET / HTTP/1.1\r\nHost: x\r\n"[..],
            &b"GET / HTTP/1.1\r\nHost: \xff\r\n\r\n"[..],
        ] {
            assert!(read(bytes).is_err(), "{:?}", String::from_utf8_lossy(bytes));
        }
        assert_eq!(
            read(b"nonsense\r\n\r\n").unwrap_err(),
            "the request line is not METHOD TARGET VERSION"
        );
        assert!(read(b"GET / HTTP/1.1\r\nno colon here\r\n\r\n")
            .unwrap_err()
            .starts_with("the head is malformed"));
    }

    #[test]
    fn a_head_over_the_limit_is_refused() {
        let prefix = "GET http://example.com/ HTTP/1.1\r\nX: ";
        let suffix = "\r\n\r\n";
        let at_limit = format!(
            "{prefix}{}{suffix}",
            "a".repeat(HEAD_MAX - prefix.len() - suffix.len())
        );
        assert!(
            read(at_limit.as_bytes()).unwrap().is_some(),
            "exactly the limit is read"
        );
        let over = format!(
            "{prefix}{}{suffix}",
            "a".repeat(HEAD_MAX - prefix.len() - suffix.len() + 1)
        );
        let error = read(over.as_bytes()).unwrap_err();
        assert!(error.contains("larger than"), "{error}");
        // One endless line, never ended: refused at the limit, not buffered.
        let endless = vec![b'a'; HEAD_MAX + 10];
        assert!(read(&endless).unwrap_err().contains("larger than"));
        // Blank lines spend the same budget as the head they precede.
        let blanks = "\r\n".repeat(HEAD_MAX);
        assert!(read(blanks.as_bytes()).is_err());
    }

    #[test]
    fn requests_inside_a_connection_name_a_path() {
        assert_eq!(request_path("/a/b?c=1").unwrap(), "/a/b?c=1");
        assert_eq!(request_path("*").unwrap(), "*");
        assert_eq!(request_path("https://x.example/a?b").unwrap(), "/a?b");
        assert_eq!(request_path("https://x.example").unwrap(), "/");
        assert_eq!(request_path("https://x.example?q").unwrap(), "/?q");
        assert!(request_path("a/b").is_err());
        let mut with = head("GET", "/");
        with.headers.push(("Host".into(), "X.example:443".into()));
        assert_eq!(host_mismatch(&with, "x.example"), None);
        assert_eq!(
            host_mismatch(&with, "y.example").as_deref(),
            Some("x.example")
        );
        assert_eq!(host_mismatch(&head("GET", "/"), "x.example"), None);
        // A Host naming a path is not a host: it mismatches, as before.
        let mut pathed = head("GET", "/");
        pathed.headers.push(("Host".into(), "x.example/a".into()));
        assert_eq!(
            host_mismatch(&pathed, "x.example").as_deref(),
            Some("x.example/a")
        );
    }

    #[test]
    fn a_body_is_forwarded_in_its_own_framing() {
        let body = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\n\r\nGET /next HTTP/1.1\r\n";
        let mut from = BufReader::new(&body[..]);
        let mut to = Vec::new();
        forward_body(&mut from, wire::Framing::Chunked, &mut to).unwrap();
        let mut decoded = String::new();
        wire::ChunkedReader::new(&to[..])
            .read_to_string(&mut decoded)
            .unwrap();
        assert_eq!(decoded, "Wikipedia");
        assert!(to.ends_with(b"0\r\n\r\n"), "{to:?}");
        let mut rest = String::new();
        from.read_to_string(&mut rest).unwrap();
        assert_eq!(
            rest, "GET /next HTTP/1.1\r\n",
            "the next request is not read"
        );

        let mut from = BufReader::new(&b"hello, world"[..]);
        let mut to = Vec::new();
        forward_body(&mut from, wire::Framing::Length(5), &mut to).unwrap();
        assert_eq!(to, b"hello");
        let mut to = Vec::new();
        forward_body(&mut from, wire::Framing::None, &mut to).unwrap();
        assert!(to.is_empty());

        let mut short = BufReader::new(&b"4\r\nWi"[..]);
        assert!(forward_body(&mut short, wire::Framing::Chunked, &mut Vec::new()).is_err());
        let mut bad = BufReader::new(&b"zz\r\n"[..]);
        assert!(forward_body(&mut bad, wire::Framing::Chunked, &mut Vec::new()).is_err());
        let mut truncated = BufReader::new(&b"hel"[..]);
        assert!(forward_body(&mut truncated, wire::Framing::Length(5), &mut Vec::new()).is_err());
    }

    /// What a verifier that checks strictly (OpenSSL's `X509_V_FLAG_X509_STRICT`,
    /// the default of Python 3.13 and later) requires of `leaf` and of the
    /// authority `ca` that issued it, read from the encoded certificates.
    fn assert_strict(ca: &[u8], leaf: &[u8], host: &str) {
        use x509_cert::der::Decode as _;
        use x509_cert::ext::pkix::{
            AuthorityKeyIdentifier, BasicConstraints, KeyUsage, KeyUsages, SubjectAltName,
            SubjectKeyIdentifier,
        };
        let ca = x509_cert::Certificate::from_der(ca)
            .unwrap()
            .tbs_certificate;
        let leaf = x509_cert::Certificate::from_der(leaf)
            .unwrap()
            .tbs_certificate;

        // The authority: a critical CA basic constraint, a key usage that
        // signs certificates, and the identifier of its key.
        assert_eq!(ca.version, x509_cert::Version::V3);
        let (critical, constraints) = ca.get::<BasicConstraints>().unwrap().unwrap();
        assert!(critical && constraints.ca, "{constraints:?}");
        let (_, usage) = ca.get::<KeyUsage>().unwrap().unwrap();
        assert!(usage.0.contains(KeyUsages::KeyCertSign), "{usage:?}");
        let (critical, ca_key_id) = ca.get::<SubjectKeyIdentifier>().unwrap().unwrap();
        assert!(!critical);
        assert!(!ca_key_id.0.as_bytes().is_empty());

        // The leaf: issued by that authority, naming the authority's key by
        // the same identifier, and no authority itself.
        assert_eq!(leaf.version, x509_cert::Version::V3);
        assert_eq!(leaf.issuer, ca.subject);
        let (critical, authority_key) = leaf
            .get::<AuthorityKeyIdentifier>()
            .unwrap()
            .unwrap_or_else(|| panic!("the certificate for {host} names no authority key"));
        assert!(!critical);
        assert_eq!(authority_key.key_identifier, Some(ca_key_id.0), "{host}");
        let (_, names) = leaf.get::<SubjectAltName>().unwrap().unwrap();
        assert_eq!(names.0.len(), 1, "{host}");
        if let Some((_, constraints)) = leaf.get::<BasicConstraints>().unwrap() {
            assert!(!constraints.ca, "{host}");
        }
        let (_, usage) = leaf.get::<KeyUsage>().unwrap().unwrap();
        assert!(!usage.0.contains(KeyUsages::KeyCertSign), "{host}");
    }

    /// Every kind of certificate the interceptor presents (for a name, an
    /// IPv4 and an IPv6 address) chains to its authority by key identifier,
    /// as strict verifiers require.
    #[test]
    fn minted_certificates_carry_what_strict_verifiers_check() {
        let interceptor = Interceptor::new(Authority::generate("branch").unwrap(), None).unwrap();
        let ca = CertificateDer::from_pem_slice(interceptor.authority_pem().as_bytes()).unwrap();
        for host in ["api.example.com", "127.0.0.1", "::1"] {
            let leaf = interceptor.leaf(host).unwrap();
            assert_strict(&ca, leaf.der(), host);
        }
    }

    /// The key identifier of the Authority Key Identifier `cert` carries,
    /// `None` when it carries none.
    fn authority_key_id(cert: &[u8]) -> Option<Vec<u8>> {
        use x509_cert::der::Decode as _;
        x509_cert::Certificate::from_der(cert)
            .unwrap()
            .tbs_certificate
            .get::<x509_cert::ext::pkix::AuthorityKeyIdentifier>()
            .unwrap()
            .map(|(_, aki)| aki.key_identifier.unwrap().as_bytes().to_vec())
    }

    /// A key of the kind an authority's is, PEM, made here so that no key is
    /// committed. [`Authority::from_pem`] does not check it against the
    /// certificate, and nothing here verifies a signature.
    fn some_key() -> String {
        KeyPair::generate().unwrap().serialize_pem()
    }

    /// An authority [`Authority::generate`] wrote, read back from its files,
    /// is kept as written, and the certificates it signs chain to it as
    /// strict verifiers require.
    #[test]
    fn a_kept_authority_signs_certificates_that_name_its_key() {
        let written = Authority::generate("branch").unwrap();
        let kept = Authority::from_pem("branch", written.cert_pem(), written.key_pem()).unwrap();
        assert_eq!(kept.cert_pem(), written.cert_pem());
        let interceptor = Interceptor::new(kept, None).unwrap();
        let ca = CertificateDer::from_pem_slice(written.cert_pem().as_bytes()).unwrap();
        let leaf = interceptor.leaf("api.example.com").unwrap();
        assert_strict(&ca, leaf.der(), "api.example.com");
    }

    /// A self-signed authority as `(certificate, key)` PEM, made with rcgen
    /// as [`Authority::params`] describes it for `branch` but for the common
    /// name and, when given, the Subject Key Identifier.
    fn foreign_authority(common_name: &str, key_id: Option<Vec<u8>>) -> (String, String) {
        let mut params = Authority::params("branch").unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params
            .distinguished_name
            .push(DnType::OrganizationName, "Branchyard");
        if let Some(key_id) = key_id {
            params.key_identifier_method = rcgen::KeyIdMethod::PreSpecified(key_id);
        }
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    /// A kept certificate whose Subject Key Identifier is not the one the
    /// library derives from the key: what the authority signs names that
    /// identifier, not a recomputed one, and is issued under the kept
    /// certificate's subject (Branchyard's own name for the owner).
    #[test]
    fn a_kept_authority_signs_under_the_key_identifier_of_its_certificate() {
        let (cert_pem, key_pem) =
            foreign_authority("Branchyard egress branch", Some((1u8..=20).collect()));
        let kept = Authority::from_pem("branch", &cert_pem, &key_pem).unwrap();
        assert_eq!(kept.cert_pem(), cert_pem);
        let interceptor = Interceptor::new(kept, None).unwrap();
        let ca = CertificateDer::from_pem_slice(cert_pem.as_bytes()).unwrap();
        for host in ["api.example.com", "127.0.0.1", "::1"] {
            let leaf = interceptor.leaf(host).unwrap();
            // Also compares the leaf's issuer with the kept subject.
            assert_strict(&ca, leaf.der(), host);
            assert_eq!(
                authority_key_id(leaf.der()),
                Some((1u8..=20).collect::<Vec<_>>()),
                "{host}"
            );
        }
    }

    /// A self-signed authority certificate with Branchyard's subject for
    /// `branch` and no Subject Key Identifier. Made with OpenSSL 3.0.13,
    /// whose key is not kept:
    ///
    /// ```text
    /// openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out ca.key
    /// openssl req -x509 -new -key ca.key -days 36500 \
    ///   -subj "/CN=Branchyard egress branch/O=Branchyard" \
    ///   -addext "basicConstraints=critical,CA:TRUE" \
    ///   -addext "keyUsage=critical,keyCertSign,cRLSign,digitalSignature" \
    ///   -addext "subjectKeyIdentifier=none" -addext "authorityKeyIdentifier=none"
    /// ```
    const NO_KEY_IDENTIFIER: &str = "-----BEGIN CERTIFICATE-----
MIIBlzCCAT2gAwIBAgIUdkSCFCS6lS/dkIUBFMgBZYpn464wCgYIKoZIzj0EAwIw
ODEhMB8GA1UEAwwYQnJhbmNoeWFyZCBlZ3Jlc3MgYnJhbmNoMRMwEQYDVQQKDApC
cmFuY2h5YXJkMCAXDTI2MTExOTE0MzQ1MloYDzIxMjYxMDI2MTQzNDUyWjA4MSEw
HwYDVQQDDBhCcmFuY2h5YXJkIGVncmVzcyBicmFuY2gxEzARBgNVBAoMCkJyYW5j
aHlhcmQwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASg+R13ILt0tLOI3HqtfUlX
4eo3A87doLIImD5IOjMmeLLlZlg4g1s7wfdwsLPoLyGL7ToB/zfMyKWeuwCCpYzf
oyMwITAPBgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwIBhjAKBggqhkjOPQQD
AgNIADBFAiEA+JBpREhlRwvvnqvK5qqJuvb7TNS2OCd89wAC9YKDMp4CID11fDqn
Rjz7BcFrqIw9/n0H6NOKtoKwBBXOyKtsCznz
-----END CERTIFICATE-----
";

    /// A kept certificate with no Subject Key Identifier: the certificates
    /// its authority signs carry no Authority Key Identifier, as none did
    /// before they named the authority's key, rather than one no
    /// certificate holds.
    #[test]
    fn a_kept_certificate_without_a_key_identifier_signs_without_one() {
        let kept = Authority::from_pem("branch", NO_KEY_IDENTIFIER, &some_key()).unwrap();
        assert_eq!(kept.cert_pem(), NO_KEY_IDENTIFIER);
        let interceptor = Interceptor::new(kept, None).unwrap();
        for host in ["api.example.com", "127.0.0.1", "::1"] {
            let leaf = interceptor.leaf(host).unwrap();
            assert_eq!(authority_key_id(leaf.der()), None, "{host}");
        }
    }

    /// A kept file whose PEM block holds bytes that are not a certificate
    /// is accepted, as it always was, and what its authority signs carries
    /// no Authority Key Identifier, since there is none to read.
    #[test]
    fn a_kept_certificate_that_cannot_be_read_signs_without_a_key_identifier() {
        let not_a_certificate = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let kept = Authority::from_pem("branch", not_a_certificate, &some_key()).unwrap();
        assert_eq!(kept.cert_pem(), not_a_certificate);
        let interceptor = Interceptor::new(kept, None).unwrap();
        let leaf = interceptor.leaf("api.example.com").unwrap();
        assert_eq!(authority_key_id(leaf.der()), None);
    }

    /// A kept certificate with another subject than Branchyard's is
    /// accepted, and what its authority signs is issued under Branchyard's
    /// name for the owner, as it always was.
    #[test]
    fn a_kept_certificate_with_another_subject_signs_under_the_owners_name() {
        use x509_cert::der::Decode as _;
        let (cert_pem, key_pem) = foreign_authority("Some Other Authority", None);
        let kept = Authority::from_pem("branch", &cert_pem, &key_pem).unwrap();
        let interceptor = Interceptor::new(kept, None).unwrap();
        let leaf = interceptor.leaf("api.example.com").unwrap();
        let ours = Authority::generate("branch").unwrap();
        let ours = CertificateDer::from_pem_slice(ours.cert_pem().as_bytes()).unwrap();
        let ours = x509_cert::Certificate::from_der(&ours).unwrap();
        let leaf = x509_cert::Certificate::from_der(leaf.der()).unwrap();
        assert_eq!(leaf.tbs_certificate.issuer, ours.tbs_certificate.subject);
    }

    #[test]
    fn an_authority_round_trips_through_pem() {
        let authority = Authority::generate("branch").unwrap();
        assert!(authority
            .cert_pem()
            .starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(authority
            .key_pem()
            .starts_with("-----BEGIN PRIVATE KEY-----"));
        let again =
            Authority::from_pem("branch", authority.cert_pem(), authority.key_pem()).unwrap();
        assert_eq!(again.cert_pem(), authority.cert_pem());
        assert!(Authority::from_pem("branch", "nope", authority.key_pem()).is_err());
        assert!(Authority::from_pem("branch", authority.cert_pem(), "nope").is_err());
        let interceptor = Interceptor::new(again, Some(authority.cert_pem())).unwrap();
        assert_eq!(interceptor.authority_pem(), authority.cert_pem());
        assert!(Interceptor::new(
            Authority::generate("x").unwrap(),
            Some("-----BEGIN CERTIFICATE-----\nnope\n-----END CERTIFICATE-----\n")
        )
        .is_err());
        // One configuration per host, for names and addresses alike.
        let a = interceptor.server("api.example.com").unwrap();
        let b = interceptor.server("api.example.com").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        interceptor.server("127.0.0.1").unwrap();
        interceptor.server("::1").unwrap();
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
