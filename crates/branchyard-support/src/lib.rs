//! Failure handling every Branchyard crate shares.
//!
//! Some failures are acceptable (a temp directory that cannot be removed, a
//! process that already exited, a lock whose holder panicked) but none is
//! acceptable *unseen*. This crate is the one sanctioned way to say "this may
//! fail, carry on, but say so":
//!
//! - [`best_effort`] and [`best_effort!`] run a fallible step whose failure
//!   the caller survives. The failure is logged at `warn` through `tracing`
//!   with a context string naming the step, and handed to the installed
//!   [`set_failure_sink`], so a server can put it in an event log.
//! - [`cleanup_dir`], [`cleanup_file`], [`kill_process`] and [`kill_group`]
//!   are the repeated cleanup steps built on it.
//! - [`LockExt`], [`RwLockExt`] and [`CondvarExt`] take a lock whose holder
//!   panicked, log the poisoning with the lock's name, and go on.
//! - [`spawn_named`] runs a thread that reports a panic through a sink the
//!   caller supplies instead of leaving it on stderr.
//!
//! Nothing here panics, so all of it is safe on a `Drop` path. See
//! CONTRIBUTING.md ("Failures that may be ignored").

mod best_effort;
mod locks;
mod process;
mod threads;

#[cfg(feature = "testing")]
pub mod testing;

pub use best_effort::{
    best_effort, cleanup_dir, cleanup_file, set_failure_sink, Failure, FailureSink,
};
pub use locks::{CondvarExt, LockExt, RwLockExt};
pub use process::{kill_group, kill_process, terminate_group};
pub use threads::{spawn_named, PanicReport};

/// Run a fallible step that may fail without failing the caller, logging the
/// failure under `$context` and yielding `Some(value)` on success.
///
/// `best_effort!("remove the staging dir", std::fs::remove_dir_all(&dir));`
#[macro_export]
macro_rules! best_effort {
    ($context:expr, $result:expr $(,)?) => {
        $crate::best_effort($context, $result)
    };
}
