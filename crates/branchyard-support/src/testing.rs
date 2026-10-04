//! A tracing subscriber that collects events, for tests that must show a
//! failure was logged rather than lost. Behind the `testing` feature.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
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

struct Collector {
    events: Arc<Mutex<Vec<Captured>>>,
    next: AtomicU64,
}

impl Subscriber for Collector {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut text = Text::default();
        event.record(&mut text);
        self.events
            .lock_recovering("captured events")
            .push(Captured {
                level: *event.metadata().level(),
                target: event.metadata().target().to_owned(),
                text: text.0,
            });
    }
}

/// Run `body` with a subscriber that collects every event the current thread
/// emits, and return them with `body`'s result. Events from other threads are
/// not seen; run the code under test on this thread.
pub fn capture<R>(body: impl FnOnce() -> R) -> (R, Vec<Captured>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector {
        events: events.clone(),
        next: AtomicU64::new(0),
    };
    let result = tracing::subscriber::with_default(collector, body);
    let captured = events.lock_recovering("captured events").clone();
    (result, captured)
}
