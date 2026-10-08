//! Run a harness [`Driver`] against a harness process, local or inside a
//! sandbox.
//!
//! [`branchyard_harness`] drivers are sans-IO: they build a launch and frames
//! and consume the lines the harness prints. A [`Session`] supplies the I/O:
//! it starts the launch argument vector without a shell through a
//! [`SandboxProvider`]'s exec ([`Session::start_in`]) or, by default, as a
//! local process in its own process group ([`Session::start`], through
//! [`LocalProvider`]). It feeds each stdout line to the driver, writes the
//! frames it produces, keeps stderr as diagnostics and optionally writes a
//! JSONL transcript of both directions.
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
//! - [`Session::close`] and [`Session::kill`] tear down every process the
//!   exec started ([`Process::teardown`]: the local process group, or the
//!   provider's equivalent), and `close` names the descendants that
//!   outlived the harness. Dropping a session without either tears it down
//!   too.
//!
//! What it does not guarantee:
//!
//! - Isolation. [`Session::start`] runs a local process with a scrubbed
//!   environment and a private `HOME` ([`Environment`]); proxy and system
//!   variables pass through, and the process sees the host filesystem. A
//!   descendant that leaves the process group escapes teardown. Under
//!   [`Session::start_in`], isolation is whatever the provider documents.
//! - Bounded frames. Lines are not size-limited; the reader applies
//!   backpressure to the harness through a bounded queue.
//! - Portability. It is Unix-only: the local provider uses process groups,
//!   and `ps` and `kill` on `PATH` for naming and signalling the group. On
//!   other targets the crate is empty.
//!
//! The `fake-acp-agent` binary in this crate is a test fixture speaking just
//! enough ACP v1 for the crate's tests; it is not a harness.
#![warn(missing_docs)]
#![cfg(unix)]

pub mod egress;
mod local;
#[cfg(target_os = "linux")]
mod netns;

use branchyard_support::LockExt as _;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use branchyard_harness::{
    Capabilities, Driver, Event, Frame, NativeSession, Open, PermissionDecision, PermissionKey,
    PermissionRequest, Rejected, TurnOutcome, Usage,
};
use branchyard_sandbox::{ExecSpec, Process, SandboxProvider};
use serde_json::json;

pub use local::{LocalProcess, LocalProvider};

/// `off` in this variable makes [`LocalProvider::confinement`] report that
/// a local process's network cannot be confined, without trying; see
/// `docs/egress.md`.
pub const ENV_EGRESS_NETNS: &str = "BRANCHYARD_EGRESS_NETNS";

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

    /// The private (or, with [`Environment::inherit`], inherited) `HOME` the harness runs with.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The complete environment this describes, read from this process's
    /// environment now. Variables whose names are not UTF-8 are kept as
    /// inherited; no rule can name them.
    pub fn resolve(&self) -> BTreeMap<OsString, OsString> {
        let mut env: BTreeMap<OsString, OsString> = std::env::vars_os()
            .filter(|(name, _)| {
                let Some(name) = name.to_str() else {
                    return true;
                };
                let upper = name.to_ascii_uppercase();
                let stripped = self.strip.iter().any(|p| upper.starts_with(p.as_str()));
                !((stripped && !self.keep.contains(name)) || self.remove.contains(name))
            })
            .collect();
        if !self.inherit_home {
            env.insert("HOME".into(), self.home.clone().into_os_string());
        }
        for (name, value) in &self.set {
            env.insert(name.into(), value.into());
        }
        env
    }

    /// Replace `command`'s environment with this one.
    pub fn apply(&self, command: &mut Command) {
        command.env_clear().envs(self.resolve());
    }
}

