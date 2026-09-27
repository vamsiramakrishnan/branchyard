//! Run a harness [`Driver`] against a local harness process.
//!
//! [`branchyard_harness`] drivers are sans-IO: they build a launch and frames
//! and consume the lines the harness prints. A [`Session`] supplies the I/O:
//! it spawns the launch argument vector without a shell in its own process
//! group, feeds each stdout line to the driver, writes the frames it produces,
//! keeps stderr as diagnostics and optionally writes a JSONL transcript of
//! both directions.
//!
//! What it guarantees:
//!
//! - Every event the driver produces is recorded in [`Session::events`] as
//!   the line arrives, before any caller sees it, and is delivered once
//!   through [`Session::next_event`], [`Session::wait_for`] or
//!   [`Session::run_turn`]. A driver batch is never cut short.
//! - When stdout closes, the driver is told once, so a turn in flight ends
//!   as [`Event::OutcomeUnknown`]. [`Session::kill`] tells it even if the
//!   pipe stays open.
//! - [`Session::close`] and [`Session::kill`] signal the whole process
//!   group, and `close` names the descendants that outlived the harness.
//!   Dropping a session without either kills the group too.
//!
//! What it does not guarantee:
//!
//! - Isolation. This is a local process with a scrubbed environment and a
//!   private `HOME` ([`Environment`]), standing in for `SandboxProvider.exec`.
//!   Proxy and system variables pass through, and the process sees the host
//!   filesystem. A descendant that leaves the process group escapes teardown.
//! - Bounded frames. Lines are not size-limited; the reader applies
//!   backpressure to the harness through a bounded queue.
//! - Portability. It is Unix-only: process groups, and `ps` and `kill` on
//!   `PATH` for naming and signalling the group. On other targets the crate
//!   is empty.
//!
//! The `fake-acp-agent` binary in this crate is a test fixture speaking just
//! enough ACP v1 for the crate's tests; it is not a harness.

#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use branchyard_harness::{
    Capabilities, Driver, Event, Frame, NativeSession, Open, PermissionDecision, PermissionKey,
    PermissionRequest, Rejected, TurnOutcome, Usage,
};
use serde_json::json;

/// Lines buffered between the stdout reader and the session. When full, the
/// reader stops reading and the harness blocks on its own writes.
const LINE_QUEUE: usize = 1024;
/// Stderr kept for diagnostics.
const STDERR_KEEP: usize = 64 * 1024;
/// Stderr quoted in errors.
const STDERR_TAIL: usize = 400;
/// How long to wait for stderr to reach EOF once stdout has closed, so the
/// harness's last words make it into the error.
const STDERR_SETTLE: Duration = Duration::from_millis(500);
/// How long to wait for stdout to close once the process group is dead.
const DRAIN: Duration = Duration::from_secs(10);
/// Variable-name prefixes stripped by default: credentials and
/// nested-session markers.
const DEFAULT_STRIP: [&str; 4] = ["ANTHROPIC", "CLAUDE", "OPENAI", "CODEX"];

/// The environment a harness process runs with.
///
/// It inherits this process's environment, removes every variable whose
/// upper-cased name starts with a stripped prefix unless it is kept by exact
/// name, removes variables named exactly by [`Environment::remove`], sets
/// `HOME` to a private directory, then applies explicit settings.
///
/// [`Environment::inherit`] is the exception: it strips no prefixes and
/// leaves `HOME` as it is, for running a harness with the user's own login.
#[derive(Clone, Debug)]
pub struct Environment {
    home: PathBuf,
    /// Leave `HOME` as inherited instead of setting it.
    inherit_home: bool,
    keep: BTreeSet<String>,
    strip: Vec<String>,
    remove: BTreeSet<String>,
    set: BTreeMap<String, String>,
}

