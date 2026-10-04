//! The host side of the bridge: connect through a router to a sandbox's
//! bridge and run requests, with a [`Process`] for exec.

use branchyard_support::{CondvarExt as _, LockExt as _};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use branchyard_sandbox::{ExecSpec, ExitStatus, Process, ProviderError};

use crate::protocol::{ExecReport, Frame, CHUNK, SUBPROTOCOL};
use crate::stream::Stream;
use crate::tls::{self, ClientTls};
use crate::tree;
use crate::ws::{self, WsReader, WsWriter};

/// How long a teardown waits for the bridge to name the survivors.
const TEARDOWN_WAIT: Duration = Duration::from_secs(30);

/// What a bridge reported for [`Endpoint::status`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BridgeStatus {
    /// The execs still running, of any attempt.
    pub execs: Vec<ExecReport>,
    /// The bridge found its attempt state file changed behind its back at
    /// least once, and rewrote it from memory.
    pub tampered: bool,
}

/// Where a bridge is reached: an `http://` or `ws://` URL in the clear, or
/// an `https://` or `wss://` URL over TLS, as `scheme://host[:port][/path]`;
/// and the credential to present.
#[derive(Clone)]
pub struct Endpoint {
    /// `https` or `wss` when `secure`, else `http` or `ws`.
    scheme: String,
    secure: bool,
    /// Who to trust over TLS; the bundled public roots when unset.
    tls: Option<ClientTls>,
    host: String,
    port: u16,
    path: String,
    credential: String,
    /// How long connecting may take.
    pub connect_timeout: Duration,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("url", &self.url())
            .finish_non_exhaustive()
    }
}