/// Why a session operation failed.
#[derive(Debug)]
pub enum RuntimeError {
    /// The launch could not be started.
    Spawn {
        /// The command line that could not be started.
        argv: Vec<String>,
        /// The operating-system error.
        source: io::Error,
    },
    /// Writing to the harness or the transcript failed.
    Io {
        /// What was being written: the harness's input or the transcript.
        context: &'static str,
        /// The operating-system error.
        source: io::Error,
    },
    /// Nothing matched before the deadline. The session is still usable.
    Timeout(Duration),
    /// The driver refused the operation; nothing was written.
    Rejected(Rejected),
    /// The harness's stdout closed. The driver's closing events, such as
    /// [`Event::OutcomeUnknown`], are in [`Session::events`].
    HarnessExited {
        /// What the harness wrote to standard error before it closed.
        stderr: String,
    },
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
    /// The turn's number, counting from 1 in its session.
    pub turn: u64,
    /// How the turn ended.
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
    process: Box<dyn Process>,
    stdin: Option<Box<dyn Write + Send>>,
    lines: Receiver<Vec<u8>>,
    stderr: Arc<Stderr>,
    transcript: Option<File>,
    events: Vec<Event>,
    /// Indices into `events` not yet delivered to the caller.
    undelivered: VecDeque<usize>,
    transport_closed: bool,
    session: Option<NativeSession>,
    cost_usd: Option<f64>,
    /// The process has been torn down; `Drop` has nothing to do.
    finished: bool,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("process", &self.process.id())
            .field("session", &self.session)
            .field("events", &self.events.len())
            .field("transport_closed", &self.transport_closed)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Open the driver, spawn the launch argument vector (no shell) as a
    /// local process in its own process group, and write the open frames.
    /// Does not wait for the handshake; see [`Session::wait_ready`].
    pub fn start(
        driver: Box<dyn Driver>,
        open: Open,
        env: &Environment,
        transcript: Option<&Path>,
    ) -> Result<Session, RuntimeError> {
        Session::launch(driver, open, env.resolve(), transcript, |spec| {
            LocalProvider::spawn(spec).map(|p| Box::new(p) as Box<dyn Process>)
        })
    }

    /// Like [`Session::start`], but confined to its own network namespace
    /// with one listener on `127.0.0.1:port` inside it, which is returned
    /// for the caller to serve ([`LocalProvider::spawn_confined`]).
    pub fn start_confined(
        driver: Box<dyn Driver>,
        open: Open,
        env: &Environment,
        transcript: Option<&Path>,
        port: u16,
    ) -> Result<(Session, std::net::TcpListener), RuntimeError> {
        let mut listener = None;
        let session = Session::launch(driver, open, env.resolve(), transcript, |spec| {
            let (process, bound) = LocalProvider::spawn_confined(spec, port)?;
            listener = Some(bound);
            Ok(Box::new(process) as Box<dyn Process>)
        })?;
        let listener = listener.ok_or_else(|| RuntimeError::Io {
            context: "the confined listener",
            source: io::Error::other("the confined process did not hand back its listener"),
        })?;
        Ok((session, listener))
    }

    /// Like [`Session::start`], but exec the launch inside `sandbox` through
    /// `provider`, with `env` added to the provider's base environment. The
    /// driver's launch directory, from [`Open::cwd`], is a sandbox path.
    pub fn start_in(
        driver: Box<dyn Driver>,
        open: Open,
        provider: &dyn SandboxProvider,
        sandbox: &str,
        env: BTreeMap<OsString, OsString>,
        transcript: Option<&Path>,
    ) -> Result<Session, RuntimeError> {
        Session::launch(driver, open, env, transcript, |spec| {
            provider.exec(sandbox, spec).map_err(io::Error::from)
        })
    }