impl Environment {
    /// Strip the default prefixes (`ANTHROPIC`, `CLAUDE`, `OPENAI`, `CODEX`)
    /// and use `home` as `HOME`.
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            inherit_home: false,
            keep: BTreeSet::new(),
            strip: DEFAULT_STRIP.iter().map(|p| (*p).to_owned()).collect(),
            remove: BTreeSet::new(),
            set: BTreeMap::new(),
        }
    }

    /// This process's environment, `HOME` and credentials included, with
    /// nothing stripped. It scrubs nothing: use [`Environment::remove`] for
    /// variables the harness must not see. [`Environment::home`] is the
    /// inherited `HOME`, or empty when it is unset.
    pub fn inherit() -> Self {
        Self {
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default(),
            inherit_home: true,
            keep: BTreeSet::new(),
            strip: Vec::new(),
            remove: BTreeSet::new(),
            set: BTreeMap::new(),
        }
    }

    /// Remove this variable, matched by exact name. [`Environment::set`]
    /// still wins.
    pub fn remove(mut self, name: impl Into<String>) -> Self {
        self.remove.insert(name.into());
        self
    }

    /// Pass this variable through even if a stripped prefix matches it.
    pub fn keep(mut self, name: impl Into<String>) -> Self {
        self.keep.insert(name.into());
        self
    }

    /// Also strip variables whose upper-cased name starts with `prefix`.
    pub fn strip(mut self, prefix: impl Into<String>) -> Self {
        self.strip.push(prefix.into().to_ascii_uppercase());
        self
    }

    /// Set a variable after scrubbing; it wins over stripping and `HOME`.
    pub fn set(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.set.insert(name.into(), value.into());
        self
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Apply this environment to `command`.
    pub fn apply(&self, command: &mut Command) {
        for (name, _) in std::env::vars_os() {
            let Some(name) = name.to_str() else { continue };
            let upper = name.to_ascii_uppercase();
            let stripped = self.strip.iter().any(|p| upper.starts_with(p.as_str()));
            if (stripped && !self.keep.contains(name)) || self.remove.contains(name) {
                command.env_remove(name);
            }
        }
        if !self.inherit_home {
            command.env("HOME", &self.home);
        }
        for (name, value) in &self.set {
            command.env(name, value);
        }
    }
}

/// Why a session operation failed.
#[derive(Debug)]
pub enum RuntimeError {
    /// The launch could not be started.
    Spawn {
        argv: Vec<String>,
        source: io::Error,
    },
    /// Writing to the harness or the transcript failed.
    Io {
        context: &'static str,
        source: io::Error,
    },
    /// Nothing matched before the deadline. The session is still usable.
    Timeout(Duration),
    /// The driver refused the operation; nothing was written.
    Rejected(Rejected),
    /// The harness's stdout closed. The driver's closing events, such as
    /// [`Event::OutcomeUnknown`], are in [`Session::events`].
    HarnessExited { stderr: String },
    /// The driver reported [`Event::OpenFailed`].
    OpenFailed(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::Spawn { argv, source } => write!(f, "spawn {argv:?}: {source}"),
            RuntimeError::Io { context, source } => write!(f, "{context}: {source}"),
            RuntimeError::Timeout(after) => write!(f, "timed out after {after:?}"),
            RuntimeError::Rejected(rejected) => write!(f, "rejected: {rejected}"),
            RuntimeError::HarnessExited { stderr } => write!(f, "harness exited: {stderr}"),
            RuntimeError::OpenFailed(reason) => write!(f, "open failed: {reason}"),
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RuntimeError::Spawn { source, .. } | RuntimeError::Io { source, .. } => Some(source),
            RuntimeError::Rejected(rejected) => Some(rejected),
            _ => None,
        }
    }
}

impl From<Rejected> for RuntimeError {
    fn from(rejected: Rejected) -> Self {
        RuntimeError::Rejected(rejected)
    }
}

/// One completed turn.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnReport {
    pub turn: u64,
    pub outcome: TurnOutcome,
    /// The turn's message deltas, concatenated.
    pub text: String,
    /// The last usage the harness reported during the turn. Session totals
    /// when `usage.cumulative`; `None` when it reported none.
    pub usage: Option<Usage>,
    /// Events from the submit through `TurnEnded`.
    pub events: Vec<Event>,
}

/// The result of [`Session::close`].
#[derive(Clone, Debug, PartialEq)]
pub struct Closed {
    /// Command names of process-group members still running after the
    /// harness exited. They have been sent SIGKILL.
    pub survivors: Vec<String>,
    /// The latest cumulative cost the harness reported, if any.
    pub cost_usd: Option<f64>,
    /// The harness did not exit within the grace period and was killed.
    pub forced: bool,
    /// Events produced while closing, ending with `SessionClosed`.
    pub events: Vec<Event>,
}

#[derive(Default)]
struct Stderr {
    bytes: Mutex<Vec<u8>>,
    done: AtomicBool,
}

