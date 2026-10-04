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
//! - It is a child subreaper (Linux): a process orphaned by an exec is
//!   reparented to the bridge, which reaps it, so no zombie accumulates
//!   whether or not the bridge is process 1. SIGTERM or SIGINT is
//!   forwarded to every exec's process group; after a grace period the
//!   groups are killed, and once every exec's last output and exit status
//!   have been sent to its client (or 2 more seconds have passed) the
//!   bridge exits with status 0. As process 1, it ignores those signals
//!   when a process inside the sandbox sends them, so only the container
//!   runtime can stop it.
//! - With `run_as`, execs run as that user and group with no supplementary
//!   groups, and files and trees are read and written with that user's
//!   file-system identity, so they belong to it and a link it planted is
//!   followed only as far as it could follow it itself. The bridge's
//!   memory is not readable by same-user processes (it is not dumpable),
//!   and its state file is private to its own user.
//! - A state file changed behind the bridge's back is noticed on the next
//!   request that reads the state, rewritten from memory, and reported by
//!   [`crate::protocol::Frame::Report`].
//!
//! What it does not guarantee:
//!
//! - Isolation between execs: they all run as one user, and the sandbox is
//!   the boundary.
//! - Reaching a descendant that leaves its process group; it is still
//!   reaped when it exits.
//! - That attempt state survives a process in the sandbox that can write
//!   the state file while the bridge is not running: see
//!   `docs/substrate.md`, "Attempt state".
//! - Confidentiality on the wire unless it is served over TLS, by the
//!   bridge ([`Config::tls`]) or by a router in front of it.

use branchyard_support::LockExt as _;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::credential::{Attempts, Claims, Identity, Refusal, Verifier};
use crate::protocol::{ExecReport, Frame, CHUNK, SUBPROTOCOL};
use crate::stream::Stream;
use crate::tls::{self, ServerTls};
use crate::tree;
use crate::ws::{self, WsReader, WsWriter};

/// How long a client may take to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Execs that may run at once.
const MAX_EXECS: usize = 64;
/// Files in the identity directory, as the actor template projects them.
pub const IDENTITY_FILES: [&str; 3] = ["atespace", "name", "uid"];
/// How long execs get to exit after SIGTERM is forwarded to them.
pub const TERM_GRACE: Duration = Duration::from_secs(10);
/// How long, once the execs have ended at shutdown, the bridge waits for
/// their last output and exit statuses to reach their clients.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

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
    /// Serve TLS rather than plain HTTP.
    pub tls: Option<ServerTls>,
    /// Run execs, and read and write files and trees, as this user and
    /// group rather than the bridge's own. Needs the bridge to be root (or
    /// hold `CAP_SETUID`, `CAP_SETGID`), and Linux for files and trees.
    pub run_as: Option<(u32, u32)>,
}

/// A process group the bridge started.
struct Group {
    pgid: u32,
    /// The attempt that started it.
    seq: u64,
    program: Vec<u8>,
    started: Instant,
    /// Set once the group was signalled for teardown; never signalled again.
    done: AtomicBool,
    /// Set, under the lock, when the leader is reaped. A kill checks it
    /// under the same lock, so it never signals a reused PID.
    reaped: Mutex<bool>,
    /// What the exec has yet to send its client: its stdout to the end,
    /// its stderr to the end and its exit status, one count each. A
    /// shutting-down bridge waits for zero before it exits.
    undelivered: Arc<AtomicUsize>,
}

impl Group {
    fn kill_leader(&self) {
        let reaped = self.reaped.lock_recovering("reaped");
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

/// The attempt state: memory is the authority for the bridge's lifetime;
/// the file carries it across a restart.
struct State {
    attempts: Attempts,
    /// What the state file held when last read or written; `None` when
    /// there was none.
    written: Option<String>,
    /// The file was found changed behind the bridge's back.
    tampered: bool,
}

struct Shared {
    verifier: Verifier,
    identity_dir: PathBuf,
    state_file: PathBuf,
    state: Mutex<State>,
    groups: Mutex<Vec<Weak<Group>>>,
    /// PIDs of launched processes not yet reaped by their exec. Held while
    /// spawning and while reaping orphans, so an orphan reaper never reaps
    /// a process an exec is about to wait for.
    children: Mutex<HashSet<u32>>,
    base_env: Vec<(OsString, OsString)>,
    run_as: Option<(u32, u32)>,
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

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock_recovering("state")
    }

    /// Write `attempts` to the state file, readable and writable by the
    /// bridge's user only, in a directory only it can enter when the
    /// bridge creates it.
    fn save(&self, state: &mut State, attempts: &Attempts) -> io::Result<()> {
        if let Some(parent) = self.state_file.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let partial = self.state_file.with_extension("partial");
        branchyard_support::cleanup_file(&partial);
        let text = attempts.encode();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&partial, &self.state_file)?;
        state.written = Some(text);
        Ok(())
    }

