//! Waiting for a condition in a test.
//!
//! One default timeout ([`DEFAULT_TIMEOUT`], 60 s, the longest any test file
//! used before), multiplied by `BY_TEST_TIMEOUT_SCALE` (a number, for slow
//! machines or sanitizers), and one poll interval with a little jitter so
//! that two waiters do not poll in lockstep. A condition that already holds
//! returns at once, before the first sleep. A condition that never holds
//! fails with what was being waited for and the last value observed.
//!
//! Tests do not call `std::thread::sleep` to wait for something: they call
//! [`until`] (or [`until_for`], [`until_value`], [`gone`], [`exec`]).
//! `tools/check_test_hygiene.py` enforces that.
//!
//! ```
//! use branchyard_testkit::wait;
//! let started = std::time::Instant::now();
//! // A `bool` check:
//! wait::until("the clock to run", || started.elapsed().as_nanos() > 0);
//! // An `Option` check hands back what it found:
//! let n = wait::until("a number", || Some(7));
//! assert_eq!(n, 7);
//! // A `Result` check shows the last error when it times out:
//! let ok: u8 = wait::until("a parse", || "5".parse::<u8>());
//! assert_eq!(ok, 5);
//! ```

use std::fmt::Debug;
use std::time::{Duration, Instant};

/// How long [`until`] waits before `BY_TEST_TIMEOUT_SCALE`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// The base poll interval; each poll adds up to [`JITTER_MS`] more.
const POLL_MS: u64 = 20;
const JITTER_MS: u64 = 5;

/// `BY_TEST_TIMEOUT_SCALE`: a positive number multiplying every timeout.
/// Unset means 1. A value that is not a positive number is a mistake worth
/// failing on rather than silently ignoring.
pub fn scale() -> f64 {
    match std::env::var("BY_TEST_TIMEOUT_SCALE") {
        Err(_) => 1.0,
        Ok(text) => match text.trim().parse::<f64>() {
            Ok(n) if n.is_finite() && n > 0.0 => n,
            _ => panic!("BY_TEST_TIMEOUT_SCALE must be a positive number, got {text:?}"),
        },
    }
}

/// `timeout` multiplied by [`scale`].
pub fn scaled(timeout: Duration) -> Duration {
    timeout.mul_f64(scale())
}

/// What a check observed: either the condition holds (with what it found)
/// or it does not (with a description of what was seen instead).
pub trait Observation {
    type Output;
    /// `Ok` when the condition holds, else the last thing observed.
    fn settle(self) -> Result<Self::Output, String>;
}

impl Observation for bool {
    type Output = ();
    fn settle(self) -> Result<(), String> {
        if self {
            Ok(())
        } else {
            Err("false".into())
        }
    }
}

impl<T> Observation for Option<T> {
    type Output = T;
    fn settle(self) -> Result<T, String> {
        self.ok_or_else(|| "None".into())
    }
}

impl<T, E: std::fmt::Display> Observation for Result<T, E> {
    type Output = T;
    fn settle(self) -> Result<T, String> {
        self.map_err(|e| e.to_string())
    }
}

/// Poll `check` until it holds, for [`DEFAULT_TIMEOUT`] (scaled).
///
/// `check` returns a `bool`, an `Option<T>` (returned when `Some`) or a
/// `Result<T, E>` (returned when `Ok`). On timeout the test fails with
/// `what` and the last observation.
#[track_caller]
pub fn until<R: Observation>(what: &str, check: impl FnMut() -> R) -> R::Output {
    until_for(what, DEFAULT_TIMEOUT, check)
}

/// [`until`] with a timeout of your own (still multiplied by
/// `BY_TEST_TIMEOUT_SCALE`). Use a shorter one only to assert that something
/// does *not* happen in time; never to make a test faster.
#[track_caller]
pub fn until_for<R: Observation>(
    what: &str,
    timeout: Duration,
    check: impl FnMut() -> R,
) -> R::Output {
    match try_until_for(timeout, check) {
        Ok(found) => found,
        Err(last) => panic!(
            "timed out after {:?} waiting for {what}; last observed: {last}",
            scaled(timeout)
        ),
    }
}

/// Poll `check` for `timeout` (multiplied by `BY_TEST_TIMEOUT_SCALE`) and
/// return `Err(last observation)` instead of failing when it never holds:
/// for a step inside a retry, or to assert that something does not happen.
pub fn try_until_for<R: Observation>(
    timeout: Duration,
    mut check: impl FnMut() -> R,
) -> Result<R::Output, String> {
    let deadline = Instant::now() + scaled(timeout);
    let mut polls = 0u64;
    loop {
        let last = match check().settle() {
            Ok(found) => return Ok(found),
            Err(last) => last,
        };
        if Instant::now() >= deadline {
            return Err(last);
        }
        pause(polls);
        polls += 1;
    }
}

/// [`until_for`] for a `bool` check whose failure should show more than
/// `false`: `context` is called once, after the timeout, and its text is
/// appended to the failure (a log, a transcript, the calls recorded so far).
#[track_caller]
pub fn until_with_context(
    what: &str,
    timeout: Duration,
    mut done: impl FnMut() -> bool,
    context: impl Fn() -> String,
) {
    if try_until_for(timeout, &mut done).is_err() {
        panic!(
            "timed out after {:?} waiting for {what}\n{}",
            scaled(timeout),
            context()
        );
    }
}

/// Poll `observe` until `done` accepts its value; returns that value. On
/// timeout the failure shows the last value observed.
#[track_caller]
pub fn until_value<T: Debug>(
    what: &str,
    mut observe: impl FnMut() -> T,
    done: impl Fn(&T) -> bool,
) -> T {
    until(what, || {
        let seen = observe();
        if done(&seen) {
            Ok(seen)
        } else {
            Err(format!("{seen:?}"))
        }
    })
}

/// The one place a test sleeps: a fixed interval plus a jitter taken from
/// the poll count (no randomness, so a failure replays the same way).
fn pause(polls: u64) {
    let jitter = polls.wrapping_mul(2654435761) >> 7;
    std::thread::sleep(Duration::from_millis(POLL_MS + jitter % (JITTER_MS + 1)));
}

/// True while `pid` is a live, non-zombie process.
pub fn alive(pid: impl Into<u64>) -> bool {
    let pid: u64 = pid.into();
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // "pid (comm) S ..." where comm may itself contain ")".
        let state = stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next();
        return !matches!(state, Some("Z") | Some("X"));
    }
    if std::path::Path::new("/proc/self").exists() {
        return false;
    }
    // No /proc (macOS): ask ps.
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

/// Wait until `pid` is no longer a live process.
#[track_caller]
pub fn gone(pid: impl Into<u64>) {
    let pid: u64 = pid.into();
    until(&format!("pid {pid} to exit"), || !alive(pid));
}

/// Wait until `pid` runs a program named `name`: a forked child is named
/// after its parent until it execs. Needs `/proc`.
#[track_caller]
pub fn exec(pid: impl Into<u64>, name: &str) {
    let pid: u64 = pid.into();
    until(&format!("pid {pid} to run {name}"), || {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm == format!("{name}\n") {
            Ok(())
        } else {
            Err(comm.trim_end().to_owned())
        }
    });
}