/// A running harness process wired to its driver.
pub struct Session {
    driver: Box<dyn Driver>,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Vec<u8>>,
    stderr: Arc<Stderr>,
    transcript: Option<File>,
    events: Vec<Event>,
    /// Indices into `events` not yet delivered to the caller.
    undelivered: VecDeque<usize>,
    transport_closed: bool,
    session: Option<NativeSession>,
    cost_usd: Option<f64>,
    /// The process group has been torn down; `Drop` has nothing to do.
    finished: bool,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("pid", &self.child.id())
            .field("session", &self.session)
            .field("events", &self.events.len())
            .field("transport_closed", &self.transport_closed)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Open the driver, spawn the launch argument vector (no shell) in its own
    /// process group, and write the open frames. Does not wait for the
    /// handshake; see [`Session::wait_ready`].
    pub fn start(
        mut driver: Box<dyn Driver>,
        open: Open,
        env: &Environment,
        transcript: Option<&Path>,
    ) -> Result<Session, RuntimeError> {
        let opened = driver.open(open)?;
        let transcript =
            transcript
                .map(File::create)
                .transpose()
                .map_err(|source| RuntimeError::Io {
                    context: "transcript",
                    source,
                })?;
        let argv = opened.launch.argv;
        let Some((program, args)) = argv.split_first() else {
            return Err(RuntimeError::Spawn {
                argv,
                source: io::Error::new(io::ErrorKind::InvalidInput, "empty argument vector"),
            });
        };
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(&opened.launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so teardown reaches every descendant.
            .process_group(0);
        env.apply(&mut command);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(source) => return Err(RuntimeError::Spawn { argv, source }),
        };

        let (sender, lines) = mpsc::sync_channel(LINE_QUEUE);
        let stdout = child.stdout.take().expect("stdout is piped");
        thread::spawn(move || {
            for line in BufReader::new(stdout).split(b'\n') {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Stderr::default());
        let sink = stderr.clone();
        let mut pipe = child.stderr.take().expect("stderr is piped");
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut bytes = sink.bytes.lock().unwrap_or_else(|e| e.into_inner());
                bytes.extend_from_slice(&chunk[..n]);
                let excess = bytes.len().saturating_sub(STDERR_KEEP);
                bytes.drain(..excess);
            }
            sink.done.store(true, Ordering::Release);
        });

