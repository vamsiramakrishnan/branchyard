use std::any::Any;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::thread::JoinHandle;

/// A thread that panicked: its name and the panic's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanicReport {
    /// The panicking thread's name.
    pub thread: String,
    /// The panic's message, or a note when it was not a string.
    pub message: String,
}

fn message_of(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic with a payload that is not text".to_owned()
    }
}

/// Spawn a named thread that reports its own panic.
///
/// A panic in a plain `std::thread::spawn` surfaces only on stderr and in a
/// `join()` that callers usually discard. Here it is caught, logged at
/// `error`, and handed to `on_panic`, which can write it somewhere the
/// operator looks (the branch's event log). `on_panic` runs on the dying
/// thread; a panic inside it is contained. The thread then ends normally, so
/// `join()` returns `Ok`: do not use this where a caller needs the panic to
/// propagate through `join`.
pub fn spawn_named<F, S>(
    name: impl Into<String>,
    on_panic: S,
    body: F,
) -> io::Result<JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
    S: FnOnce(PanicReport) + Send + 'static,
{
    let name = name.into();
    let thread_name = name.clone();
    std::thread::Builder::new().name(name).spawn(move || {
        if let Err(payload) = catch_unwind(AssertUnwindSafe(body)) {
            let report = PanicReport {
                thread: thread_name,
                message: message_of(payload.as_ref()),
            };
            tracing::error!(
                thread = %report.thread,
                panic = %report.message,
                "thread `{}` panicked: {}",
                report.thread,
                report.message
            );
            crate::best_effort(
                "run the panic handler",
                catch_unwind(AssertUnwindSafe(|| on_panic(report)))
                    .map_err(|payload| message_of(payload.as_ref())),
            );
        }
    })
}

/// Wait for a thread whose result the caller does not need. A panic that
/// ended it is reported with `context` (what the thread did) instead of being
/// dropped with the `Err`; the value of a thread that finished is returned.
pub fn join_reporting<T>(context: &str, handle: JoinHandle<T>) -> Option<T> {
    match handle.join() {
        Ok(value) => Some(value),
        Err(payload) => {
            crate::best_effort::report(
                &format!("join the {context} thread"),
                &format!("it panicked: {}", message_of(payload.as_ref())),
            );
            None
        }
    }
}

/// Run `body`, containing a panic in it: the panic is reported as a survived
/// failure under `context` (what `body` was doing, as for
/// [`crate::best_effort()`]) and `None` is returned. For code the caller does
/// not own and must not be ended by, such as an observer the engine tells of
/// each event: a reader whose output is a closed pipe must not lose the turn
/// it was watching.
pub fn survive_panic<T>(context: &str, body: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(value) => Some(value),
        Err(payload) => {
            crate::best_effort::report(
                context,
                &format!("it panicked: {}", message_of(payload.as_ref())),
            );
            None
        }
    }
}
