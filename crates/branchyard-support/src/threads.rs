use std::any::Any;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::thread::JoinHandle;

/// A thread that panicked: its name and the panic's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanicReport {
    pub thread: String,
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
            let _ = catch_unwind(AssertUnwindSafe(|| on_panic(report)));
        }
    })
}
