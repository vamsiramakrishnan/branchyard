use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt::Display;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::locks::LockExt as _;

/// A failure that was survived: what was being done and why it failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// What the step was doing, in the caller's words.
    pub context: String,
    /// The error, rendered.
    pub error: String,
}

/// Where survived failures go besides the log: a process may install one that
/// writes them to an event log; none is installed by default. It runs on whatever thread hit the failure,
/// possibly inside a `Drop`, so it must be quick and must not block. To
/// gather the failures of one piece of work instead, see
/// [`collect_failures`].
pub trait FailureSink: Send + Sync {
    /// Take note of one survived failure.
    fn record(&self, failure: &Failure);
}

impl<F: Fn(&Failure) + Send + Sync> FailureSink for F {
    fn record(&self, failure: &Failure) {
        self(failure)
    }
}

static SINK: Mutex<Option<Arc<dyn FailureSink>>> = Mutex::new(None);

/// Install the process-wide sink for survived failures, replacing any other.
/// The log line is written whether or not a sink is installed.
pub fn set_failure_sink(sink: Arc<dyn FailureSink>) {
    *SINK.lock_recovering("failure sink") = Some(sink);
}

thread_local! {
    /// The failures survived on this thread inside each running
    /// [`collect_failures`], innermost last.
    static SCOPES: RefCell<Vec<Vec<Failure>>> = const { RefCell::new(Vec::new()) };
}

/// Hand `failure` to the innermost [`collect_failures`] on this thread, if
/// one is running. Never panics: a thread that is shutting down, or a
/// failure reported while the scopes are being changed, is passed over.
fn note_in_scope(failure: &Failure) {
    let noted = SCOPES.try_with(|scopes| {
        if let Ok(mut scopes) = scopes.try_borrow_mut() {
            if let Some(scope) = scopes.last_mut() {
                scope.push(failure.clone());
            }
        }
    });
    if noted.is_err() {
        tracing::debug!("a survived failure was not collected: the thread is shutting down");
    }
}

/// Closes a [`collect_failures`] scope, however its body ends.
struct Scope {
    open: bool,
}

impl Scope {
    fn enter() -> Scope {
        let open = SCOPES
            .try_with(|scopes| match scopes.try_borrow_mut() {
                Ok(mut scopes) => {
                    scopes.push(Vec::new());
                    true
                }
                Err(_) => false,
            })
            .unwrap_or(false);
        Scope { open }
    }

    /// The failures survived in the scope, which stays visible to the scope
    /// around it.
    fn close(&mut self) -> Vec<Failure> {
        if !std::mem::take(&mut self.open) {
            return Vec::new();
        }
        SCOPES
            .try_with(|scopes| {
                let Ok(mut scopes) = scopes.try_borrow_mut() else {
                    return Vec::new();
                };
                let failures = scopes.pop().unwrap_or_default();
                if let Some(outer) = scopes.last_mut() {
                    outer.extend(failures.iter().cloned());
                }
                failures
            })
            .unwrap_or_default()
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        // An unwinding body: the scope still has to come off the stack.
        self.close();
    }
}

/// Run `body` and return its value with the failures survived on this thread
/// while it ran: every [`best_effort()`], [`best_effort_once()`] (its first
/// failure of a step), [`cleanup_dir`] and [`cleanup_file`] that failed.
/// They are logged and handed to the failure sink as usual; this is for a
/// caller that wants to put what it survived somewhere its readers look, as
/// the delegation engine does with a turn's (the branch's event log).
/// Failures on other threads are not collected, and a scope inside another
/// also reports its failures to the outer one. Never panics.
pub fn collect_failures<T>(body: impl FnOnce() -> T) -> (T, Vec<Failure>) {
    let mut scope = Scope::enter();
    let value = body();
    let failures = scope.close();
    (value, failures)
}

pub(crate) fn report(context: &str, error: &dyn Display) {
    let error = error.to_string();
    tracing::warn!(
        context = %context,
        error = %error,
        "best-effort step failed: {context}: {error}"
    );
    let failure = Failure {
        context: context.to_owned(),
        error,
    };
    note_in_scope(&failure);
    let sink = SINK.lock_recovering("failure sink").clone();
    if let Some(sink) = sink {
        // A sink that panics must not take a Drop path down with it.
        if catch_unwind(AssertUnwindSafe(|| sink.record(&failure))).is_err() {
            tracing::warn!("the failure sink panicked; this failure was logged but not recorded");
        }
    }
}

/// Take the value of a step whose failure the caller survives. `Err` is
/// logged at `warn` with `context` and the error, handed to the failure
/// sink, and becomes `None`. Never panics.
pub fn best_effort<T, E: Display>(context: &str, result: Result<T, E>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            report(context, &error);
            None
        }
    }
}

static FAILING: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// [`best_effort()`] for a step a loop repeats: a persistent failure is
/// logged on its first occurrence only, and a success after failures logs
/// that the step recovered. Failures are told apart by `context`, so put
/// anything that tells two steps apart (a branch, a path) in it. Everything
/// else is as [`best_effort()`]; the failure sink hears the first failure
/// only.
pub fn best_effort_once<T, E: Display>(context: &str, result: Result<T, E>) -> Option<T> {
    match result {
        Ok(value) => {
            let was_failing = FAILING
                .lock_recovering("failing steps")
                .as_mut()
                .is_some_and(|failing| failing.remove(context));
            if was_failing {
                tracing::info!(context = %context, "best-effort step recovered: {context}");
            }
            Some(value)
        }
        Err(error) => {
            let first = FAILING
                .lock_recovering("failing steps")
                .get_or_insert_with(HashSet::new)
                .insert(context.to_owned());
            if first {
                report(context, &error);
            }
            None
        }
    }
}

/// Remove a directory tree. One already gone is success; any other failure
/// is reported with the path.
pub fn cleanup_dir(path: impl AsRef<Path>) {
    let path = path.as_ref();
    if let Err(e) = std::fs::remove_dir_all(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            report(&format!("remove directory {}", path.display()), &e);
        }
    }
}

/// Remove a file. One already gone is success; any other failure is
/// reported with the path.
pub fn cleanup_file(path: impl AsRef<Path>) {
    let path = path.as_ref();
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            report(&format!("remove file {}", path.display()), &e);
        }
    }
}