    /// Compare the state file with what the bridge last wrote; if it
    /// differs, record the tampering and rewrite it from memory.
    fn verify_file(&self, state: &mut State) -> io::Result<()> {
        let found = match fs::read_to_string(&self.state_file) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if found == state.written {
            return Ok(());
        }
        state.tampered = true;
        eprintln!(
            "branchyard-bridge: the attempt state file {} was changed behind the bridge; \
             restoring it from memory",
            self.state_file.display()
        );
        let attempts = state.attempts.clone();
        self.save(state, &attempts)
    }

    /// Tear down the groups `select` picks; returns the survivors' names.
    fn teardown(&self, select: impl Fn(&Group) -> bool) -> Vec<String> {
        let groups: Vec<Arc<Group>> = {
            let mut groups = self.groups.lock_recovering("groups");
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
        let mut state = self.lock_state();
        self.verify_file(&mut state).map_err(|e| {
            (
                503,
                format!("the bridge cannot check its attempt state: {e}"),
            )
        })?;
        let mut next = state.attempts.clone();
        let dead = next.admit(&claims, &identity).map_err(unauthorized)?;
        if next != state.attempts {
            self.save(&mut state, &next)
                .map_err(|e| (503, format!("the bridge cannot record the attempt: {e}")))?;
            state.attempts = next;
        }
        drop(state);
        if let Some(dead) = dead {
            self.teardown(|g| g.seq <= dead);
        }
        Ok(claims)
    }

    fn end(&self, seq: u64) -> io::Result<Vec<String>> {
        {
            let mut state = self.lock_state();
            self.verify_file(&mut state)?;
            let mut next = state.attempts.clone();
            next.end(seq);
            if next != state.attempts {
                self.save(&mut state, &next)?;
                state.attempts = next;
            }
        }
        Ok(self.teardown(|g| g.seq <= seq))
    }

    /// The groups still alive: an exec whose connection is open, or whose
    /// group still has live members.
    fn live_groups(&self) -> Vec<Arc<Group>> {
        let mut groups = self.groups.lock_recovering("groups");
        groups.retain(|g| g.strong_count() > 0);
        groups.iter().filter_map(Weak::upgrade).collect()
    }

    /// What [`Frame::Status`] answers.
    fn report(&self) -> io::Result<Frame> {
        let tampered = {
            let mut state = self.lock_state();
            self.verify_file(&mut state)?;
            state.tampered
        };
        let execs = self
            .live_groups()
            .into_iter()
            .filter_map(|group| {
                let running = !*group.reaped.lock_recovering("reaped");
                let members = group_members(group.pgid);
                (running || !members.is_empty()).then(|| ExecReport {
                    pid: group.pgid,
                    attempt: group.seq,
                    program: group.program.clone(),
                    seconds: group.started.elapsed().as_secs().min(u32::MAX.into()) as u32,
                    members,
                })
            })
            .collect();
        Ok(Frame::Report { execs, tampered })
    }

    /// Reap every zombie child that no exec is waiting for: orphans
    /// reparented to the bridge as process 1 or as a subreaper.
    fn reap_orphans(&self) {
        let children = self.children.lock_recovering("children");
        for pid in zombie_children() {
            if !children.contains(&pid) {
                let mut status = 0;
                // SAFETY: plain syscall on a zombie child that nothing else
                // waits for.
                unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
            }
        }
    }

    /// Forward SIGTERM to every group, wait up to `grace` for them to end,
    /// then kill what is left.
    fn terminate(&self, grace: Duration) {
        let groups = self.live_groups();
        for group in &groups {
            if !group.done.load(Ordering::Acquire) {
                // SAFETY: plain syscall; a negative PID names the group.
                unsafe { libc::kill(-(group.pgid as i32), libc::SIGTERM) };
            }
        }
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && groups.iter().any(|g| !group_members(g.pgid).is_empty())
        {
            self.reap_orphans();
            thread::sleep(Duration::from_millis(50));
        }
        self.teardown(|_| true);
        self.reap_orphans();
        // The groups have ended, but their pipes may still hold output the
        // pumps have not sent, and the waiters may not have sent the exit
        // status: exiting now would cut both off from the clients.
        let deadline = Instant::now() + DRAIN_GRACE;
        while Instant::now() < deadline
            && groups
                .iter()
                .any(|g| g.undelivered.load(Ordering::Acquire) > 0)
        {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Become a child subreaper, so orphans of execs are reparented to the
/// bridge rather than to process 1, and make the bridge's memory
/// unreadable to other processes of its user. Linux only; elsewhere a
/// no-op.
fn harden() {
    #[cfg(target_os = "linux")]
    // SAFETY: plain prctl calls with integer arguments.
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
}

fn termination_signals() -> libc::sigset_t {
    // SAFETY: sigemptyset and sigaddset on a local set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in [libc::SIGCHLD, libc::SIGTERM, libc::SIGINT] {
            libc::sigaddset(&mut set, signal);
        }
        set
    }
}

/// Block SIGCHLD, SIGTERM and SIGINT in the calling thread, and so in every
/// thread it starts afterwards, so that [`Bridge::serve`] receives them
/// synchronously. Call it first thing in `main`, before any thread starts.
/// Processes the bridge starts get an empty signal mask all the same.
pub fn block_signals() {
    let set = termination_signals();
    // SAFETY: plain call with a valid set.
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
}

/// Receive signals and reap orphans until told to terminate.
fn supervise(shared: Arc<Shared>) {
    let set = termination_signals();
    let pid1 = std::process::id() == 1;
    let tick = libc::timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    loop {
        // SAFETY: sigtimedwait with a valid set and out-parameter.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let signal = unsafe { libc::sigtimedwait(&set, &mut info, &tick) };
        shared.reap_orphans();
        if signal == libc::SIGTERM || signal == libc::SIGINT {
            // SAFETY: si_pid is valid for signals sent with kill.
            let sender = unsafe { info.si_pid() };
            if pid1 && sender != 0 {
                eprintln!(
                    "branchyard-bridge: ignoring signal {signal} from process {sender} in the sandbox"
                );
                continue;
            }
            shared.terminate(TERM_GRACE);
            std::process::exit(0);
        }
    }
}

/// PIDs of this process's children that are zombies, from `/proc`.
fn zombie_children() -> Vec<u32> {
    let me = std::process::id().to_string();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut zombies = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let mut fields = stat[close + 1..].split_whitespace();
        if let (Some("Z"), Some(ppid)) = (fields.next(), fields.next()) {
            if ppid == me {
                zombies.push(pid);
            }
        }
    }
    zombies
}

/// The file-system identity of the calling thread, switched to another
/// user until dropped (Linux: `setfsuid`, which is per thread).
struct AsUser {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    previous: Option<(u32, u32)>,
}

impl AsUser {
    fn enter(user: Option<(u32, u32)>) -> io::Result<AsUser> {
        let Some((uid, gid)) = user else {
            return Ok(AsUser { previous: None });
        };
        #[cfg(target_os = "linux")]
        {
            // SAFETY: plain syscalls; they return the previous value.
            let old_gid = unsafe { libc::setfsgid(gid) } as u32;
            let old_uid = unsafe { libc::setfsuid(uid) } as u32;
            // Each returns the previous identity whether or not it changed;
            // a second call reports whether the first took effect.
            let now_uid = unsafe { libc::setfsuid(uid) } as u32;
            let now_gid = unsafe { libc::setfsgid(gid) } as u32;
            let guard = AsUser {
                previous: Some((old_uid, old_gid)),
            };
            if now_uid != uid || now_gid != gid {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("the bridge cannot act as {uid}:{gid}"),
                ));
            }
            Ok(guard)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (uid, gid);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "moving files as another user needs Linux",
            ))
        }
    }
}

