//! The egress proxy against local listeners, a local process confined to
//! it, and connections it looks inside.

#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use branchyard_runtime::egress::{Authority, Decision, Interceptor, Proxy, Verdict};
use branchyard_runtime::LocalProvider;
use branchyard_sandbox::ExecSpec;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, StreamOwned};

const WAIT: Duration = Duration::from_secs(10);

/// A listener that answers each connection with `HTTP 200` and sends the
/// request head it received (and a `Content-Length` body, after a blank
/// line) to the test.
struct Upstream {
    port: u16,
    heads: Receiver<String>,
}

/// Read a request head from `reader`, then its `Content-Length` body if
/// any: the head's lines, a blank line, the body.
fn read_request(reader: &mut impl BufRead) -> String {
    let mut head = String::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
        head.push_str(&line);
    }
    if length > 0 {
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).is_ok() {
            head.push_str("\r\n");
            head.push_str(&String::from_utf8_lossy(&body));
        }
    }
    head
}

/// A listener on a free port of this host's loopback, and its port.
fn listen() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

fn upstream() -> Upstream {
    let (listener, port) = listen();
    let (sender, heads) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let sender = sender.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let head = read_request(&mut reader);
                let body = format!("upstream {port}\n");
                branchyard_support::best_effort(
                    "write the origin's answer",
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    ),
                );
                branchyard_support::best_effort(
                    "hand the request head to the test",
                    sender.send(head),
                );
            });
        }
    });
    Upstream { port, heads }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// An origin speaking TLS with a self-signed certificate for `127.0.0.1`,
/// answering like [`upstream`]; and its certificate, PEM, for whoever must
/// trust it.
fn tls_upstream() -> (Upstream, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    let cert = params.self_signed(&key).unwrap();
    let pem = cert.pem();
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .unwrap();
    let config = Arc::new(config);
    let (listener, port) = listen();
    let (sender, heads) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let sender = sender.clone();
            let config = config.clone();
            thread::spawn(move || {
                let conn = rustls::ServerConnection::new(config).unwrap();
                let mut tls = StreamOwned::new(conn, stream);
                let mut reader = BufReader::new(&mut tls);
                let head = read_request(&mut reader);
                let body = format!("upstream {port}\n");
                branchyard_support::best_effort(
                    "write the origin's answer",
                    write!(
                        tls,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ),
                );
                tls.conn.send_close_notify();
                branchyard_support::best_effort("flush the origin's answer", tls.flush());
                branchyard_support::best_effort(
                    "hand the request head to the test",
                    sender.send(head),
                );
            });
        }
    });
    (Upstream { port, heads }, pem)
}

/// A proxy allowing exactly `allowed` ports of 127.0.0.1 (a rule naming
/// the address, so loopback is reachable), and `name` on any port without
/// loopback; and what it reports.
fn proxy(allowed: &[u16], name: &str) -> (Proxy, SocketAddr, Arc<Mutex<Vec<Decision>>>) {
    let allowed = allowed.to_vec();
    let name = name.to_owned();
    let reports = Arc::new(Mutex::new(Vec::new()));
    let sink = reports.clone();
    let proxy = Proxy::new(
        move |host, port, _| match (host, port) {
            ("127.0.0.1", p) if allowed.contains(&p) => Verdict {
                allowed: true,
                rule: Some(format!("127.0.0.1:{p}")),
                loopback: true,
                inspect: false,
            },
            (h, _) if h == name => Verdict {
                allowed: true,
                rule: Some(name.clone()),
                loopback: false,
                inspect: false,
            },
            _ => Verdict::default(),
        },
        move |decision| sink.lock().unwrap().push(decision.clone()),
    );
    let address = proxy.listen_loopback().unwrap();
    (proxy, address, reports)
}