impl Endpoint {
    /// Parse `url`. `https` and `wss` connect over TLS, trusting the
    /// bundled public roots unless [`Endpoint::with_tls`] says otherwise;
    /// `http` and `ws` connect in the clear, and whether that is acceptable
    /// is the caller's decision.
    pub fn new(url: &str, credential: impl Into<String>) -> io::Result<Endpoint> {
        let bad = |why: &str| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("bridge URL {url:?}: {why}"),
            )
        };
        let (scheme, secure, rest) = match url.split_once("://") {
            Some((scheme @ ("http" | "ws"), rest)) => (scheme, false, rest),
            Some((scheme @ ("https" | "wss"), rest)) => (scheme, true, rest),
            Some((scheme, _)) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "bridge URL {url:?}: the {scheme} scheme is not supported; use https, \
                         wss, http or ws"
                    ),
                ))
            }
            None => return Err(bad("expected https://host[:port][/path]")),
        };
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };
        if authority.is_empty() || authority.contains('@') {
            return Err(bad("expected a host without credentials"));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !host.ends_with(']') || authority.starts_with('[') => {
                let port = port.parse().map_err(|_| bad("bad port"))?;
                (host.to_owned(), port)
            }
            _ => (authority.to_owned(), if secure { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(bad("expected a host"));
        }
        Ok(Endpoint {
            scheme: scheme.to_owned(),
            secure,
            tls: None,
            host,
            port,
            path: path.to_owned(),
            credential: credential.into(),
            connect_timeout: Duration::from_secs(30),
        })
    }

    pub fn url(&self) -> String {
        format!("{}://{}:{}{}", self.scheme, self.host, self.port, self.path)
    }

    /// The host part of the URL: a name, an IPv4 address or a bracketed
    /// IPv6 address.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Whether connections use TLS.
    pub fn is_secure(&self) -> bool {
        self.secure
    }

    /// Verify the server with `tls` rather than the bundled public roots.
    /// Ignored for a URL in the clear.
    pub fn with_tls(mut self, tls: ClientTls) -> Endpoint {
        self.tls = Some(tls);
        self
    }

    /// The same credential presented at another URL, trusting the same
    /// authorities.
    pub fn with_url(&self, url: &str) -> io::Result<Endpoint> {
        Ok(Endpoint {
            connect_timeout: self.connect_timeout,
            tls: self.tls.clone(),
            ..Endpoint::new(url, self.credential.clone())?
        })
    }

    /// The same bridge with another credential.
    pub fn with_credential(&self, credential: impl Into<String>) -> Endpoint {
        Endpoint {
            credential: credential.into(),
            ..self.clone()
        }
    }

    /// A connection to the bridge or router, over TLS for a secure URL.
    fn stream(&self) -> io::Result<Stream> {
        let tcp = self.tcp()?;
        if !self.secure {
            return Ok(Stream::Tcp(tcp));
        }
        let fallback;
        let tls = match &self.tls {
            Some(tls) => tls,
            None => {
                fallback = ClientTls::public_roots();
                &fallback
            }
        };
        tls::connect(tcp, &self.host, tls, self.connect_timeout)
    }

    fn tcp(&self) -> io::Result<TcpStream> {
        let host = self.host.trim_start_matches('[').trim_end_matches(']');
        let mut last = io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} resolves to no address", self.host),
        );
        for address in (host, self.port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&address, self.connect_timeout) {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    return Ok(stream);
                }
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    fn connect(&self) -> io::Result<Connection> {
        let stream = self.stream()?;
        let authority = match (self.port, self.secure) {
            (80, false) | (443, true) => self.host.clone(),
            (port, _) => format!("{}:{port}", self.host),
        };
        let bearer = format!("Bearer {}", self.credential);
        let (reader, writer) = ws::client(
            stream,
            &authority,
            &self.path,
            SUBPROTOCOL,
            &[("Authorization", &bearer)],
        )?;
        Ok(Connection { reader, writer })
    }

    /// Whether the bridge answers its health check, `GET <path>healthz`.
    pub fn healthy(&self) -> bool {
        self.health().is_ok()
    }

    /// The bridge's health check, `GET <path>healthz`: `Ok` for a 200. A
    /// TLS failure, such as a certificate from an authority not trusted,
    /// is an [`io::ErrorKind::InvalidData`] error, which retrying does not
    /// fix.
    pub fn health(&self) -> io::Result<()> {
        let mut stream = self.stream()?;
        stream.set_read_timeout(Some(self.connect_timeout))?;
        let path = format!("{}/healthz", self.path.trim_end_matches('/'));
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.host
        );
        stream.write_all(request.as_bytes())?;
        let head = ws::read_head(&mut stream)?;
        match head.start.split_whitespace().nth(1) {
            Some("200") => Ok(()),
            _ => Err(io::Error::other(format!(
                "the health check at {} answered {:?}",
                self.url(),
                head.start
            ))),
        }
    }

    /// The execs running in the sandbox and whether the bridge saw its
    /// attempt state tampered with.
    pub fn status(&self) -> io::Result<BridgeStatus> {
        let mut connection = self.connect()?;
        connection.send(&Frame::Status)?;
        let status = match connection.recv_ok()? {
            Frame::Report { execs, tampered } => BridgeStatus { execs, tampered },
            other => return Err(unexpected(&other)),
        };
        connection.close();
        Ok(status)
    }

    /// Start `spec` in the sandbox.
    pub fn exec(&self, spec: &ExecSpec) -> Result<BridgeProcess, ProviderError> {
        if spec.argv.is_empty() {
            return Err(ProviderError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty argument vector",
            )));
        }
        let mut connection = self.connect()?;
        connection.send(&Frame::Exec {
            argv: spec.argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            cwd: spec.cwd.as_os_str().as_bytes().to_vec(),
            env: spec
                .env
                .iter()
                .map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect(),
        })?;
        match connection.recv()? {
            Frame::Started { pid } => Ok(BridgeProcess::start(pid, connection)),
            Frame::Failed { kind, message } => {
                Err(ProviderError::Io(io::Error::new(kind.into(), message)))
            }
            other => Err(unexpected(&other).into()),
        }
    }

    /// Write `content` to `path` in the sandbox with permission bits `mode`.
    pub fn put_file(&self, path: &Path, mode: u32, content: &mut dyn Read) -> io::Result<()> {
        let mut connection = self.connect()?;
        connection.send(&Frame::PutFile {
            path: path.as_os_str().as_bytes().to_vec(),
            mode,
        })?;
        tree::send_content(content, &mut |frame| connection.send(&frame))?;
        connection.expect_done()
    }

    /// Copy `path` from the sandbox into `out`.
    pub fn get_file(&self, path: &Path, out: &mut dyn Write) -> io::Result<()> {
        let mut connection = self.connect()?;
        connection.send(&Frame::GetFile {
            path: path.as_os_str().as_bytes().to_vec(),
        })?;
        tree::receive_content(out, &mut || connection.recv_ok())?;
        connection.close();
        Ok(())
    }

    /// Copy the host directory `from` to `to` in the sandbox.
    pub fn put_tree(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut connection = self.connect()?;
        connection.send(&Frame::PutTree {
            path: to.as_os_str().as_bytes().to_vec(),
        })?;
        tree::send(from, &mut |frame| connection.send(&frame))?;
        connection.expect_done()
    }

    /// Copy the sandbox directory `from` into the host directory `to`,
    /// which must not exist.
    pub fn get_tree(&self, from: &Path, to: &Path) -> io::Result<()> {
        if to.symlink_metadata().is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", to.display()),
            ));
        }
        let mut connection = self.connect()?;
        connection.send(&Frame::GetTree {
            path: from.as_os_str().as_bytes().to_vec(),
        })?;
        tree::receive(to, &mut || connection.recv_ok())?;
        connection.close();
        Ok(())
    }

    /// End this credential's attempt; returns the processes it killed.
    pub fn end_attempt(&self) -> io::Result<Vec<String>> {
        self.survivors(Frame::EndAttempt)
    }

    /// Tear down every process the bridge started; returns their names.
    pub fn shutdown(&self) -> io::Result<Vec<String>> {
        self.survivors(Frame::Shutdown)
    }

    fn survivors(&self, request: Frame) -> io::Result<Vec<String>> {
        let mut connection = self.connect()?;
        connection.send(&request)?;
        let names = match connection.recv_ok()? {
            Frame::Survivors(names) => names,
            other => return Err(unexpected(&other)),
        };
        connection.close();
        Ok(names)
    }
}

