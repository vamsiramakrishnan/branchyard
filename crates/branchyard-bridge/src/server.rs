//! The bridge server: runs inside the sandbox and executes requests from
//! authenticated connections.
//!
//! What it guarantees:
//!
//! - Every connection is refused before the upgrade unless it carries a
//!   credential signed by the host's key, for this actor's current identity,
//!   unexpired, and for the newest attempt the bridge has seen that has not
//!   ended ([`crate::credential::Attempts`]). The attempt state is written to
//!   the state file before any connection that changed it is answered, so an
//!   ended attempt stays ended across a bridge restart.
//! - Each exec runs without a shell in its own process group, with the
//!   bridge's environment (minus its own `BRANCHYARD_BRIDGE*` variables) and
//!   the request's variables. `Teardown`, a closed connection, an ended or
//!   superseded attempt and `Shutdown` kill the group; teardown names the
//!   live members first.
//! - The exit status is sent when the launched process is reaped,
//!   independently of its output pipes.
//!
//! What it does not guarantee:
//!
//! - Isolation between execs: they run as the bridge's user, and the
//!   sandbox is the boundary.
//! - Reaching a descendant that leaves its process group, or reaping
//!   orphans when the bridge is process 1: run it under an init that reaps.
//! - Confidentiality on the wire. Credentials and data travel in plain
//!   HTTP; the router and the network to it are trusted.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::credential::{Attempts, Claims, Identity, Refusal, Verifier};
use crate::protocol::{Frame, CHUNK, SUBPROTOCOL};
use crate::tree;
use crate::ws::{self, WsReader, WsWriter};

/// How long a client may take to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Execs that may run at once.
const MAX_EXECS: usize = 64;
/// Files in the identity directory, as the actor template projects them.
pub const IDENTITY_FILES: [&str; 3] = ["atespace", "name", "uid"];

/// How the bridge is configured.
#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub verifier: Verifier,
    /// Holds `atespace`, `name` and `uid`, re-read on every connection
    /// because a restored or branched actor's identity changes under a
    /// running bridge.
    pub identity_dir: PathBuf,
    /// Where the attempt state is kept. Keep it on the sandbox's own
    /// filesystem so it is snapshotted with it.
    pub state_file: PathBuf,
    /// Tear everything down and exit when stdin reaches end of file. For a
    /// supervisor that holds the bridge's stdin, such as a test fake.
    pub lifeline: bool,
}

/// A process group the bridge started.
struct Group {
    pgid: u32,
    /// The attempt that started it.
    seq: u64,
    /// Set once the group was signalled for teardown; never signalled again.
    done: AtomicBool,
    /// Set, under the lock, when the leader is reaped. A kill checks it
    /// under the same lock, so it never signals a reused PID.
    reaped: Mutex<bool>,
}

impl Group {
    fn kill_leader(&self) {
        let reaped = self.reaped.lock().unwrap_or_else(|e| e.into_inner());
        if !*reaped {
            // SAFETY: plain syscall on a PID that has not been reaped.
            unsafe { libc::kill(self.pgid as i32, libc::SIGKILL) };
        }
    }

    fn teardown(&self) -> Vec<String> {
        if self.done.swap(true, Ordering::AcqRel) {
            return Vec::new();
        }
        let survivors = group_members(self.pgid);
        // SAFETY: plain syscall; a negative PID names the process group.
        unsafe { libc::kill(-(self.pgid as i32), libc::SIGKILL) };
        survivors
    }
}

struct Shared {
    verifier: Verifier,
    identity_dir: PathBuf,
    state_file: PathBuf,
    attempts: Mutex<Attempts>,
    groups: Mutex<Vec<Weak<Group>>>,
    base_env: Vec<(OsString, OsString)>,
}