impl Drop for AsUser {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some((uid, gid)) = self.previous {
            // SAFETY: plain syscalls restoring the thread's identity.
            unsafe {
                libc::setfsuid(uid);
                libc::setfsgid(gid);
            }
        }
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
    tls: Option<ServerTls>,
}

impl Bridge {
    /// Bind the listener and load the attempt state. A state file that
    /// cannot be read is an error, never a fresh start: that would revive
    /// ended attempts.
    pub fn bind(config: Config) -> io::Result<Bridge> {
        let (attempts, written) = match fs::read_to_string(&config.state_file) {
            Ok(text) => (
                Attempts::decode(&text).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unreadable attempt state {}", config.state_file.display()),
                    )
                })?,
                Some(text),
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (Attempts::default(), None),
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
                state: Mutex::new(State {
                    attempts,
                    written,
                    tampered: false,
                }),
                groups: Mutex::new(Vec::new()),
                children: Mutex::new(HashSet::new()),
                base_env,
                run_as: config.run_as,
            }),
            lifeline: config.lifeline,
            tls: config.tls,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve until the process ends. Makes this process a child subreaper
    /// and not dumpable (Linux), and reaps orphans; SIGTERM and SIGINT end
    /// it as the module documentation says if [`block_signals`] was called
    /// before any thread started, and otherwise act as they would.
    pub fn serve(self) -> io::Result<()> {
        harden();
        {
            let shared = self.shared.clone();
            thread::spawn(move || supervise(shared));
        }
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
            let tls = self.tls.clone();
            thread::spawn(move || {
                let _ = stream.set_nodelay(true);
                let stream = match &tls {
                    None => Stream::Tcp(stream),
                    // A TLS client starts with a handshake record (0x16).
                    // Anything else is plain HTTP, which a TLS bridge
                    // answers only for the health check: Substrate's
                    // wakeup probe cannot speak TLS.
                    Some(tls) => match starts_tls(&stream) {
                        Ok(true) => match tls::accept(stream, tls, HEAD_TIMEOUT) {
                            Ok(stream) => stream,
                            Err(_) => return,
                        },
                        Ok(false) => {
                            let _ = health_only(stream);
                            return;
                        }
                        Err(_) => return,
                    },
                };
                let _ = connection(&shared, stream);
            });
        }
        Ok(())
    }
}