    fn launch(
        mut driver: Box<dyn Driver>,
        open: Open,
        env: BTreeMap<OsString, OsString>,
        transcript: Option<&Path>,
        exec: impl FnOnce(&ExecSpec) -> io::Result<Box<dyn Process>>,
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
        let spec = ExecSpec {
            argv: opened.launch.argv,
            cwd: PathBuf::from(&opened.launch.cwd),
            env,
        };
        if spec.argv.is_empty() {
            return Err(RuntimeError::Spawn {
                argv: spec.argv,
                source: io::Error::new(io::ErrorKind::InvalidInput, "empty argument vector"),
            });
        }
        let mut process = match exec(&spec) {
            Ok(process) => process,
            Err(source) => {
                return Err(RuntimeError::Spawn {
                    argv: spec.argv,
                    source,
                })
            }
        };
        let piped = |what: &str| RuntimeError::Io {
            context: "exec",
            source: io::Error::other(format!("the provider did not pipe {what}")),
        };
        let (Some(stdin), Some(stdout), Some(mut pipe)) = (
            process.take_stdin(),
            process.take_stdout(),
            process.take_stderr(),
        ) else {
            return Err(piped("stdin, stdout and stderr"));
        };

        let (sender, lines) = mpsc::sync_channel(LINE_QUEUE);
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
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut bytes = sink.bytes.lock_recovering("bytes");
                bytes.extend_from_slice(&chunk[..n]);
                let excess = bytes.len().saturating_sub(STDERR_KEEP);
                bytes.drain(..excess);
            }
            sink.done.store(true, Ordering::Release);
        });

        let mut session = Session {
            driver,
            stdin: Some(stdin),
            process,
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

    /// The provider's identifier for the harness process: its pid for a
    /// local process, which also leads its own process group.
    pub fn process_id(&self) -> String {
        self.process.id()
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

    /// Deliver `text` into the turn in flight, as the driver's
    /// [`Driver::steer`] does. The
    /// harness confirms or refuses it with `SteerAccepted` or
    /// `SteerRejected`.
    pub fn steer(&mut self, text: &str) -> Result<(), RuntimeError> {
        let frames = self.driver.steer(text)?;
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

    /// Whether the turn in flight is held open for the harness's
    /// background tasks; see [`Driver::holding`].
    pub fn holding(&self) -> bool {
        self.driver.holding()
    }

    /// Whether the turn in flight is held open at all, including the wait
    /// for the harness's answer to its tasks' notification; see
    /// [`Driver::held`].
    pub fn held(&self) -> bool {
        self.driver.held()
    }

    /// The last few hundred bytes of the harness's stderr.
    pub fn stderr_tail(&self) -> String {
        let bytes = self.stderr.bytes.lock_recovering("bytes");
        let start = bytes.len().saturating_sub(STDERR_TAIL);
        String::from_utf8_lossy(&bytes[start..]).trim().to_owned()
    }

    /// End the session: write the driver's request to end it
    /// ([`Driver::close`], when its protocol has one), close stdin, and
    /// wait up to `grace` for the harness to exit, killing it if it does
    /// not. Then tear down what remains of its process group (or the
    /// provider's equivalent), naming it, and let the driver see the
    /// transport close.
    ///
    /// The wait ends when the harness process exits, not when stdout closes,
    /// so a descendant holding the pipe open does not stall it.
    pub fn close(mut self, grace: Duration) -> Result<Closed, RuntimeError> {
        let start = self.events.len();
        let goodbye = self.driver.close();
        if let Err(error) = self.write(&goodbye) {
            // A harness already gone cannot be asked; closing its input is
            // all there is left to do.
            let gone = matches!(&error, RuntimeError::Io { source, .. }
                if source.kind() == io::ErrorKind::BrokenPipe);
            if !gone {
                branchyard_support::best_effort::<(), _>(
                    "ask the harness to end its session",
                    Err(error),
                );
            }
        }
        self.stdin.take();
        let deadline = Deadline::after(grace);
        let mut forced = false;
        loop {
            if self.process.try_wait().map_err(io_error("wait"))?.is_some() {
                break;
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                forced = true;
                branchyard_support::best_effort("kill process", self.process.kill());
                break;
            }
            let slice = remaining.min(Duration::from_millis(50));
            // Stdout closing is not the harness exiting: keep enforcing the
            // grace period until the process itself is gone.
            if self.transport_closed {
                std::thread::sleep(slice);
            } else {
                self.receive_line(slice)?;
            }
        }
        self.process.wait().map_err(io_error("wait"))?;
        let survivors = self.teardown();
        self.drain();
        Ok(Closed {
            survivors,
            cost_usd: self.cost_usd,
            forced,
            events: self.events[start..].to_vec(),
        })
    }

    /// Tear down the process and its group now and return the events the
    /// driver produced as the transport closed, such as
    /// [`Event::OutcomeUnknown`] for a turn in flight.
    pub fn kill(mut self) -> Result<Vec<Event>, RuntimeError> {
        let start = self.events.len();
        self.process.teardown();
        branchyard_support::best_effort("kill process", self.process.kill());
        self.process.wait().map_err(io_error("wait"))?;
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

    /// Name the exec's live processes, then kill them all.
    fn teardown(&mut self) -> Vec<String> {
        let survivors = self.process.teardown();
        self.finished = true;
        survivors
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.finished {
            self.process.teardown();
            branchyard_support::best_effort("kill process", self.process.kill());
            branchyard_support::best_effort("reap process", self.process.wait());
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
