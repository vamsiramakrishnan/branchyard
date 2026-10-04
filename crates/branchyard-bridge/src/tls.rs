//! TLS for bridge connections, with rustls and its `ring` provider.
//!
//! The host reaches a bridge through a router over `https://` or `wss://`
//! ([`ClientTls`]); the bridge itself serves TLS ([`ServerTls`]) where the
//! router passes TLS through rather than terminating it.
//!
//! A TLS connection is handshaken on the calling thread, then a thread of
//! its own moves bytes between the TLS session on the socket and one end
//! of a local socket pair, whose other end is returned as a
//! [`Stream::Tls`]. The rest of the bridge therefore reads and writes a
//! plain blocking stream it can clone and shut down per direction, as it
//! does for TCP, and one direction never waits on the other.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, Connection, RootCertStore, ServerConfig};

use crate::stream::Stream;

/// Plaintext held for the application before the peer is read further.
const BUFFER_LIMIT: usize = 256 * 1024;
/// How long a connection whose application is gone waits for the peer to
/// finish.
const LINGER: Duration = Duration::from_secs(5);

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn certificates(pem: &[u8], what: &str) -> io::Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| invalid(format!("{what}: {e}")))?;
    if certs.is_empty() {
        return Err(invalid(format!("{what} holds no PEM certificate")));
    }
    Ok(certs)
}

fn read(path: &Path, what: &str) -> io::Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("could not read the {what} {}: {e}", path.display()),
        )
    })
}

/// How a host verifies a bridge or router: which certificate authorities
/// it trusts.
#[derive(Clone)]
pub struct ClientTls {
    config: Arc<ClientConfig>,
}

impl std::fmt::Debug for ClientTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientTls").finish_non_exhaustive()
    }
}

impl ClientTls {
    fn with_roots(roots: RootCertStore) -> io::Result<ClientTls> {
        let config = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| io::Error::other(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(ClientTls {
            config: Arc::new(config),
        })
    }

    /// Trust the Mozilla root certificates bundled at build time
    /// (`webpki-roots`), not the host's certificate store.
    #[allow(clippy::expect_used)] // ratchet: branchyard-bridge
    pub fn public_roots() -> ClientTls {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        ClientTls::with_roots(roots).expect("the default protocol versions are supported")
    }

    /// Trust only the certificate authorities in `pem`.
    pub fn from_ca_pem(pem: &[u8]) -> io::Result<ClientTls> {
        let mut roots = RootCertStore::empty();
        for cert in certificates(pem, "the CA file")? {
            roots
                .add(cert)
                .map_err(|e| invalid(format!("the CA file holds an unusable certificate: {e}")))?;
        }
        ClientTls::with_roots(roots)
    }

    /// Trust only the certificate authorities in the PEM file at `path`.
    pub fn from_ca_file(path: &Path) -> io::Result<ClientTls> {
        ClientTls::from_ca_pem(&read(path, "CA file")?)
            .map_err(|e| io::Error::new(e.kind(), format!("CA file {}: {e}", path.display())))
    }
}

/// A bridge's certificate chain and key, to serve TLS itself.
#[derive(Clone)]
pub struct ServerTls {
    config: Arc<ServerConfig>,
}

impl std::fmt::Debug for ServerTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerTls").finish_non_exhaustive()
    }
}

impl ServerTls {
    /// From a PEM certificate chain (leaf first) and a PEM private key.
    pub fn from_pem(chain: &[u8], key: &[u8]) -> io::Result<ServerTls> {
        let chain = certificates(chain, "the certificate file")?;
        let key = PrivateKeyDer::from_pem_slice(key)
            .map_err(|e| invalid(format!("the key file: {e}")))?;
        let config = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| io::Error::other(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| {
                invalid(format!(
                    "the certificate and key do not form an identity: {e}"
                ))
            })?;
        Ok(ServerTls {
            config: Arc::new(config),
        })
    }

    pub fn from_pem_files(chain: &Path, key: &Path) -> io::Result<ServerTls> {
        ServerTls::from_pem(&read(chain, "certificate file")?, &read(key, "key file")?)
    }
}