        let mut session = Session {
            driver,
            stdin: child.stdin.take(),
            child,
            lines,
            stderr,
            transcript,
            events: Vec::new(),
            undelivered: VecDeque::new(),
            transport_closed: false,
            session: None,
            cost_usd: None,
            finished: false,
        };
        session.write(&opened.frames)?;
        Ok(session)
    }

    /// Wait for the handshake: `Ok` on [`Event::Ready`], an error on
    /// [`Event::OpenFailed`], exit or timeout. Permission requests that arrive
    /// meanwhile stay unanswered.
    pub fn wait_ready(&mut self, timeout: Duration) -> Result<(), RuntimeError> {
        match self.wait_for(timeout, |e| {
            matches!(e, Event::Ready | Event::OpenFailed { .. })
        })? {
            Event::OpenFailed { reason } => Err(RuntimeError::OpenFailed(reason)),
            _ => Ok(()),
        }
    }

    /// The next undelivered event, reading from the harness for up to
    /// `timeout`. Response frames the driver produces are written as lines
    /// arrive. `Ok(None)` on timeout; [`RuntimeError::HarnessExited`] once
    /// stdout has closed and every event has been delivered.
    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<Event>, RuntimeError> {
        Ok(self
            .next_index(timeout)?
            .map(|index| self.events[index].clone()))
    }

    /// Deliver events until `until` matches one, and return it. Events
    /// delivered before it are recorded in [`Session::events`] but not
    /// returned; events after it in the same batch stay undelivered.
    pub fn wait_for(
        &mut self,
        timeout: Duration,
        until: impl Fn(&Event) -> bool,
    ) -> Result<Event, RuntimeError> {
        let deadline = Deadline::after(timeout);
        loop {
            match self.next_index(deadline.remaining())? {
                Some(index) if until(&self.events[index]) => return Ok(self.events[index].clone()),
                Some(_) => {}
                None => return Err(RuntimeError::Timeout(timeout)),
            }
        }
    }

    /// Write `prompt` as a new turn and return its local turn number.
    pub fn submit(&mut self, prompt: &str) -> Result<u64, RuntimeError> {
        let submitted = self.driver.submit(prompt)?;
        self.write(&submitted.frames)?;
        Ok(submitted.turn)
    }

    /// Request cancellation of the turn in flight. The turn's terminal state
    /// still arrives as [`Event::TurnEnded`].
    pub fn interrupt(&mut self) -> Result<(), RuntimeError> {
        let frames = self.driver.interrupt()?;
        self.write(&frames)
    }

    /// Answer an outstanding permission request.
    pub fn respond(
        &mut self,
        key: &PermissionKey,
        decision: PermissionDecision,
    ) -> Result<(), RuntimeError> {
        let frames = self.driver.respond(key, decision)?;
        self.write(&frames)
    }

    /// Submit `prompt` and deliver events until the turn ends, answering every
    /// permission request with `policy`. On timeout the turn is still in
    /// flight; interrupt it or keep waiting.
    pub fn run_turn(
        &mut self,
        prompt: &str,
        policy: &mut dyn FnMut(&PermissionRequest) -> PermissionDecision,
        timeout: Duration,
    ) -> Result<TurnReport, RuntimeError> {
        let deadline = Deadline::after(timeout);
        let start = self.events.len();
        let turn = self.submit(prompt)?;
        loop {
            let Some(index) = self.next_index(deadline.remaining())? else {
                return Err(RuntimeError::Timeout(timeout));
            };
            match &self.events[index] {
                Event::PermissionRequested { request, .. } => {
                    let decision = policy(request);
                    let key = request.key.clone();
                    self.respond(&key, decision)?;
                }
                Event::TurnEnded { turn: t, outcome } if *t == turn => {
                    let outcome = outcome.clone();
                    let events = self.events[start..=index].to_vec();
                    return Ok(TurnReport {
                        turn,
                        outcome,
                        text: text(&events, turn),
                        usage: events.iter().rev().find_map(|e| match e {
                            Event::UsageObserved { turn: t, usage }
                                if t.is_none_or(|t| t == turn) =>
                            {
                                Some(usage.clone())
                            }
                            _ => None,
                        }),
                        events,
                    });
                }
                _ => {}
            }
        }
    }

    /// Every event the driver has produced, in order, delivered or not.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The native session from the latest [`Event::SessionStarted`].
    pub fn session_id(&self) -> Option<&NativeSession> {
        self.session.as_ref()
    }

    /// The latest cumulative cost the harness reported. `None` when it
    /// reported none, which is unknown, not zero.
    pub fn cost_usd(&self) -> Option<f64> {
        self.cost_usd
    }

    /// The driver's capabilities before negotiation.
    pub fn capabilities(&self) -> Capabilities {
        self.driver.capabilities()
    }

    /// The last few hundred bytes of the harness's stderr.
    pub fn stderr_tail(&self) -> String {
        let bytes = self.stderr.bytes.lock().unwrap_or_else(|e| e.into_inner());
        let start = bytes.len().saturating_sub(STDERR_TAIL);
        String::from_utf8_lossy(&bytes[start..]).trim().to_owned()
    }

    /// Close stdin and wait up to `grace` for the harness to exit, killing it
    /// if it does not. Then kill what remains of its process group, naming
    /// it, and let the driver see the transport close.
    ///
    /// The wait ends when the harness process exits, not when stdout closes,
    /// so a descendant holding the pipe open does not stall it.
    pub fn close(mut self, grace: Duration) -> Result<Closed, RuntimeError> {
        let start = self.events.len();
        self.stdin.take();
        let deadline = Deadline::after(grace);
        let mut forced = false;
        while !self.transport_closed {
            if self.child.try_wait().map_err(io_error("wait"))?.is_some() {
                break;
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                forced = true;
                let _ = self.child.kill();
                break;
            }
            self.receive_line(remaining.min(Duration::from_millis(50)))?;
        }
        self.child.wait().map_err(io_error("wait"))?;
        let survivors = self.teardown();
        self.drain();
        Ok(Closed {
            survivors,
            cost_usd: self.cost_usd,
            forced,
            events: self.events[start..].to_vec(),
        })
    }

    /// Kill the process group now and return the events the driver produced
    /// as the transport closed, such as [`Event::OutcomeUnknown`] for a turn
    /// in flight.
    pub fn kill(mut self) -> Result<Vec<Event>, RuntimeError> {
        let start = self.events.len();
        signal_group(self.child.id());
        let _ = self.child.kill();
        self.child.wait().map_err(io_error("wait"))?;
        self.teardown();
        self.drain();
        Ok(self.events[start..].to_vec())
    }

    fn next_index(&mut self, timeout: Duration) -> Result<Option<usize>, RuntimeError> {
        let deadline = Deadline::after(timeout);
        loop {
            if let Some(index) = self.undelivered.pop_front() {
                return Ok(Some(index));
            }
            if self.transport_closed {
                return Err(RuntimeError::HarnessExited {
                    stderr: self.stderr_tail(),
                });
            }
            if !self.receive_line(deadline.remaining())? {
                return Ok(None);
            }
        }
    }

    /// Feed one line, or the transport's close, to the driver. False on
    /// timeout.
    fn receive_line(&mut self, timeout: Duration) -> Result<bool, RuntimeError> {
        match self.lines.recv_timeout(timeout) {
            Ok(line) => {
                if let Some(transcript) = &mut self.transcript {
                    let entry = json!({"dir": "in", "line": String::from_utf8_lossy(&line)});
                    writeln!(transcript, "{entry}").map_err(io_error("transcript"))?;
                }
                let output = self.driver.receive(&line);
                self.record(output.events);
                // The events are recorded first so a write failure loses none.
                // A closed pipe means the harness is going away; its reader
                // will report that.
                match self.write(&output.frames) {
                    Err(RuntimeError::Io { source, .. })
                        if source.kind() == io::ErrorKind::BrokenPipe => {}
                    other => other?,
                }
                Ok(true)
            }
            Err(RecvTimeoutError::Timeout) => Ok(false),
            Err(RecvTimeoutError::Disconnected) => {
                self.close_transport();
                Ok(true)
            }
        }
    }

    fn close_transport(&mut self) {
        if self.transport_closed {
            return;
        }
        self.transport_closed = true;
        let settle = Deadline::after(STDERR_SETTLE);
        while !self.stderr.done.load(Ordering::Acquire) && !settle.remaining().is_zero() {
            thread::sleep(Duration::from_millis(10));
        }
        let events = self.driver.transport_closed();
        self.record(events);
    }

    /// After the group is dead, let stdout reach EOF so the driver sees every
    /// last line; if something outside the group holds the pipe, report the
    /// close anyway.
    fn drain(&mut self) {
        let deadline = Deadline::after(DRAIN);
        while !self.transport_closed && !deadline.remaining().is_zero() {
            if self.receive_line(deadline.remaining()).is_err() {
                break;
            }
        }
        self.close_transport();
    }

    fn record(&mut self, events: Vec<Event>) {
        for event in events {
            match &event {
                Event::UsageObserved { usage, .. } if usage.cumulative => {
                    if let Some(cost) = usage.cost_usd {
                        self.cost_usd = Some(self.cost_usd.map_or(cost, |c| c.max(cost)));
                    }
                }
                Event::SessionStarted { session, .. } => self.session = Some(session.clone()),
                _ => {}
            }
            self.undelivered.push_back(self.events.len());
            self.events.push(event);
        }
    }

    fn write(&mut self, frames: &[Frame]) -> Result<(), RuntimeError> {
        if frames.is_empty() {
            return Ok(());
        }
        let stdin = self.stdin.as_mut().ok_or_else(|| RuntimeError::Io {
            context: "write",
            source: io::Error::new(io::ErrorKind::BrokenPipe, "stdin is closed"),
        })?;
        for frame in frames {
            if let Some(transcript) = &mut self.transcript {
                let line = String::from_utf8_lossy(frame);
                let entry = json!({"dir": "out", "line": line.trim()});
                writeln!(transcript, "{entry}").map_err(io_error("transcript"))?;
            }
            stdin.write_all(frame).map_err(io_error("write"))?;
        }
        stdin.flush().map_err(io_error("flush"))
    }

    /// Name the process group's live members, then kill the group.
    fn teardown(&mut self) -> Vec<String> {
        let pgid = self.child.id();
        let survivors = group_members(pgid);
        signal_group(pgid);
        self.finished = true;
        survivors
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.finished {
            signal_group(self.child.id());
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn text(events: &[Event], turn: u64) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            Event::MessageDelta { turn: t, text } if *t == turn => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn io_error(context: &'static str) -> impl Fn(io::Error) -> RuntimeError {
    move |source| RuntimeError::Io { context, source }
}

/// Command names of the live (non-zombie) members of process group `pgid`.
/// Empty when `ps` is unavailable.
fn group_members(pgid: u32) -> Vec<String> {
    let Ok(listing) = Command::new("ps")
        .args(["-A", "-o", "pgid=", "-o", "stat=", "-o", "comm="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let pgid = pgid.to_string();
    String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (group, stat) = (fields.next()?, fields.next()?);
            let name = fields.collect::<Vec<_>>().join(" ");
            (group == pgid && !stat.starts_with('Z') && !name.is_empty()).then_some(name)
        })
        .collect()
}

fn signal_group(pgid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// A deadline that tolerates `Duration::MAX`.
struct Deadline(Option<Instant>);

impl Deadline {
    fn after(timeout: Duration) -> Self {
        Deadline(Instant::now().checked_add(timeout))
    }

    fn remaining(&self) -> Duration {
        match self.0 {
            Some(at) => at.saturating_duration_since(Instant::now()),
            None => Duration::MAX,
        }
    }
}
