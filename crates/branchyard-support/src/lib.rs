//! Failure handling, time and randomness every Branchyard crate shares.
//!
//! Some failures are acceptable (a temp directory that cannot be removed, a
//! process that already exited, a lock whose holder panicked) but none is
//! acceptable *unseen*. This crate is the one sanctioned way to say "this may
//! fail, carry on, but say so":
//!
//! - [`best_effort()`] and [`best_effort!`] run a fallible step whose failure
//!   the caller survives. The failure is logged at `warn` through `tracing`
//!   with a context string naming the step, and handed to the installed
//!   [`set_failure_sink`], so a process can put it in an event log.
//! - [`best_effort_once()`] is the same for a step a loop repeats: a
//!   persistent failure is logged once, and its recovery is logged too.
//! - [`cleanup_dir`], [`cleanup_file`], [`kill_process`] and [`kill_group`]
//!   are the repeated cleanup steps built on it.
//! - [`LockExt`], [`RwLockExt`] and [`CondvarExt`] take a lock whose holder
//!   panicked, log the poisoning with the lock's name, and go on.
//! - [`spawn_named`] runs a thread that reports a panic through a sink the
//!   caller supplies instead of leaving it on stderr, and [`join_reporting`]
//!   waits for one without dropping a panic that ended it.
//!
//! - [`time`] is the one clock and the one set of date formats (on `jiff`),
//!   [`rng`] the one seedable generator and the system entropy (on
//!   `getrandom`), and [`new_ulid`] mints ids from both.
//!
//! Nothing here panics, so all of it is safe on a `Drop` path. See
//! CONTRIBUTING.md ("Failures that may be ignored").
#![warn(missing_docs)]

mod best_effort;
mod id;
mod locks;
mod process;
pub mod rng;
mod threads;
pub mod time;

#[cfg(feature = "testing")]
pub mod testing;

pub use best_effort::{
    best_effort, best_effort_once, cleanup_dir, cleanup_file, set_failure_sink, Failure,
    FailureSink,
};
pub use id::{new_ulid, ulid_from_parts};
pub use locks::{CondvarExt, LockExt, RwLockExt};
pub use process::{kill_group, kill_process, terminate_group};
pub use threads::{join_reporting, spawn_named, PanicReport};

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
