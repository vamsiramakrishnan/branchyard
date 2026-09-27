//! The event log: each branch's [`RecordedEvent`]s in the store, numbered
//! from 1 in the branch and by feed position across the repository,
//! appended before the observer sees them.

use std::time::Duration;

use crate::state::{now_ms, Fence, Lease, Record, Store};
use crate::{Activity, BranchEvent, Error, FeedEvent, FeedPage, Observer, Page, RecordedEvent};

/// Most events one page returns.
pub(crate) const PAGE_MAX: usize = 10_000;

/// Appends one branch's activity and passes it to the observer.
pub(crate) struct Recorder {
    store: Store,
    branch: String,
    /// The turn's lease; appends fail once it is lost.
    fence: Option<Fence>,
    observer: Option<Observer>,
}

impl Recorder {
    /// A recorder outside any turn, such as a merge or a delegation note.
    pub fn open(store: &Store, branch: &str, observer: Option<Observer>) -> Result<Self, Error> {
        Ok(Recorder {
            store: store.clone(),
            branch: branch.to_owned(),
            fence: None,
            observer,
        })
    }

    /// A recorder for the fenced turn.
    pub fn fenced(store: &Store, fence: &Fence, observer: Option<Observer>) -> Self {
        Recorder {
            store: store.clone(),
            branch: fence.branch.clone(),
            fence: Some(fence.clone()),
            observer,
        }
    }

    /// Append `activity`, then call the observer.
    pub fn record(&mut self, activity: Activity) -> Result<(), Error> {
        let entry = RecordedEvent {
            at_ms: now_ms(),
            activity,
        };
        self.store
            .append(&self.branch, &entry, self.fence.as_ref())?;
        if let Some(observer) = &self.observer {
            observer(&BranchEvent {
                branch: self.branch.clone(),
                activity: entry.activity,
            });
        }
        Ok(())
    }
}

impl Recorder {
    /// Write the turn's final record and its status event and release the
    /// lease, atomically; then call the observer.
    pub fn finish(&mut self, lease: Lease, record: &Record) -> Result<(), Error> {
        let entry = RecordedEvent {
            at_ms: now_ms(),
            activity: Activity::Status(record.info.status.clone()),
        };
        lease.finish(Some(record), Some(&entry))?;
        if let Some(observer) = &self.observer {
            observer(&BranchEvent {
                branch: self.branch.clone(),
                activity: entry.activity,
            });
        }
        Ok(())
    }
}

/// Every recorded event for `branch`, oldest first.
pub(crate) fn read(store: &Store, branch: &str) -> Result<Vec<RecordedEvent>, Error> {
    store.read(branch)?;
    let mut events = Vec::new();
    let mut cursor = 0;
    loop {
        let page = store.backend().events_since(branch, cursor, PAGE_MAX)?;
        let Some((last, _)) = page.last() else {
            return Ok(events);
        };
        cursor = *last;
        events.extend(page.into_iter().map(|(_, event)| event));
    }
}

/// Up to `limit` events of `branch` after `cursor`.
pub(crate) fn since(store: &Store, branch: &str, cursor: u64, limit: usize) -> Result<Page, Error> {
    let limit = limit.clamp(1, PAGE_MAX);
    let page = store.backend().events_since(branch, cursor, limit)?;
    let next_cursor = page.last().map_or(cursor, |(seq, _)| *seq);
    Ok(Page {
        events: page.into_iter().map(|(_, event)| event).collect(),
        next_cursor,
    })
}

/// [`since`], waiting up to `timeout` for the first event.
pub(crate) fn wait(
    store: &Store,
    branch: &str,
    cursor: u64,
    limit: usize,
    timeout: Duration,
) -> Result<Page, Error> {
    let found = store.wait(timeout, || {
        let page = since(store, branch, cursor, limit)?;
        Ok((!page.events.is_empty()).then_some(page))
    })?;
    Ok(found.unwrap_or(Page {
        events: Vec::new(),
        next_cursor: cursor,
    }))
}

/// Up to `limit` events of every branch after feed position `cursor`.
pub(crate) fn feed(store: &Store, cursor: u64, limit: usize) -> Result<FeedPage, Error> {
    let limit = limit.clamp(1, PAGE_MAX);
    let rows = store.backend().feed_since(cursor, limit)?;
    let next_cursor = rows.last().map_or(cursor, |row| row.id);
    Ok(FeedPage {
        events: rows
            .into_iter()
            .map(|row| FeedEvent {
                position: row.id,
                branch: row.branch,
                event: row.event,
            })
            .collect(),
        next_cursor,
    })
}

/// [`feed`], waiting up to `timeout` for the first event.
pub(crate) fn wait_feed(
    store: &Store,
    cursor: u64,
    limit: usize,
    timeout: Duration,
) -> Result<FeedPage, Error> {
    let found = store.wait(timeout, || {
        let page = feed(store, cursor, limit)?;
        Ok((!page.events.is_empty()).then_some(page))
    })?;
    Ok(found.unwrap_or(FeedPage {
        events: Vec::new(),
        next_cursor: cursor,
    }))
}
