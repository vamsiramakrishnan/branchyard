//! A tracing subscriber that collects events, for tests that must show a
//! failure was logged rather than lost. Behind the `testing` feature.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};

pub use tracing::Level;

use crate::LockExt as _;

/// One event a test subscriber saw.
#[derive(Clone, Debug)]
pub struct Captured {
    /// The event's level.
    pub level: Level,
    /// The event's target (its module path unless set).
    pub target: String,
    /// The event's fields, as `name=value` pairs joined by spaces, with the
    /// message first.
    pub text: String,
}

#[derive(Default)]
struct Text(String);

impl Visit for Text {
    #[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-support
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, "{}={value:?}", field.name());
        }
    }
}

/// Events go to the innermost capture running on their own thread, if any.
/// One instance is the process's global subscriber, so every callsite's
/// cached interest is "sometimes" and each event asks [`Subscriber::enabled`].
/// A scoped subscriber is not enough: tracing caches a callsite's interest
/// for the whole process when it is first reached, and a callsite another
/// thread reached first, with no subscriber, while only this thread's one
/// existed, stayed disabled for every later capture.
struct Router {
    next: AtomicU64,
}

thread_local! {
    /// The events of the innermost capture running on this thread.
    static CAPTURING: RefCell<Option<Arc<Mutex<Vec<Captured>>>>> = const { RefCell::new(None) };
}

fn capturing() -> Option<Arc<Mutex<Vec<Captured>>>> {
    CAPTURING
        .try_with(|current| current.try_borrow().ok().and_then(|c| c.clone()))
        .ok()
        .flatten()
}

impl Subscriber for Router {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        capturing().is_some()
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let Some(events) = capturing() else {
            return;
        };
        let mut text = Text::default();
        event.record(&mut text);
        events.lock_recovering("captured events").push(Captured {
            level: *event.metadata().level(),
            target: event.metadata().target().to_owned(),
            text: text.0,
        });
    }
}

/// Puts back the capture a nested one replaced, however its body ends.
struct Restore(Option<Arc<Mutex<Vec<Captured>>>>);

impl Drop for Restore {
    fn drop(&mut self) {
        let previous = self.0.take();
        CAPTURING.with(|current| *current.borrow_mut() = previous);
    }
}

/// Run `body` with a subscriber that collects every event the current thread
/// emits, and return them with `body`'s result. Events from other threads are
/// not seen; run the code under test on this thread.
pub fn capture<R>(body: impl FnOnce() -> R) -> (R, Vec<Captured>) {
    static GLOBAL: OnceLock<bool> = OnceLock::new();
    let global = *GLOBAL.get_or_init(|| {
        tracing::subscriber::set_global_default(Router {
            next: AtomicU64::new(0),
        })
        .is_ok()
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let _restore = Restore(CAPTURING.with(|current| current.replace(Some(events.clone()))));
    let result = match global {
        true => body(),
        // Another global subscriber was set first: route this thread's
        // events here as well as can be.
        false => tracing::subscriber::with_default(
            Router {
                next: AtomicU64::new(0),
            },
            || {
                tracing::callsite::rebuild_interest_cache();
                body()
            },
        ),
    };
    let captured = events.lock_recovering("captured events").clone();
    (result, captured)
}
