//! The event log: `.branchyard/events/<name>.jsonl`, one [`RecordedEvent`]
//! per line, appended before the observer sees it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};

use crate::state::{now_ms, Store};
use crate::{Activity, BranchEvent, Error, Observer, RecordedEvent};

/// Appends one branch's activity and passes it to the observer.
pub(crate) struct Recorder {
    branch: String,
    file: File,
    observer: Option<Observer>,
}

impl Recorder {
    pub fn open(store: &Store, branch: &str, observer: Option<Observer>) -> Result<Self, Error> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(store.events_path(branch))
            .map_err(|e| Error::State(format!("event log for {branch}: {e}")))?;
        Ok(Recorder {
            branch: branch.to_owned(),
            file,
            observer,
        })
    }

    /// Append `activity`, then call the observer. The line goes out in one
    /// write so concurrent appenders cannot interleave within it.
    pub fn record(&mut self, activity: Activity) -> Result<(), Error> {
        let entry = RecordedEvent {
            at_ms: now_ms(),
            activity,
        };
        let mut line = serde_json::to_vec(&entry)
            .map_err(|e| Error::State(format!("encode event for {}: {e}", self.branch)))?;
        line.push(b'\n');
        self.file
            .write_all(&line)
            .map_err(|e| Error::State(format!("event log for {}: {e}", self.branch)))?;
        if let Some(observer) = &self.observer {
            observer(&BranchEvent {
                branch: self.branch.clone(),
                activity: entry.activity,
            });
        }
        Ok(())
    }
}

/// Every recorded event for `branch`, oldest first. A final line without
/// its newline, left by a crash mid-write, is skipped.
pub(crate) fn read(store: &Store, branch: &str) -> Result<Vec<RecordedEvent>, Error> {
    store.read(branch)?;
    let text = match fs::read_to_string(store.events_path(branch)) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::State(format!("event log for {branch}: {e}"))),
    };
    let complete = match text.rfind('\n') {
        Some(end) => &text[..end],
        None => "",
    };
    complete
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(n, line)| {
            serde_json::from_str(line)
                .map_err(|e| Error::State(format!("event log for {branch}, line {}: {e}", n + 1)))
        })
        .collect()
}