/// Handshake as a client with `host` (a DNS name or an IP address) over
/// `tcp`, waiting at most `timeout` for the server.
pub fn connect(
    tcp: TcpStream,
    host: &str,
    tls: &ClientTls,
    timeout: Duration,
) -> io::Result<Stream> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let name = ServerName::try_from(host.to_owned())
        .map_err(|_| invalid(format!("{host} is not a valid TLS server name")))?;
    let conn = ClientConnection::new(tls.config.clone(), name)
        .map_err(|e| io::Error::other(format!("could not start TLS: {e}")))?;
    establish(Connection::Client(conn), tcp, timeout)
        .map_err(|e| io::Error::new(e.kind(), format!("TLS with {host} failed: {e}")))
}

/// Handshake as a server over `tcp`, waiting at most `timeout` for the
/// client.
pub fn accept(tcp: TcpStream, tls: &ServerTls, timeout: Duration) -> io::Result<Stream> {
    let conn = rustls::ServerConnection::new(tls.config.clone())
        .map_err(|e| io::Error::other(format!("could not start TLS: {e}")))?;
    establish(Connection::Server(conn), tcp, timeout)
}

fn establish(mut conn: Connection, mut tcp: TcpStream, timeout: Duration) -> io::Result<Stream> {
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp)?;
    }
    while conn.wants_write() {
        conn.write_tls(&mut tcp)?;
    }
    tcp.set_read_timeout(None)?;
    tcp.set_write_timeout(None)?;
    let (app, inner) = UnixStream::pair()?;
    thread::Builder::new()
        .name("bridge-tls".into())
        .spawn(move || pump(conn, tcp, inner))?;
    Ok(Stream::Tls(app))
}

/// Drain decrypted bytes into `out`; false once the peer has closed.
fn drain(conn: &mut Connection, out: &mut Vec<u8>, buf: &mut [u8]) -> bool {
    loop {
        match conn.reader().read(buf) {
            Ok(0) => return false,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        }
    }
}