/// Whether the client's first byte opens a TLS handshake.
fn starts_tls(stream: &std::net::TcpStream) -> io::Result<bool> {
    stream.set_read_timeout(Some(HEAD_TIMEOUT))?;
    let mut first = [0u8; 1];
    let seen = stream.peek(&mut first)?;
    Ok(seen == 1 && first[0] == 0x16)
}

/// Answer a plain-HTTP request to a bridge that serves TLS: the health
/// check, and a refusal for anything else, which must not cross in the
/// clear.
fn health_only(stream: std::net::TcpStream) -> io::Result<()> {
    let mut stream = Stream::Tcp(stream);
    let head = ws::read_head(&mut stream)?;
    let mut start = head.start.split_whitespace();
    let (method, path) = (start.next().unwrap_or(""), start.next().unwrap_or(""));
    let path = path.split('?').next().unwrap_or(path);
    match (
        method,
        path.ends_with("/healthz"),
        head.lists("upgrade", "websocket"),
    ) {
        ("GET", true, false) => ws::respond(&mut stream, "200 OK", "ok\n"),
        _ => ws::respond(
            &mut stream,
            "403 Forbidden",
            "this bridge takes requests over TLS only\n",
        ),
    }
}

fn connection(shared: &Arc<Shared>, mut stream: Stream) -> io::Result<()> {
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
    let send =
        |frame: Frame| -> io::Result<()> { writer.lock_recovering("writer").send(&frame.encode()) };
    let Some(first) = reader.recv()? else {
        return Ok(());
    };
    let request = Frame::decode(&first)?;
    let as_user = || AsUser::enter(shared.run_as);
    let outcome = match request {
        Frame::Exec { argv, cwd, env } => {
            return exec(shared, claims.seq, reader, writer, argv, cwd, env)
        }
        Frame::PutFile { path, mode } => as_user()
            .and_then(|_user| put_file(&path, mode, &mut reader))
            .map(|()| Frame::Done),
        Frame::GetFile { path } => as_user().and_then(|_user| get_file(&path, &send)),
        Frame::PutTree { path } => as_user().and_then(|_user| {
            absolute(&path).and_then(|root| {
                tree::receive(&root, &mut || next_frame(&mut reader)).map(|()| Frame::Done)
            })
        }),
        Frame::GetTree { path } => as_user().and_then(|_user| {
            absolute(&path)
                .and_then(|root| tree::send(&root, &mut |frame| send(frame)))
                .map(|()| Frame::End)
        }),
        Frame::EndAttempt => shared.end(claims.seq).map(Frame::Survivors),
        Frame::Shutdown => Ok(Frame::Survivors(shared.teardown(|_| true))),
        Frame::Status => shared.report(),
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
    let _ = writer.lock_recovering("writer").close();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match reader.recv() {
            Ok(Some(_)) => continue,
            _ => break,
        }
    }
    writer.lock_recovering("writer").abort();
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
    // Owner-only until the content is in; its mode comes after.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&partial)?;
    fs::set_permissions(&partial, fs::Permissions::from_mode(0o600))?;
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
            writer.lock_recovering("writer").send(&frame.encode())
        }
    };
    let running = shared.live_groups().len();
    let children = shared.children.lock_recovering("children");
    let spawned = match argv.split_first() {
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty argument vector",
        )),
        Some(_) if running >= MAX_EXECS => Err(io::Error::other(format!(
            "the bridge already runs {MAX_EXECS} execs"
        ))),
        Some((program, args)) => {
            let mut command = Command::new(OsStr::from_bytes(program));
            command
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
                .process_group(0);
            // As root, std also clears the supplementary groups.
            if let Some((uid, gid)) = shared.run_as {
                command.uid(uid).gid(gid);
            }
            // The bridge's threads block the signals its supervisor waits
            // for (`block_signals`); the child must not inherit that.
            // SAFETY: only async-signal-safe calls between fork and exec.
            unsafe {
                command.pre_exec(|| {
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                    Ok(())
                });
            }
            command.spawn()
        }
    };
    let mut child: Child = match spawned {
        Ok(child) => child,
        Err(error) => {
            drop(children);
            send(Frame::failed(&error))?;
            finish(reader, &writer);
            return Ok(());
        }
    };
    let pid = child.id();
    let mut children = children;
    children.insert(pid);
    drop(children);
    let group = Arc::new(Group {
        pgid: pid,
        seq,
        program: argv[0].clone(),
        started: Instant::now(),
        done: AtomicBool::new(false),
        reaped: Mutex::new(false),
        undelivered: Arc::new(AtomicUsize::new(3)),
    });
    shared
        .groups
        .lock_recovering("groups")
        .push(Arc::downgrade(&group));
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let stdin = child.stdin.take().expect("piped");
    send(Frame::Started { pid })?;

    {
        let send = send.clone();
        let undelivered = group.undelivered.clone();
        thread::spawn(move || {
            pump(stdout, send, Frame::Stdout, Frame::StdoutClosed);
            undelivered.fetch_sub(1, Ordering::AcqRel);
        });
    }
    {
        let send = send.clone();
        let undelivered = group.undelivered.clone();
        thread::spawn(move || {
            pump(stderr, send, Frame::Stderr, Frame::StderrClosed);
            undelivered.fetch_sub(1, Ordering::AcqRel);
        });
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
        let shared = shared.clone();
        thread::spawn(move || {
            wait_exited(pid);
            let status = {
                let mut children = shared.children.lock_recovering("children");
                let mut reaped = group.reaped.lock_recovering("reaped");
                let status = child.wait();
                *reaped = true;
                children.remove(&pid);
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
            group.undelivered.fetch_sub(1, Ordering::AcqRel);
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
    branchyard_support::join_reporting("waiter", waiter);
    writer.lock_recovering("writer").abort();
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
