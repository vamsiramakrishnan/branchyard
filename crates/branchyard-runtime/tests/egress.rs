//! The egress proxy against local listeners, and a local process confined
//! to it.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use branchyard_runtime::egress::{Decision, Proxy, Verdict};
use branchyard_runtime::LocalProvider;
use branchyard_sandbox::ExecSpec;

const WAIT: Duration = Duration::from_secs(10);

/// A listener that answers each connection with `HTTP 200` and the request
/// head it received, and sends that head to the test.
struct Upstream {
    port: u16,
    heads: Receiver<String>,
}

fn upstream() -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, heads) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let sender = sender.clone();
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let body = format!("upstream {port}\n");
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = sender.send(head);
            });
        }
    });
    Upstream { port, heads }
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
        move |host, port| match (host, port) {
            ("127.0.0.1", p) if allowed.contains(&p) => Verdict {
                allowed: true,
                rule: Some(format!("127.0.0.1:{p}")),
                loopback: true,
            },
            (h, _) if h == name => Verdict {
                allowed: true,
                rule: Some(name.clone()),
                loopback: false,
            },
            _ => Verdict::default(),
        },
        move |decision| sink.lock().unwrap().push(decision.clone()),
    );
    let address = proxy.listen_loopback().unwrap();
    (proxy, address, reports)
}

/// Send `request` to the proxy and read everything it answers.
fn ask(proxy: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(proxy).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    answer
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
        "egress policy does not allow 127.0.0.1:{}",
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
    let _ = stream.read_to_string(&mut answer);
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
        .map(|d| (d.method.as_str(), d.port, d.allowed))
        .collect();
    assert_eq!(
        summary,
        [
            ("CONNECT", allowed.port, true),
            ("CONNECT", denied.port, false)
        ]
    );
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
    };
    let reports: Arc<Mutex<Vec<Decision>>> = Arc::default();
    let (mut process, listener) = LocalProvider::spawn_confined(&spec, 3128).unwrap();
    let confined = Proxy::new(
        {
            let port = target.port;
            move |host, p| Verdict {
                allowed: host == "127.0.0.1" && p == port,
                rule: Some("127.0.0.1".into()),
                loopback: true,
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
