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

/// Where survived failures go besides the log: a server installs one that
/// writes them to an event log. It runs on whatever thread hit the failure,
/// possibly inside a `Drop`, so it must be quick and must not block.
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

#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-support
pub(crate) fn report(context: &str, error: &dyn Display) {
    let error = error.to_string();
    tracing::warn!(
        context = %context,
        error = %error,
        "best-effort step failed: {context}: {error}"
    );
    let sink = SINK.lock_recovering("failure sink").clone();
    if let Some(sink) = sink {
        let failure = Failure {
            context: context.to_owned(),
            error,
        };
        // A sink that panics must not take a Drop path down with it.
        let _ = catch_unwind(AssertUnwindSafe(|| sink.record(&failure)));
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
