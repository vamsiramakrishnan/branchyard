//! `.branchyard/`: one JSON record per branch, written atomically, and the
//! directories the engine owns.
//!
//! A branch name is reserved by creating its record file exclusively; an
//! empty record file is a reservation whose branch is still being set up.
//!
//! A record's `children` belong to [`Store::add_child`]: every other write
//! keeps the list on disk, so a turn that ends after it spawned children
//! cannot drop them. This holds within one process; the record lock is not
//! shared across processes.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::delegation::Grant;
use crate::{BranchInfo, Error};

pub(crate) const DIR: &str = ".branchyard";

/// A branch's record: its public info and what the engine needs to continue
/// it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Record {
    pub info: BranchInfo,
    /// Milliseconds since the Unix epoch, for ordering.
    pub created_ms: u64,
    pub check: Option<Vec<String>>,
    /// Command override, reused by sends and forks.
    pub command: Option<Vec<String>>,
    /// Private `HOME` when the branch runs isolated.
    pub home: Option<PathBuf>,
    /// For a forked session, the parent's cumulative cost at the fork,
    /// which the harness keeps reporting as part of the fork's.
    pub cost_baseline: Option<f64>,
    /// What the branch may delegate, and for a delegated child the limits
    /// and denials its parent imposed. `None`: no delegation.
    #[serde(default)]
    pub grant: Option<Grant>,
}

pub(crate) struct Store {
    dir: PathBuf,
}

static TEMP: AtomicU64 = AtomicU64::new(0);

/// Serializes this process's record writes, so [`Store::add_child`] and a
/// turn's own writes cannot lose each other's changes.
static RECORDS: Mutex<()> = Mutex::new(());

fn records_lock() -> MutexGuard<'static, ()> {
    RECORDS.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Store {
    pub fn new(root: &Path) -> Store {
        Store {
            dir: root.join(DIR),
        }
    }

    /// The `.branchyard` directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn create_dirs(&self) -> io::Result<()> {
        for sub in ["branches", "events", "worktrees"] {
            fs::create_dir_all(self.dir.join(sub))?;
        }
        Ok(())
    }

    fn record_path(&self, name: &str) -> PathBuf {
        self.dir.join("branches").join(format!("{name}.json"))
    }

    pub fn events_path(&self, name: &str) -> PathBuf {
        self.dir.join("events").join(format!("{name}.jsonl"))
    }

    pub fn worktree(&self, name: &str) -> PathBuf {
        self.dir.join("worktrees").join(name)
    }

    pub fn home(&self, name: &str) -> PathBuf {
        self.dir.join("homes").join(name)
    }

    /// Whether a record or reservation exists for `name`.
    pub fn taken(&self, name: &str) -> bool {
        self.record_path(name).exists()
    }

    /// Reserve `name`; false if it is already taken.
    pub fn reserve(&self, name: &str) -> Result<bool, Error> {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.record_path(name))
        {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(state_io("reserve", name, e)),
        }
    }

    /// Give up a reservation made by [`Store::reserve`].
    pub fn release(&self, name: &str) {
        let _ = fs::remove_file(self.record_path(name));
    }

    /// Replace the record through a temporary file and a rename, so readers
    /// see the old record or the new one, never part of either. The
    /// `children` already on disk are kept.
    pub fn write(&self, record: &Record) -> Result<(), Error> {
        let _lock = records_lock();
        match self.read(&record.info.name) {
            Ok(current) if current.info.children != record.info.children => {
                let mut record = record.clone();
                record.info.children = current.info.children;
                self.replace(&record)
            }
            _ => self.replace(record),
        }
    }

    /// Append `child` to `parent`'s children.
    pub fn add_child(&self, parent: &str, child: &str) -> Result<(), Error> {
        let _lock = records_lock();
        let mut record = self.read(parent)?;
        if !record.info.children.iter().any(|c| c == child) {
            record.info.children.push(child.to_owned());
        }
        self.replace(&record)
    }

    fn replace(&self, record: &Record) -> Result<(), Error> {
        let name = &record.info.name;
        let path = self.record_path(name);
        let temp = self.dir.join("branches").join(format!(
            ".{name}.{}-{}.tmp",
            std::process::id(),
            TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let text = serde_json::to_vec_pretty(record)
            .map_err(|e| Error::State(format!("encode {name}: {e}")))?;
        let written = (|| {
            let mut file = fs::File::create(&temp)?;
            file.write_all(&text)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temp, &path)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&temp);
            return Err(state_io("write", name, e));
        }
        Ok(())
    }

    pub fn read(&self, name: &str) -> Result<Record, Error> {
        if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
            return Err(Error::UnknownBranch(name.to_owned()));
        }
        let text = match fs::read(self.record_path(name)) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::UnknownBranch(name.to_owned()))
            }
            Err(e) => return Err(state_io("read", name, e)),
        };
        if text.is_empty() {
            // Reserved, not yet created.
            return Err(Error::UnknownBranch(name.to_owned()));
        }
        serde_json::from_slice(&text).map_err(|e| Error::State(format!("record {name}: {e}")))
    }

    /// Every created branch, oldest first.
    pub fn list(&self) -> Result<Vec<Record>, Error> {
        let entries = match fs::read_dir(self.dir.join("branches")) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(state_io("list", "branches", e)),
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| state_io("list", "branches", e))?;
            let file = entry.file_name();
            let Some(name) = file.to_str().and_then(|f| f.strip_suffix(".json")) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            match self.read(name) {
                Ok(record) => records.push(record),
                Err(Error::UnknownBranch(_)) => {}
                Err(e) => return Err(e),
            }
        }
        records.sort_by(|a, b| (a.created_ms, &a.info.name).cmp(&(b.created_ms, &b.info.name)));
        Ok(records)
    }

    /// Where the engine running `name` writes its delegation token and the
    /// address of its broker, while a turn runs.
    pub fn token_path(&self, name: &str) -> PathBuf {
        self.dir.join("delegation").join(format!("{name}.json"))
    }

    fn cancel_path(&self, name: &str) -> PathBuf {
        self.dir.join("delegation").join(format!("{name}.cancel"))
    }

    /// Ask the engine running `name` to stop its turn; `by` names who asked.
    pub fn request_cancel(&self, name: &str, by: &str) -> Result<(), Error> {
        let path = self.cancel_path(name);
        fs::create_dir_all(self.dir.join("delegation"))
            .and_then(|()| fs::write(&path, by))
            .map_err(|e| state_io("cancel", name, e))
    }

    /// Who asked to cancel `name`'s turn, if anyone did.
    pub fn cancel_requested(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.cancel_path(name)).ok()
    }

    pub fn clear_cancel(&self, name: &str) {
        let _ = fs::remove_file(self.cancel_path(name));
    }

    /// Delete a branch's record and event log.
    pub fn delete(&self, name: &str) -> Result<(), Error> {
        self.clear_cancel(name);
        for path in [self.events_path(name), self.record_path(name)] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(state_io("delete", name, e)),
            }
        }
        Ok(())
    }
}

fn state_io(what: &str, name: &str, error: io::Error) -> Error {
    Error::State(format!("{what} {name}: {error}"))
}