/// Move bytes between the TLS session on `tcp` and the local end `app`
/// until both directions have ended.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-bridge
fn pump(mut conn: Connection, mut tcp: TcpStream, mut app: UnixStream) {
    if tcp.set_nonblocking(true).is_err() || app.set_nonblocking(true).is_err() {
        return;
    }
    let mut buf = vec![0u8; 16 * 1024];
    // Plaintext from the peer, not yet taken by the application.
    let mut to_app = Vec::new();
    // The peer may still send.
    let mut peer_open = drain(&mut conn, &mut to_app, &mut buf);
    // The application may still send.
    let mut app_open = true;
    // The application still takes what the peer sends.
    let mut deliver = true;
    let mut app_shut = false;
    let mut tcp_shut = false;
    // Once the application is gone, how long to keep reading (and
    // discarding) what the peer still sends, so closing the socket with
    // unread data does not reset the connection before the peer has read
    // what was sent to it.
    let mut linger: Option<Instant> = None;
    loop {
        if !deliver {
            to_app.clear();
        }
        if deliver && !peer_open && to_app.is_empty() && !app_shut {
            let _ = app.shutdown(Shutdown::Write);
            app_shut = true;
        }
        if !app_open && !tcp_shut && !conn.wants_write() {
            let _ = tcp.shutdown(Shutdown::Write);
            tcp_shut = true;
        }
        if tcp_shut && !peer_open && (to_app.is_empty() || !deliver) {
            return;
        }
        if tcp_shut && !deliver {
            let deadline = *linger.get_or_insert_with(|| Instant::now() + LINGER);
            if Instant::now() >= deadline {
                return;
            }
        }
        let mut tcp_events = 0;
        if peer_open && (to_app.len() < BUFFER_LIMIT || !deliver) {
            tcp_events |= libc::POLLIN;
        }
        if conn.wants_write() {
            tcp_events |= libc::POLLOUT;
        }
        let mut app_events = 0;
        if app_open && !conn.wants_write() {
            app_events |= libc::POLLIN;
        }
        if deliver && !to_app.is_empty() {
            app_events |= libc::POLLOUT;
        }
        if tcp_events == 0 && app_events == 0 {
            return;
        }
        // A descriptor of -1 is skipped, so nothing unrequested wakes us.
        let mut fds = [
            libc::pollfd {
                fd: if tcp_events == 0 { -1 } else { tcp.as_raw_fd() },
                events: tcp_events,
                revents: 0,
            },
            libc::pollfd {
                fd: if app_events == 0 { -1 } else { app.as_raw_fd() },
                events: app_events,
                revents: 0,
            },
        ];
        let timeout = match linger {
            None => -1,
            Some(deadline) => deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32,
        };
        // SAFETY: two valid pollfd structures.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        let (tcp_ready, app_ready) = (fds[0].revents, fds[1].revents);
        let wake = libc::POLLHUP | libc::POLLERR;

        if tcp_events & libc::POLLIN != 0 && tcp_ready & (libc::POLLIN | wake) != 0 {
            match conn.read_tls(&mut tcp) {
                Ok(0) => peer_open = false,
                Ok(_) => {
                    if conn.process_new_packets().is_err() {
                        // Send the alert rustls queued, then give up.
                        let _ = tcp.set_nonblocking(false);
                        while conn.wants_write() && conn.write_tls(&mut tcp).is_ok() {}
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => peer_open = false,
            }
            if !drain(&mut conn, &mut to_app, &mut buf) {
                peer_open = false;
            }
        }
        if tcp_events & libc::POLLOUT != 0 && tcp_ready & (libc::POLLOUT | wake) != 0 {
            while conn.wants_write() {
                match conn.write_tls(&mut tcp) {
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    // The peer is gone; so is this connection.
                    Err(_) => return,
                }
            }
        }
        if app_events & libc::POLLIN != 0 && app_ready & (libc::POLLIN | wake) != 0 {
            match app.read(&mut buf) {
                Ok(0) => {
                    app_open = false;
                    conn.send_close_notify();
                }
                Ok(n) => {
                    if conn.writer().write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    app_open = false;
                    conn.send_close_notify();
                }
            }
        }
        if app_events & libc::POLLOUT != 0 && app_ready & (libc::POLLOUT | wake) != 0 {
            match app.write(&to_app) {
                Ok(n) => {
                    to_app.drain(..n);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                // The application stopped reading.
                Err(_) => deliver = false,
            }
        }
        if app_ready & libc::POLLHUP != 0 {
            // The application closed its end: nothing more reaches it, but
            // what it wrote before closing is still read and sent.
            deliver = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    fn identity(ca: &rcgen::Issuer<'_, rcgen::KeyPair>) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params =
            rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
        let cert = params.signed_by(&key, ca).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    fn authority() -> (String, rcgen::Issuer<'static, rcgen::KeyPair>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), rcgen::Issuer::new(params, key))
    }

    #[test]
    fn bytes_cross_both_ways_and_half_closes_propagate() {
        let (ca_pem, ca) = authority();
        let (cert, key) = identity(&ca);
        let server_tls = ServerTls::from_pem(cert.as_bytes(), key.as_bytes()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut stream = accept(tcp, &server_tls, Duration::from_secs(10)).unwrap();
            let mut received = Vec::new();
            stream.read_to_end(&mut received).unwrap();
            stream.write_all(&received).unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
        });
        let tls = ClientTls::from_ca_pem(ca_pem.as_bytes()).unwrap();
        let tcp = TcpStream::connect(address).unwrap();
        let mut stream = connect(tcp, "127.0.0.1", &tls, Duration::from_secs(10)).unwrap();
        let sent: Vec<u8> = (0..1_000_000u32).map(|i| i as u8).collect();
        let mut writer = stream.try_clone().unwrap();
        let writing = {
            let sent = sent.clone();
            thread::spawn(move || {
                writer.write_all(&sent).unwrap();
                writer.shutdown(Shutdown::Write).unwrap();
            })
        };
        let mut echoed = Vec::new();
        stream.read_to_end(&mut echoed).unwrap();
        writing.join().unwrap();
        assert_eq!(echoed.len(), sent.len());
        assert!(echoed == sent, "content differs");
        server.join().unwrap();
    }

    #[test]
    fn a_server_from_another_authority_is_refused() {
        let (_, ca) = authority();
        let (other_pem, _) = authority();
        let (cert, key) = identity(&ca);
        let server_tls = ServerTls::from_pem(cert.as_bytes(), key.as_bytes()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            accept(tcp, &server_tls, Duration::from_secs(10)).is_err()
        });
        let tls = ClientTls::from_ca_pem(other_pem.as_bytes()).unwrap();
        let tcp = TcpStream::connect(address).unwrap();
        let error = connect(tcp, "127.0.0.1", &tls, Duration::from_secs(10)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        assert!(error.to_string().contains("certificate"), "{error}");
        assert!(
            server.join().unwrap(),
            "the server saw a completed handshake"
        );

        assert!(ClientTls::from_ca_pem(b"not a certificate").is_err());
        assert!(ServerTls::from_pem(cert.as_bytes(), b"no key").is_err());
    }
}