fn unexpected(frame: &Frame) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("unexpected bridge frame {frame:?}"),
    )
}

struct Connection {
    reader: WsReader,
    writer: Arc<Mutex<WsWriter>>,
}

impl Connection {
    fn send(&mut self, frame: &Frame) -> io::Result<()> {
        self.writer.lock_recovering("writer").send(&frame.encode())
    }

    fn recv(&mut self) -> io::Result<Frame> {
        match self.reader.recv()? {
            Some(message) => Frame::decode(&message),
            None => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the bridge closed the connection",
            )),
        }
    }

    /// The next frame, with [`Frame::Failed`] turned into an error.
    fn recv_ok(&mut self) -> io::Result<Frame> {
        match self.recv()? {
            Frame::Failed { kind, message } => Err(io::Error::new(kind.into(), message)),
            frame => Ok(frame),
        }
    }

    fn expect_done(mut self) -> io::Result<()> {
        match self.recv_ok()? {
            Frame::Done => {
                self.close();
                Ok(())
            }
            other => Err(unexpected(&other)),
        }
    }

    fn close(mut self) {
        let _ = self.writer.lock_recovering("writer").close();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.reader.recv() {
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        self.writer.lock_recovering("writer").abort();
    }
}

#[derive(Default)]
struct State {
    status: Option<ExitStatus>,
    /// The connection ended; no more frames will arrive.
    gone: bool,
    survivors: Option<Vec<String>>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock_recovering("state"));
        self.changed.notify_all();
    }

    fn wait_until<T>(
        &self,
        timeout: Option<Duration>,
        mut ready: impl FnMut(&mut State) -> Option<T>,
    ) -> Option<T> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut state = self.state.lock_recovering("state");
        loop {
            if let Some(value) = ready(&mut state) {
                return Some(value);
            }
            match deadline {
                None => state = self.changed.wait_recovering(state, "changed"),
                Some(deadline) => {
                    let left = deadline.checked_duration_since(Instant::now())?;
                    state = self
                        .changed
                        .wait_timeout_recovering(state, left, "changed")
                        .0;
                }
            }
        }
    }
}

/// A process running behind a bridge. Its pipes are backed by the
/// connection's frames; a reader thread demultiplexes them.
///
/// If the connection ends before the bridge reports an exit, [`wait`]
/// returns an unknown exit status (no code, no signal): the bridge tears
/// the process down when its connection ends, but the host cannot observe
/// how it ended.
///
/// [`wait`]: Process::wait
pub struct BridgeProcess {
    pid: u32,
    writer: Arc<Mutex<WsWriter>>,
    shared: Arc<Shared>,
    stdin: Option<Box<dyn Write + Send>>,
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
    torn_down: bool,
}

impl std::fmt::Debug for BridgeProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeProcess")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