/// A proxy whose one rule is `GET|POST 127.0.0.1:PORT/ok/*`: the host is
/// inspected, and a request inside is allowed when its method is `GET` or
/// `POST` and its path is under `/ok`. The proxy signs with a fresh
/// authority and trusts `roots` (an origin's certificate) besides the
/// Mozilla roots.
fn inspecting(
    port: u16,
    roots: Option<&str>,
) -> (Proxy, SocketAddr, Authority, Arc<Mutex<Vec<Decision>>>) {
    let reports = Arc::new(Mutex::new(Vec::new()));
    let sink = reports.clone();
    let rule = format!("GET|POST 127.0.0.1:{port}/ok/*");
    let proxy = Proxy::new(
        move |host, p, request| {
            if host != "127.0.0.1" || p != port {
                return Verdict::default();
            }
            let allowed = match request {
                None => true,
                Some((method, path)) => {
                    matches!(method, "GET" | "POST")
                        && path.trim_start_matches('/').split(['/', '?']).next() == Some("ok")
                        && path.trim_start_matches('/').contains('/')
                }
            };
            Verdict {
                allowed,
                rule: Some(rule.clone()),
                loopback: true,
                inspect: true,
            }
        },
        move |decision| sink.lock().unwrap().push(decision.clone()),
    );
    let authority = Authority::generate("test").unwrap();
    let again = Authority::from_pem("test", authority.cert_pem(), authority.key_pem()).unwrap();
    proxy
        .intercept(Interceptor::new(again, roots).unwrap())
        .unwrap();
    let address = proxy.listen_loopback().unwrap();
    (proxy, address, authority, reports)
}

/// Send `request` to the proxy and read everything it answers.
fn ask(proxy: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(proxy).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    branchyard_support::best_effort(
        "read the proxy's answer (it closes after answering)",
        stream.read_to_string(&mut answer),
    );
    answer
}

/// `CONNECT 127.0.0.1:port` through the proxy; returns the stream once the
/// proxy says the connection is established.
fn connect(proxy: SocketAddr, port: u16) -> TcpStream {
    let mut stream = TcpStream::connect(proxy).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    write!(
        stream,
        "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
    )
    .unwrap();
    let mut established = Vec::new();
    while !established.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        assert_eq!(stream.read(&mut byte).unwrap(), 1, "{established:?}");
        established.push(byte[0]);
    }
    assert!(
        established.starts_with(b"HTTP/1.1 200 Connection established"),
        "{}",
        String::from_utf8_lossy(&established)
    );
    stream
}