impl Shared {
    fn identity(&self) -> io::Result<Identity> {
        let read = |file: &str| -> io::Result<String> {
            let path = self.identity_dir.join(file);
            let text = fs::read_to_string(&path).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("could not read identity {}: {e}", path.display()),
                )
            })?;
            Ok(text.trim().to_owned())
        };
        Ok(Identity {
            atespace: read("atespace")?,
            actor: read("name")?,
            uid: read("uid")?,
        })
    }

    fn save(&self, attempts: &Attempts) -> io::Result<()> {
        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let partial = self.state_file.with_extension("partial");
        let mut file = fs::File::create(&partial)?;
        file.write_all(attempts.encode().as_bytes())?;
        file.sync_all()?;
        fs::rename(&partial, &self.state_file)
    }

    /// Tear down the groups `select` picks; returns the survivors' names.
    fn teardown(&self, select: impl Fn(&Group) -> bool) -> Vec<String> {
        let groups: Vec<Arc<Group>> = {
            let mut groups = self.groups.lock().unwrap_or_else(|e| e.into_inner());
            groups.retain(|g| g.strong_count() > 0);
            groups.iter().filter_map(Weak::upgrade).collect()
        };
        groups
            .iter()
            .filter(|g| select(g))
            .flat_map(|g| g.teardown())
            .collect()
    }

    /// Admit a credential; persist and enforce any change.
    fn admit(&self, token: Option<&str>) -> Result<Claims, (u16, String)> {
        let unauthorized = |refusal: Refusal| (401, refusal.to_string());
        let token = token.ok_or(unauthorized(Refusal::Missing))?;
        let claims = self.verifier.verify(token, now()).map_err(unauthorized)?;
        let identity = self
            .identity()
            .map_err(|e| (503, format!("the bridge cannot read its identity: {e}")))?;
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = attempts.clone();
        let dead = next.admit(&claims, &identity).map_err(unauthorized)?;
        if next != *attempts {
            self.save(&next)
                .map_err(|e| (503, format!("the bridge cannot record the attempt: {e}")))?;
            *attempts = next;
        }
        drop(attempts);
        if let Some(dead) = dead {
            self.teardown(|g| g.seq <= dead);
        }
        Ok(claims)
    }

    fn end(&self, seq: u64) -> io::Result<Vec<String>> {
        {
            let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
            let mut next = attempts.clone();
            next.end(seq);
            if next != *attempts {
                self.save(&next)?;
                *attempts = next;
            }
        }
        Ok(self.teardown(|g| g.seq <= seq))
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A bound bridge, not yet serving.
pub struct Bridge {
    listener: TcpListener,
    shared: Arc<Shared>,
    lifeline: bool,
}

impl Bridge {
    /// Bind the listener and load the attempt state. A state file that
    /// cannot be read is an error, never a fresh start: that would revive
    /// ended attempts.
    pub fn bind(config: Config) -> io::Result<Bridge> {
        let attempts = match fs::read_to_string(&config.state_file) {
            Ok(text) => Attempts::decode(&text).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unreadable attempt state {}", config.state_file.display()),
                )
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Attempts::default(),
            Err(error) => return Err(error),
        };
        let listener = TcpListener::bind(config.listen)?;
        let base_env = std::env::vars_os()
            .filter(|(name, _)| !name.as_bytes().starts_with(b"BRANCHYARD_BRIDGE"))
            .collect();
        Ok(Bridge {
            listener,
            shared: Arc::new(Shared {
                verifier: config.verifier,
                identity_dir: config.identity_dir,
                state_file: config.state_file,
                attempts: Mutex::new(attempts),
                groups: Mutex::new(Vec::new()),
                base_env,
            }),
            lifeline: config.lifeline,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve until the process ends.
    pub fn serve(self) -> io::Result<()> {
        if self.lifeline {
            let shared = self.shared.clone();
            thread::spawn(move || {
                let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
                shared.teardown(|_| true);
                std::process::exit(0);
            });
        }
        for stream in self.listener.incoming() {
            let Ok(stream) = stream else { continue };
            let shared = self.shared.clone();
            thread::spawn(move || {
                let _ = connection(&shared, stream);
            });
        }
        Ok(())
    }
}

fn connection(shared: &Arc<Shared>, mut stream: TcpStream) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(HEAD_TIMEOUT))?;
    let head = ws::read_head(&mut stream)?;
    let mut start = head.start.split_whitespace();
    let (method, path) = (start.next().unwrap_or(""), start.next().unwrap_or(""));
    if !head.lists("upgrade", "websocket") {
        let path = path.split('?').next().unwrap_or(path);
        return match (method, path.ends_with("/healthz")) {
            ("GET", true) => ws::respond(&mut stream, "200 OK", "ok\n"),
            _ => ws::respond(&mut stream, "404 Not Found", "not a bridge endpoint\n"),
        };
    }
    if head.header("sec-websocket-version") != Some("13")
        || !head.lists("sec-websocket-protocol", SUBPROTOCOL)
    {
        return ws::respond(
            &mut stream,
            "426 Upgrade Required",
            &format!("this bridge speaks WebSocket 13 with subprotocol {SUBPROTOCOL}\n"),
        );
    }
    let token = head
        .header("authorization")
        .and_then(|value| value.strip_prefix("Bearer "));
    let claims = match shared.admit(token) {
        Ok(claims) => claims,
        Err((status, reason)) => {
            let status = match status {
                401 => "401 Unauthorized",
                _ => "503 Service Unavailable",
            };
            return ws::respond(&mut stream, status, &format!("{reason}\n"));
        }
    };
    stream.set_read_timeout(None)?;
    let (mut reader, writer) = ws::server_accept(stream, &head, SUBPROTOCOL)?;
    let send = |frame: Frame| -> io::Result<()> {
        writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(&frame.encode())
    };
    let Some(first) = reader.recv()? else {
        return Ok(());
    };
    let request = Frame::decode(&first)?;
    let outcome = match request {
        Frame::Exec { argv, cwd, env } => {
            return exec(shared, claims.seq, reader, writer, argv, cwd, env)
        }
        Frame::PutFile { path, mode } => put_file(&path, mode, &mut reader).map(|()| Frame::Done),
        Frame::GetFile { path } => get_file(&path, &send),
        Frame::PutTree { path } => absolute(&path).and_then(|root| {
            tree::receive(&root, &mut || next_frame(&mut reader)).map(|()| Frame::Done)
        }),
        Frame::GetTree { path } => absolute(&path)
            .and_then(|root| tree::send(&root, &mut |frame| send(frame)))
            .map(|()| Frame::End),
        Frame::EndAttempt => shared.end(claims.seq).map(Frame::Survivors),
        Frame::Shutdown => Ok(Frame::Survivors(shared.teardown(|_| true))),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{other:?} is not a request"),
        )),
    };
    match outcome {
        // A sent tree or file already ended with its own `End`.
        Ok(Frame::End) => {}
        Ok(reply) => send(reply)?,
        Err(error) => send(Frame::failed(&error))?,
    }
    finish(reader, &writer);
    Ok(())
}

