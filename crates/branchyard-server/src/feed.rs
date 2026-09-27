//! A repository's activity feed: every branch's recorded events, numbered
//! by the engine's store in the order they were recorded (see
//! [`Yard::events_since`]). The server keeps no copy; it reads the store
//! from a cursor and tracks the head so streams wake when it moves.
//!
//! The head moves when the engine in this process records activity (the
//! observer wakes the poller at once) and, for other processes such as a
//! local `by run` in the same repository, when the poller next looks.

use branchyard::{Error, Yard};
use branchyard_client::api::FeedEntry;
use tokio::sync::watch;

pub struct Feed {
    yard: Yard,
    head: watch::Sender<u64>,
}

impl Feed {
    /// The feed of `yard`'s store, at its current head.
    pub fn open(yard: Yard) -> Result<Feed, Error> {
        let (head, _) = watch::channel(yard.events_head()?);
        Ok(Feed { yard, head })
    }

    /// The position of the last event; 0 when empty.
    pub fn head(&self) -> u64 {
        *self.head.borrow()
    }

    /// Wakes when the head moves.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.head.subscribe()
    }

    /// Read the head from the store and publish it. Blocking.
    pub fn sync(&self) -> Result<u64, Error> {
        let head = self.yard.events_head()?;
        self.head.send_if_modified(|current| {
            let moved = *current != head;
            *current = head;
            moved
        });
        Ok(head)
    }

    /// Up to `limit` entries after `cursor`. Blocking.
    pub fn read_after(&self, cursor: u64, limit: usize) -> Result<Vec<FeedEntry>, Error> {
        Ok(self
            .yard
            .events_since(cursor, limit)?
            .events
            .into_iter()
            .map(|e| FeedEntry {
                seq: e.position,
                branch: e.branch,
                at_ms: e.event.at_ms,
                activity: e.event.activity,
            })
            .collect())
    }
}