/// A TLS client trusting the certificates in `roots_pem` only.
fn client(roots_pem: &str) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(roots_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    Arc::new(
        ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// Through an inspected `CONNECT`, over TLS trusting `roots_pem`, send
/// `request` and read the whole answer; an `Err` is the TLS failure.
fn ask_inside(
    proxy: SocketAddr,
    port: u16,
    roots_pem: &str,
    request: &str,
) -> Result<String, String> {
    let stream = connect(proxy, port);
    let name = ServerName::try_from("127.0.0.1").map_err(|e| e.to_string())?;
    let conn = ClientConnection::new(client(roots_pem), name).map_err(|e| e.to_string())?;
    let mut tls = StreamOwned::new(conn, stream);
    tls.write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut answer = String::new();
    match tls.read_to_string(&mut answer) {
        Ok(_) => Ok(answer),
        // A clean close without a close notice still ends the answer.
        Err(e) if !answer.is_empty() => {
            let _ = e;
            Ok(answer)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Wait until the proxy has reported `count` decisions: a connection's
/// last report follows the client's view of it by a moment.
fn reported(reports: &Arc<Mutex<Vec<Decision>>>, count: usize) {
    branchyard_testkit::wait::until(&format!("{count} decisions reported"), || {
        reports.lock().unwrap().len() >= count
    });
}

fn summary(reports: &[Decision]) -> Vec<(String, Option<String>, bool, Option<String>)> {
    reports
        .iter()
        .map(|d| {
            (
                d.method.clone(),
                d.path.clone(),
                d.allowed,
                d.reason.clone(),
            )
        })
        .collect()
}

#[test]
fn plain_http_is_forwarded_to_an_allowed_host_and_refused_elsewhere() {
    let (allowed, denied) = (upstream(), upstream());
    let (_proxy, address, reports) = proxy(&[allowed.port], "unused.invalid");
    let answer = ask(
        address,
        &format!(
            "GET http://127.0.0.1:{}/path?q=1 HTTP/1.1\r\nHost: evil.example\r\n\
             Proxy-Authorization: Basic eDp5\r\nConnection: keep-alive\r\nX-Kept: 1\r\n\r\n",
            allowed.port
        ),
    );
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    assert!(answer.ends_with(&format!("upstream {}\n", allowed.port)));
    let head = allowed.heads.recv_timeout(WAIT).unwrap();
    assert!(head.starts_with("GET /path?q=1 HTTP/1.1\r\n"), "{head}");
    assert!(head.contains(&format!("Host: 127.0.0.1:{}\r\n", allowed.port)));
    assert!(head.contains("Connection: close\r\n"), "{head}");
    assert!(head.contains("X-Kept: 1\r\n"));
    assert!(!head.contains("evil.example") && !head.contains("Proxy-"));

    let answer = ask(
        address,
        &format!(
            "GET http://127.0.0.1:{}/ HTTP/1.1\r\nHost: x\r\n\r\n",
            denied.port
        ),
    );
    assert!(answer.starts_with("HTTP/1.1 403 Forbidden"), "{answer}");
    assert!(answer.contains("X-Branchyard-Egress: denied"));
    assert!(answer.contains(&format!(
        "egress policy does not allow GET 127.0.0.1:{}/",
        denied.port
    )));
    assert!(
        denied.heads.try_recv().is_err(),
        "the denied host was reached"
    );

    let reports = reports.lock().unwrap();
    assert_eq!(reports.len(), 2);
    assert_eq!(
        (reports[0].method.as_str(), reports[0].allowed),
        ("GET", true)
    );
    assert_eq!(reports[0].path.as_deref(), Some("/path?q=1"));
    assert_eq!(
        reports[0].rule.as_deref(),
        Some(&*format!("127.0.0.1:{}", allowed.port))
    );
    assert_eq!(
        (
            reports[1].host.as_str(),
            reports[1].port,
            reports[1].allowed
        ),
        ("127.0.0.1", denied.port, false)
    );
    assert_eq!(reports[1].reason.as_deref(), Some("no rule allows it"));
}

#[test]
fn connect_tunnels_to_an_allowed_port_only() {
    let (allowed, denied) = (upstream(), upstream());
    let (_proxy, address, reports) = proxy(&[allowed.port], "unused.invalid");
    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    // The tunnelled request rides in the same write as the CONNECT.
    write!(
        stream,
        "CONNECT 127.0.0.1:{0} HTTP/1.1\r\nHost: 127.0.0.1:{0}\r\n\r\nGET /inside HTTP/1.1\r\n\r\n",
        allowed.port
    )
    .unwrap();
    let mut answer = String::new();
    branchyard_support::best_effort(
        "read the proxy's answer (it closes after answering)",
        stream.read_to_string(&mut answer),
    );
    assert!(
        answer.starts_with("HTTP/1.1 200 Connection established\r\n\r\nHTTP/1.1 200 OK"),
        "{answer}"
    );
    assert_eq!(
        allowed.heads.recv_timeout(WAIT).unwrap(),
        "GET /inside HTTP/1.1\r\n"
    );

    let answer = ask(
        address,
        &format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n", denied.port),
    );
    assert!(answer.starts_with("HTTP/1.1 403 Forbidden"), "{answer}");
    assert!(denied.heads.try_recv().is_err());

    let reports = reports.lock().unwrap();
    let summary: Vec<_> = reports
        .iter()
        .map(|d| (d.method.as_str(), d.port, d.allowed, d.path.clone()))
        .collect();
    assert_eq!(
        summary,
        [
            ("CONNECT", allowed.port, true, None),
            ("CONNECT", denied.port, false, None)
        ]
    );
}

#[test]
fn an_inspected_connection_allows_only_matching_requests() {
    let (origin, origin_pem) = tls_upstream();
    let (_proxy, address, authority, reports) = inspecting(origin.port, Some(&origin_pem));
    let ca = authority.cert_pem();
    let port = origin.port;
    let host = format!("Host: 127.0.0.1:{port}\r\n");

    // Allowed: the request reaches the origin over TLS, in origin form,
    // closing after; the answer comes back inside the client's TLS.
    let answer = ask_inside(
        address,
        port,
        ca,
        &format!("GET /ok/1?x=1 HTTP/1.1\r\n{host}X-Kept: 1\r\nProxy-Authorization: no\r\n\r\n"),
    )
    .unwrap();
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    assert!(answer.ends_with(&format!("upstream {port}\n")), "{answer}");
    let head = origin.heads.recv_timeout(WAIT).unwrap();
    assert!(head.starts_with("GET /ok/1?x=1 HTTP/1.1\r\n"), "{head}");
    assert!(
        head.contains(&host) && head.contains("X-Kept: 1\r\n"),
        "{head}"
    );
    assert!(head.contains("Connection: close\r\n"), "{head}");
    assert!(!head.contains("Proxy-"), "{head}");

    // A body goes through whole.
    let answer = ask_inside(
        address,
        port,
        ca,
        &format!("POST /ok/up HTTP/1.1\r\n{host}Content-Length: 5\r\n\r\nhello"),
    )
    .unwrap();
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    let head = origin.heads.recv_timeout(WAIT).unwrap();
    assert!(head.starts_with("POST /ok/up HTTP/1.1\r\n"), "{head}");
    assert!(head.ends_with("\r\n\r\nhello"), "{head}");

    // The wrong method, a path outside the rule, and a Host header naming
    // another host: refused inside the connection, the origin untouched.
    for (request, why) in [
        (
            format!("DELETE /ok/1 HTTP/1.1\r\n{host}\r\n"),
            format!("does not allow DELETE 127.0.0.1:{port}/ok/1"),
        ),
        (
            format!("GET /other HTTP/1.1\r\n{host}\r\n"),
            format!("does not allow GET 127.0.0.1:{port}/other"),
        ),
        (
            "GET /ok/2 HTTP/1.1\r\nHost: evil.example\r\n\r\n".to_owned(),
            "Host header names evil.example".to_owned(),
        ),
    ] {
        let answer = ask_inside(address, port, ca, &request).unwrap();
        assert!(answer.starts_with("HTTP/1.1 403 Forbidden"), "{answer}");
        assert!(answer.contains("X-Branchyard-Egress: denied"), "{answer}");
        assert!(answer.contains(&why), "{request}: {answer}");
    }
    assert!(
        origin.heads.try_recv().is_err(),
        "a refused request reached the origin"
    );

    // A client that does not trust the authority gets no further than the
    // handshake, and the report says so.
    let other = Authority::generate("other").unwrap();
    let failed = ask_inside(
        address,
        port,
        other.cert_pem(),
        &format!("GET /ok/1 HTTP/1.1\r\n{host}\r\n"),
    );
    assert!(failed.is_err(), "{failed:?}");

    reported(&reports, 11);
    let reports = reports.lock().unwrap();
    let rule = format!("GET|POST 127.0.0.1:{port}/ok/*");
    assert!(
        reports
            .iter()
            .all(|d| d.host == "127.0.0.1" && d.port == port),
        "{reports:?}"
    );
    assert!(
        reports
            .iter()
            .all(|d| d.rule.as_deref() == Some(rule.as_str())),
        "{reports:?}"
    );
    let inspected = Some("inspected".to_owned());
    let expected = vec![
        ("CONNECT".to_owned(), None, true, inspected.clone()),
        ("GET".to_owned(), Some("/ok/1?x=1".to_owned()), true, None),
        ("CONNECT".to_owned(), None, true, inspected.clone()),
        ("POST".to_owned(), Some("/ok/up".to_owned()), true, None),
        ("CONNECT".to_owned(), None, true, inspected.clone()),
        (
            "DELETE".to_owned(),
            Some("/ok/1".to_owned()),
            false,
            Some("no rule allows it".to_owned()),
        ),
        ("CONNECT".to_owned(), None, true, inspected.clone()),
        (
            "GET".to_owned(),
            Some("/other".to_owned()),
            false,
            Some("no rule allows it".to_owned()),
        ),
        ("CONNECT".to_owned(), None, true, inspected.clone()),
        (
            "GET".to_owned(),
            Some("/ok/2".to_owned()),
            false,
            Some("its Host header names evil.example, not the connection's host".to_owned()),
        ),
    ];
    let mut got = summary(&reports);
    let last = got.pop().unwrap();
    assert_eq!(got, expected);
    assert_eq!((last.0.as_str(), last.1, last.2), ("CONNECT", None, true));
    let reason = last.3.unwrap();
    assert!(
        reason.contains("TLS handshake with the harness failed")
            && reason.contains("certificate authority"),
        "{reason}"
    );
}

#[test]
fn plain_http_inside_an_inspected_tunnel_is_checked_too() {
    let origin = upstream();
    let (_proxy, address, _authority, reports) = inspecting(origin.port, None);
    let port = origin.port;
    // The request rides in the same write as the CONNECT.
    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    write!(
        stream,
        "CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\nGET /ok/plain HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
    )
    .unwrap();
    let mut answer = String::new();
    branchyard_support::best_effort(
        "read the proxy's answer (it closes after answering)",
        stream.read_to_string(&mut answer),
    );
    assert!(
        answer.starts_with("HTTP/1.1 200 Connection established\r\n\r\nHTTP/1.1 200 OK"),
        "{answer}"
    );
    assert!(answer.ends_with(&format!("upstream {port}\n")), "{answer}");
    let head = origin.heads.recv_timeout(WAIT).unwrap();
    assert!(head.starts_with("GET /ok/plain HTTP/1.1\r\n"), "{head}");
    assert!(head.contains("Connection: close\r\n"), "{head}");

    // Sent after the proxy's answer, without a Host header: one is added.
    let mut stream = connect(address, port);
    stream.write_all(b"GET /ok/later HTTP/1.0\r\n\r\n").unwrap();
    let mut answer = String::new();
    branchyard_support::best_effort(
        "read the proxy's answer (it closes after answering)",
        stream.read_to_string(&mut answer),
    );
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    let head = origin.heads.recv_timeout(WAIT).unwrap();
    assert!(
        head.contains(&format!("Host: 127.0.0.1:{port}\r\n")),
        "{head}"
    );

    // Refused inside: the origin is not reached.
    let mut stream = connect(address, port);
    write!(
        stream,
        "DELETE /ok/plain HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
    )
    .unwrap();
    let mut answer = String::new();
    branchyard_support::best_effort(
        "read the proxy's answer (it closes after answering)",
        stream.read_to_string(&mut answer),
    );
    assert!(answer.starts_with("HTTP/1.1 403 Forbidden"), "{answer}");
    assert!(origin.heads.try_recv().is_err());

    // Nothing inside: reported as such, nothing reached.
    let stream = connect(address, port);
    drop(stream);

    reported(&reports, 7);
    let reports = reports.lock().unwrap();
    let got = summary(&reports);
    assert_eq!(
        got,
        [
            (
                "CONNECT".to_owned(),
                None,
                true,
                Some("inspected".to_owned())
            ),
            ("GET".to_owned(), Some("/ok/plain".to_owned()), true, None),
            (
                "CONNECT".to_owned(),
                None,
                true,
                Some("inspected".to_owned())
            ),
            ("GET".to_owned(), Some("/ok/later".to_owned()), true, None),
            (
                "CONNECT".to_owned(),
                None,
                true,
                Some("inspected".to_owned())
            ),
            (
                "DELETE".to_owned(),
                Some("/ok/plain".to_owned()),
                false,
                Some("no rule allows it".to_owned())
            ),
            (
                "CONNECT".to_owned(),
                None,
                true,
                Some("inspected, but nothing was sent inside".to_owned())
            ),
        ]
    );
}

#[test]
fn a_python_client_trusts_the_authority_through_its_bundle() {
    let (origin, origin_pem) = tls_upstream();
    let (_proxy, address, authority, reports) = inspecting(origin.port, Some(&origin_pem));
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("ca-bundle.pem");
    std::fs::write(&bundle, authority.cert_pem()).unwrap();
    let port = origin.port;
    let script = format!(
        "import urllib.error, urllib.request\n\
         print(urllib.request.urlopen('https://127.0.0.1:{port}/ok/py', timeout=10).read().decode().strip())\n\
         try:\n\
         \x20   urllib.request.urlopen(urllib.request.Request('https://127.0.0.1:{port}/ok/py', method='DELETE'), timeout=10)\n\
         except urllib.error.HTTPError as error:\n\
         \x20   print('denied', error.code, error.headers.get('X-Branchyard-Egress'))\n"
    );
    let out = std::process::Command::new("python3")
        .args(["-c", &script])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HTTPS_PROXY", format!("http://{address}"))
        .env("SSL_CERT_FILE", &bundle)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [format!("upstream {port}").as_str(), "denied 403 denied"],
        "{stderr}"
    );
    let head = origin.heads.recv_timeout(WAIT).unwrap();
    assert!(head.starts_with("GET /ok/py HTTP/1.1\r\n"), "{head}");
    assert!(origin.heads.try_recv().is_err());
    let reports = reports.lock().unwrap();
    let requests: Vec<_> = reports
        .iter()
        .filter(|d| d.method != "CONNECT")
        .map(|d| (d.method.as_str(), d.path.as_deref(), d.allowed))
        .collect();
    assert_eq!(
        requests,
        [
            ("GET", Some("/ok/py"), true),
            ("DELETE", Some("/ok/py"), false)
        ]
    );
}

#[test]
fn an_inspected_rule_needs_an_interceptor() {
    let origin = upstream();
    let reports = Arc::new(Mutex::new(Vec::new()));
    let sink = reports.clone();
    let port = origin.port;
    let proxy = Proxy::new(
        move |_, _, _| Verdict {
            allowed: true,
            rule: Some("GET 127.0.0.1/x".into()),
            loopback: true,
            inspect: true,
        },
        move |decision| sink.lock().unwrap().push(decision.clone()),
    );
    let address = proxy.listen_loopback().unwrap();
    let answer = ask(
        address,
        &format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n"),
    );
    assert!(answer.starts_with("HTTP/1.1 403 Forbidden"), "{answer}");
    assert!(
        answer.contains("cannot look inside the connection"),
        "{answer}"
    );
    assert!(origin.heads.try_recv().is_err());
    let reports = reports.lock().unwrap();
    assert_eq!(reports.len(), 1);
    assert!(!reports[0].allowed);
    assert!(
        reports[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("no certificate authority"),
        "{reports:?}"
    );
    // The interceptor is given once.
    proxy
        .intercept(Interceptor::new(Authority::generate("a").unwrap(), None).unwrap())
        .unwrap();
    assert!(proxy
        .intercept(Interceptor::new(Authority::generate("b").unwrap(), None).unwrap())
        .is_err());
}

#[test]
fn a_name_that_leads_to_loopback_needs_a_rule_naming_the_address() {
    let target = upstream();
    // `localhost` is allowed by a rule that does not name an address.
    let (_proxy, address, reports) = proxy(&[], "localhost");
    let answer = ask(
        address,
        &format!("CONNECT localhost:{} HTTP/1.1\r\n\r\n", target.port),
    );
    assert!(answer.starts_with("HTTP/1.1 403 Forbidden"), "{answer}");
    assert!(
        answer.contains("resolves to a loopback address"),
        "{answer}"
    );
    assert!(target.heads.try_recv().is_err());
    let reports = reports.lock().unwrap();
    assert!(!reports[0].allowed);
    assert_eq!(reports[0].rule.as_deref(), Some("localhost"));
}

#[test]
fn what_is_not_a_proxy_request_is_refused_and_not_reported() {
    let (_proxy, address, reports) = proxy(&[], "unused.invalid");
    for (request, why) in [
        ("GET / HTTP/1.1\r\n\r\n", "absolute URI or CONNECT"),
        ("GET https://x.example/ HTTP/1.1\r\n\r\n", "needs CONNECT"),
        ("CONNECT x.example HTTP/1.1\r\n\r\n", "host:port"),
        ("nonsense\r\n\r\n", "METHOD TARGET VERSION"),
    ] {
        let answer = ask(address, request);
        assert!(answer.starts_with("HTTP/1.1 400 Bad Request"), "{answer}");
        assert!(answer.contains(why), "{request}: {answer}");
    }
    assert!(reports.lock().unwrap().is_empty());
}

#[test]
fn dropping_the_proxy_stops_it() {
    let (proxy, address, _) = proxy(&[], "unused.invalid");
    drop(proxy);
    let refused = TcpStream::connect_timeout(&address, WAIT)
        .and_then(|mut s| {
            s.set_read_timeout(Some(WAIT))?;
            s.write_all(b"CONNECT a.example:1 HTTP/1.1\r\n\r\n")?;
            let mut text = String::new();
            s.read_to_string(&mut text)?;
            Ok(text)
        })
        .map_or(true, |text| text.is_empty());
    assert!(refused, "a dropped proxy still answered");
}

/// The reason this host cannot confine a process, when that is the
/// kernel's or its security module's doing; a failure of ours is a panic.
fn confinement_unavailable() -> Option<String> {
    let why = LocalProvider::confinement().err()?;
    if std::env::var(branchyard_runtime::ENV_EGRESS_NETNS).is_ok_and(|v| v == "off") {
        return Some(why);
    }
    // `unshare -rn` is the same request made by util-linux: if it works,
    // our probe should have too.
    let unshare = std::process::Command::new("unshare")
        .args(["-rn", "true"])
        .status();
    if unshare.is_ok_and(|s| s.success()) {
        panic!("unshare -rn works on this host but confinement does not: {why}");
    }
    Some(why)
}

#[test]
fn a_confined_process_reaches_only_its_listener() {
    if let Some(why) = confinement_unavailable() {
        eprintln!("SKIPPED a_confined_process_reaches_only_its_listener: {why}");
        return;
    }
    let host = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_port = host.local_addr().unwrap().port();
    let target = upstream();
    // The confined script asks for the target through the proxy (allowed)
    // and directly (no route), then tries the host listener directly.
    let script = format!(
        "import socket, urllib.request\n\
         opener = urllib.request.build_opener(urllib.request.ProxyHandler(\
         {{'http': 'http://127.0.0.1:3128'}}))\n\
         print(opener.open('http://127.0.0.1:{0}/via-proxy', timeout=5).read().decode().strip())\n\
         for port in ({0}, {1}):\n\
         \x20   try:\n\
         \x20       socket.create_connection(('127.0.0.1', port), timeout=5)\n\
         \x20       print('reached', port)\n\
         \x20   except OSError:\n\
         \x20       print('blocked', port)\n",
        target.port, host_port
    );
    let spec = ExecSpec {
        argv: vec!["python3".into(), "-c".into(), script],
        cwd: PathBuf::from("/"),
        env: BTreeMap::from([("PATH".into(), std::env::var_os("PATH").unwrap_or_default())]),
        writable: None,
    };
    let reports: Arc<Mutex<Vec<Decision>>> = Arc::default();
    let (mut process, listener) = LocalProvider::spawn_confined(&spec, 3128).unwrap();
    let confined = Proxy::new(
        {
            let port = target.port;
            move |host, p, _| Verdict {
                allowed: host == "127.0.0.1" && p == port,
                rule: Some("127.0.0.1".into()),
                loopback: true,
                inspect: false,
            }
        },
        {
            let reports = reports.clone();
            move |d| reports.lock().unwrap().push(d.clone())
        },
    );
    confined.serve(listener).unwrap();
    use branchyard_sandbox::Process;
    drop(process.take_stdin());
    let mut out = String::new();
    process
        .take_stdout()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let mut err = String::new();
    process
        .take_stderr()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let status = process.wait().unwrap();
    assert!(status.success(), "{status}: {err}");
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        [
            format!("upstream {}", target.port),
            format!("blocked {}", target.port),
            format!("blocked {host_port}"),
        ],
        "{err}"
    );
    host.set_nonblocking(true).unwrap();
    assert!(host.accept().is_err(), "the host listener was reached");
    let reports = reports.lock().unwrap();
    assert_eq!(reports.len(), 1);
    assert!(reports[0].allowed && reports[0].port == target.port);
}

#[test]
fn the_local_provider_declares_egress_where_it_can_confine() {
    use branchyard_sandbox::SandboxProvider;
    let declared = LocalProvider::new().capabilities().egress;
    match confinement_unavailable() {
        Some(why) => {
            assert!(!declared, "declared egress though: {why}");
            eprintln!("this host cannot confine a process ({why}); egress is not declared");
        }
        None => assert!(declared),
    }
}