/// Close the WebSocket and wait briefly for the client to close too.
fn finish(mut reader: WsReader, writer: &Arc<Mutex<WsWriter>>) {
    let _ = writer.lock().unwrap_or_else(|e| e.into_inner()).close();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match reader.recv() {
            Ok(Some(_)) => continue,
            _ => break,
        }
    }
    writer.lock().unwrap_or_else(|e| e.into_inner()).abort();
}

fn next_frame(reader: &mut WsReader) -> io::Result<Frame> {
    match reader.recv()? {
        Some(message) => Frame::decode(&message),
        None => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the client closed the connection mid-request",
        )),
    }
}

fn absolute(path: &[u8]) -> io::Result<PathBuf> {
    let path = Path::new(OsStr::from_bytes(path));
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not an absolute path", path.display()),
        ));
    }
    Ok(path.to_path_buf())
}

fn put_file(path: &[u8], mode: u32, reader: &mut WsReader) -> io::Result<()> {
    let path = absolute(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut partial = path.clone().into_os_string();
    partial.push(".branchyard-partial");
    let partial = PathBuf::from(partial);
    let mut file = fs::File::create(&partial)?;
    tree::receive_content(&mut file, &mut || next_frame(reader))?;
    file.set_permissions(fs::Permissions::from_mode(mode & 0o7777))?;
    file.sync_all()?;
    fs::rename(&partial, &path)
}

fn get_file(path: &[u8], send: &dyn Fn(Frame) -> io::Result<()>) -> io::Result<Frame> {
    let path = absolute(path)?;
    let mut file = fs::File::open(&path)?;
    tree::send_content(&mut file, &mut |frame| send(frame))?;
    Ok(Frame::End)
}

/// Wait for `pid` to exit without reaping it.
fn wait_exited(pid: u32) {
    loop {
        // SAFETY: waitid with a zeroed siginfo out-parameter.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

fn pump(
    mut pipe: impl Read,
    send: impl Fn(Frame) -> io::Result<()>,
    wrap: fn(Vec<u8>) -> Frame,
    closed: Frame,
) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if send(wrap(buf[..n].to_vec())).is_err() {
                    // The client is gone; keep draining so the process is
                    // never blocked on a full pipe before teardown.
                    let _ = io::copy(&mut pipe, &mut io::sink());
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = send(closed);
}

#[allow(clippy::too_many_arguments)]
fn exec(
    shared: &Arc<Shared>,
    seq: u64,
    mut reader: WsReader,
    writer: Arc<Mutex<WsWriter>>,
    argv: Vec<Vec<u8>>,
    cwd: Vec<u8>,
    env: Vec<(Vec<u8>, Vec<u8>)>,
) -> io::Result<()> {
    let send = {
        let writer = writer.clone();
        move |frame: Frame| -> io::Result<()> {
            writer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .send(&frame.encode())
        }
    };
    let running = {
        let mut groups = shared.groups.lock().unwrap_or_else(|e| e.into_inner());
        groups.retain(|g| g.strong_count() > 0);
        groups.len()
    };
    let spawned = match argv.split_first() {
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty argument vector",
        )),
        Some(_) if running >= MAX_EXECS => Err(io::Error::other(format!(
            "the bridge already runs {MAX_EXECS} execs"
        ))),
        Some((program, args)) => Command::new(OsStr::from_bytes(program))
            .args(args.iter().map(|a| OsStr::from_bytes(a)))
            .current_dir(OsStr::from_bytes(&cwd))
            .env_clear()
            .envs(shared.base_env.iter().cloned())
            .envs(
                env.iter()
                    .map(|(n, v)| (OsStr::from_bytes(n), OsStr::from_bytes(v))),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn(),
    };
    let mut child: Child = match spawned {
        Ok(child) => child,
        Err(error) => {
            send(Frame::failed(&error))?;
            finish(reader, &writer);
            return Ok(());
        }
    };
    let pid = child.id();
    let group = Arc::new(Group {
        pgid: pid,
        seq,
        done: AtomicBool::new(false),
        reaped: Mutex::new(false),
    });
    shared
        .groups
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Arc::downgrade(&group));
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let stdin = child.stdin.take().expect("piped");
    send(Frame::Started { pid })?;

    {
        let send = send.clone();
        thread::spawn(move || pump(stdout, send, Frame::Stdout, Frame::StdoutClosed));
    }
    {
        let send = send.clone();
        thread::spawn(move || pump(stderr, send, Frame::Stderr, Frame::StderrClosed));
    }
    // Stdin is written on its own thread, so a process that stops reading
    // cannot keep this loop from seeing a kill or teardown.
    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut stdin = stdin;
        for data in stdin_rx {
            if stdin.write_all(&data).and_then(|()| stdin.flush()).is_err() {
                break;
            }
        }
    });
    let waiter = {
        let group = group.clone();
        let send = send.clone();
        thread::spawn(move || {
            wait_exited(pid);
            let status = {
                let mut reaped = group.reaped.lock().unwrap_or_else(|e| e.into_inner());
                let status = child.wait();
                *reaped = true;
                status
            };
            let frame = match status {
                Ok(status) => Frame::Exited {
                    code: status.code(),
                    signal: status.signal(),
                },
                Err(error) => Frame::failed(&error),
            };
            let _ = send(frame);
        })
    };

    let mut stdin = Some(stdin_tx);
    while let Ok(Some(message)) = reader.recv() {
        let Ok(frame) = Frame::decode(&message) else {
            break;
        };
        match frame {
            Frame::Stdin(data) => {
                if let Some(tx) = &stdin {
                    if tx.send(data).is_err() {
                        stdin = None;
                    }
                }
            }
            Frame::CloseStdin => stdin = None,
            Frame::Kill => group.kill_leader(),
            Frame::Teardown => {
                let survivors = group.teardown();
                if send(Frame::Survivors(survivors)).is_err() {
                    break;
                }
            }
            _ => break,
        }
    }
    // The client closed the connection or broke the protocol: end the exec.
    drop(stdin);
    group.teardown();
    group.kill_leader();
    let _ = waiter.join();
    writer.lock().unwrap_or_else(|e| e.into_inner()).abort();
    Ok(())
}

/// Command names of the live (non-zombie) members of process group `pgid`,
/// from `/proc`.
fn group_members(pgid: u32) -> Vec<String> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut members = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let comm = &stat[open + 1..close];
        let mut fields = stat[close + 1..].split_whitespace();
        let (Some(state), Some(_ppid), Some(pgrp)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if pgrp == pgid.to_string() && state != "Z" && state != "X" {
            members.push(comm.to_owned());
        }
    }
    members
}