struct Stdin {
    writer: Arc<Mutex<WsWriter>>,
}

impl Stdin {
    fn send(&self, frame: Frame) -> io::Result<()> {
        self.writer.lock_recovering("writer").send(&frame.encode())
    }
}

impl Write for Stdin {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(CHUNK);
        self.send(Frame::Stdin(buf[..n].to_vec()))?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Stdin {
    fn drop(&mut self) {
        let _ = self.send(Frame::CloseStdin);
    }
}

impl BridgeProcess {
    fn start(pid: u32, connection: Connection) -> BridgeProcess {
        let Connection { mut reader, writer } = connection;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let (stdout_read, stdout_write) = io::pipe().expect("create a pipe");
        let (stderr_read, stderr_write) = io::pipe().expect("create a pipe");
        {
            let shared = shared.clone();
            thread::spawn(move || {
                let mut stdout = Some(stdout_write);
                let mut stderr = Some(stderr_write);
                let deliver = |pipe: &mut Option<io::PipeWriter>, data: &[u8]| {
                    // A reader that went away discards the rest.
                    if pipe.as_mut().is_some_and(|p| p.write_all(data).is_err()) {
                        *pipe = None;
                    }
                };
                while let Ok(Some(message)) = reader.recv() {
                    let Ok(frame) = Frame::decode(&message) else {
                        break;
                    };
                    match frame {
                        Frame::Stdout(data) => deliver(&mut stdout, &data),
                        Frame::Stderr(data) => deliver(&mut stderr, &data),
                        Frame::StdoutClosed => stdout = None,
                        Frame::StderrClosed => stderr = None,
                        Frame::Exited { code, signal } => {
                            shared.update(|s| s.status = Some(ExitStatus { code, signal }))
                        }
                        Frame::Survivors(names) => shared.update(|s| s.survivors = Some(names)),
                        _ => {}
                    }
                }
                drop((stdout, stderr));
                shared.update(|s| s.gone = true);
            });
        }
        BridgeProcess {
            pid,
            stdin: Some(Box::new(Stdin {
                writer: writer.clone(),
            })),
            stdout: Some(Box::new(stdout_read)),
            stderr: Some(Box::new(stderr_read)),
            writer,
            shared,
            torn_down: false,
        }
    }

    fn send(&self, frame: Frame) -> io::Result<()> {
        self.writer.lock_recovering("writer").send(&frame.encode())
    }

    fn gone(&self) -> bool {
        self.shared.state.lock_recovering("state").gone
    }
}

impl Process for BridgeProcess {
    /// The process ID inside the sandbox.
    fn id(&self) -> String {
        self.pid.to_string()
    }

    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        self.stdin.take()
    }

    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stderr.take()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let state = self.shared.state.lock_recovering("state");
        Ok(match (state.status, state.gone) {
            (Some(status), _) => Some(status),
            (None, true) => Some(ExitStatus::default()),
            (None, false) => None,
        })
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        Ok(self
            .shared
            .wait_until(None, |s| match (s.status, s.gone) {
                (Some(status), _) => Some(status),
                (None, true) => Some(ExitStatus::default()),
                (None, false) => None,
            })
            .unwrap_or_default())
    }

    /// SIGKILL to the launched process only; [`Process::teardown`] reaches
    /// the rest of its group.
    fn kill(&mut self) -> io::Result<()> {
        match self.send(Frame::Kill) {
            Err(_) if self.gone() => Ok(()),
            other => other,
        }
    }

    fn teardown(&mut self) -> Vec<String> {
        if std::mem::replace(&mut self.torn_down, true) {
            return Vec::new();
        }
        if self.send(Frame::Teardown).is_err() {
            return Vec::new();
        }
        self.shared
            .wait_until(Some(TEARDOWN_WAIT), |s| match (&mut s.survivors, s.gone) {
                (Some(names), _) => Some(std::mem::take(names)),
                (None, true) => Some(Vec::new()),
                (None, false) => None,
            })
            .unwrap_or_default()
    }
}

impl Drop for BridgeProcess {
    /// Closing the connection makes the bridge tear the exec down.
    fn drop(&mut self) {
        drop(self.stdin.take());
        let mut writer = self.writer.lock_recovering("writer");
        if !self.torn_down {
            let _ = writer.send(&Frame::Teardown.encode());
        }
        writer.abort();
    }
}
