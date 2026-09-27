//! A repository's activity feed: every branch's recorded events, numbered
//! in the order the server ingested them, in one append-only file under the
//! server's data directory.
//!
//! The branch event logs stay authoritative. The feed copies them so a
//! stream across branches can resume from one integer cursor. Ingestion
//! reads each log incrementally ([`LogTail`]); the engine's observer wakes
//! it, and a poll catches activity from other processes, such as a local
//! `by run` in the same repository.
//!
//! Each stored line also records where it came from (inode and end offset
//! in its branch log), so a restart resumes ingestion exactly where the
//! durable feed ends: no entry is lost or repeated. A final line torn by a
//! crash is truncated on open.
//!
//! Order within one branch follows its log. Across branches it is the
//! order of ingestion, not of `at_ms`.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use branchyard::RecordedEvent;
use branchyard_client::api::FeedEntry;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::tail::{self, LogTail};

/// A stored line: the public entry and its source position.
#[derive(Serialize, Deserialize)]
struct Stored {
    seq: u64,
    branch: String,
    /// Inode of the branch log (0 when unknown) and the offset just past
    /// the line.
    src: (u64, u64),
    event: RecordedEvent,
}

pub struct Feed {
    root: PathBuf,
    events_dir: PathBuf,
    path: PathBuf,
    inner: Mutex<Inner>,
    head: watch::Sender<u64>,
}

struct Inner {
    file: File,
    /// Byte offset of entry `seq` at index `seq - 1`.
    index: Vec<u64>,
    len: u64,
    tails: HashMap<String, LogTail>,
}

impl Feed {
    /// Open or create the feed at `path` for the repository at `root`.
    pub fn open(path: PathBuf, root: &Path) -> io::Result<Feed> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)?;
        let inner = load(file, root, &path)?;
        let (head, _) = watch::channel(inner.index.len() as u64);
        Ok(Feed {
            root: root.to_path_buf(),
            events_dir: tail::events_dir(root),
            path,
            inner: Mutex::new(inner),
            head,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The sequence number of the last entry; 0 when empty.
    pub fn head(&self) -> u64 {
        *self.head.borrow()
    }

    /// Wakes when the head moves.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.head.subscribe()
    }

    /// Ingest everything the branch logs gained since the last sync, and
    /// return the new head. Blocking; the entries are on disk when it
    /// returns.
    pub fn sync(&self) -> io::Result<u64> {
        let mut inner = self.lock();
        let mut names = Vec::new();
        match fs::read_dir(&self.events_dir) {
            Ok(entries) => {
                for entry in entries {
                    let file = entry?.file_name();
                    if let Some(name) = file.to_str().and_then(|f| f.strip_suffix(".jsonl")) {
                        if !name.starts_with('.') {
                            names.push(name.to_owned());
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        names.sort();
        let mut buffer = Vec::new();
        let mut offsets = Vec::new();
        let mut seq = inner.index.len() as u64;
        for name in names {
            let path = self.events_dir.join(format!("{name}.jsonl"));
            let tail = inner
                .tails
                .entry(name.clone())
                .or_insert_with(|| LogTail::new(path));
            for line in tail.read()? {
                let event = match line.event {
                    Ok(event) => event,
                    Err(error) => {
                        eprintln!(
                            "branchyard-server: skipping an unreadable event of {name} before byte {}: {error}",
                            line.end
                        );
                        continue;
                    }
                };
                seq += 1;
                let stored = Stored {
                    seq,
                    branch: name.clone(),
                    src: (tail.inode().unwrap_or(0), line.end),
                    event,
                };
                offsets.push(buffer.len() as u64);
                serde_json::to_writer(&mut buffer, &stored).map_err(io::Error::other)?;
                buffer.push(b'\n');
            }
        }
        if buffer.is_empty() {
            return Ok(inner.index.len() as u64);
        }
        let start = inner.len;
        if let Err(error) = inner
            .file
            .write_all(&buffer)
            .and_then(|_| inner.file.sync_data())
        {
            // Leave no partial batch behind. The tails already moved past
            // it, so reload them from what is durable.
            let _ = inner.file.set_len(start);
            if let Ok(file) = inner.file.try_clone() {
                if let Ok(fresh) = load(file, &self.root, &self.path) {
                    *inner = fresh;
                }
            }
            return Err(error);
        }
        inner.index.extend(offsets.into_iter().map(|o| start + o));
        inner.len = start + buffer.len() as u64;
        let head = inner.index.len() as u64;
        self.head.send_replace(head);
        Ok(head)
    }

    /// Stop following `branch`'s log, after its removal. A new branch with
    /// the same name is read from the start of its own log.
    pub fn forget(&self, branch: &str) {
        self.lock().tails.remove(branch);
    }

    /// Up to `limit` entries after `cursor`.
    pub fn read_after(&self, cursor: u64, limit: usize) -> io::Result<Vec<FeedEntry>> {
        let (start, end) = {
            let inner = self.lock();
            let count = inner.index.len() as u64;
            if cursor >= count || limit == 0 {
                return Ok(Vec::new());
            }
            let last = (cursor + limit as u64).min(count);
            let start = inner.index[cursor as usize];
            let end = match inner.index.get(last as usize) {
                Some(offset) => *offset,
                None => inner.len,
            };
            (start, end)
        };
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(end - start).read_to_end(&mut bytes)?;
        bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let stored: Stored = serde_json::from_slice(line).map_err(io::Error::other)?;
                Ok(FeedEntry {
                    seq: stored.seq,
                    branch: stored.branch,
                    at_ms: stored.event.at_ms,
                    activity: stored.event.activity,
                })
            })
            .collect()
    }
}

/// Index a feed file and recover each branch's position, truncating a
/// torn or unreadable tail.
fn load(mut file: File, root: &Path, path: &Path) -> io::Result<Inner> {
    let mut index = Vec::new();
    let mut tails = HashMap::new();
    let mut good = 0u64;
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(&file);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        let Ok(stored) = serde_json::from_slice::<Stored>(&line) else {
            break;
        };
        if stored.seq != index.len() as u64 + 1 {
            break;
        }
        index.push(good);
        good += n as u64;
        let (inode, end) = stored.src;
        let inode = (inode != 0).then_some(inode);
        let log = tail::log_path(root, &stored.branch);
        tails.insert(stored.branch, LogTail::at(log, inode, end));
    }
    drop(reader);
    let len = file.metadata()?.len();
    if len != good {
        eprintln!(
            "branchyard-server: truncating {} from {len} to {good} bytes after an incomplete entry",
            path.display()
        );
        file.set_len(good)?;
        file.sync_all()?;
    }
    Ok(Inner {
        file,
        index,
        len: good,
        tails,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::{Activity, BranchStatus};

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("branchyard-feed-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo/.branchyard/events")).unwrap();
        dir
    }

    fn append(dir: &Path, branch: &str, at_ms: u64) {
        let event = RecordedEvent {
            at_ms,
            activity: Activity::Status(BranchStatus::Running),
        };
        let path = tail::log_path(&dir.join("repo"), branch);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&event).unwrap()).unwrap();
    }

    fn summary(entries: &[FeedEntry]) -> Vec<(u64, String, u64)> {
        entries
            .iter()
            .map(|e| (e.seq, e.branch.clone(), e.at_ms))
            .collect()
    }

    #[test]
    fn numbers_entries_and_resumes_after_reopening() {
        let dir = temp("resume");
        let root = dir.join("repo");
        let path = dir.join("data/feeds/r.jsonl");
        let feed = Feed::open(path.clone(), &root).unwrap();
        assert_eq!(feed.sync().unwrap(), 0);
        append(&dir, "a", 1);
        append(&dir, "b", 2);
        append(&dir, "a", 3);
        assert_eq!(feed.sync().unwrap(), 3);
        assert_eq!(
            summary(&feed.read_after(0, 10).unwrap()),
            [(1, "a".into(), 1), (2, "a".into(), 3), (3, "b".into(), 2)]
        );
        assert_eq!(
            summary(&feed.read_after(1, 1).unwrap()),
            [(2, "a".into(), 3)]
        );
        assert!(feed.read_after(3, 10).unwrap().is_empty());
        drop(feed);

        append(&dir, "b", 4);
        // A torn final line from a crash is dropped on open.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{{\"seq\":4,").unwrap();
        drop(file);
        let feed = Feed::open(path.clone(), &root).unwrap();
        assert_eq!(feed.head(), 3);
        assert_eq!(feed.sync().unwrap(), 4);
        assert_eq!(
            summary(&feed.read_after(3, 10).unwrap()),
            [(4, "b".into(), 4)]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_forgotten_branch_is_reread_when_recreated() {
        let dir = temp("forget");
        let root = dir.join("repo");
        let feed = Feed::open(dir.join("feed.jsonl"), &root).unwrap();
        append(&dir, "a", 1);
        append(&dir, "a", 2);
        feed.sync().unwrap();
        fs::remove_file(tail::log_path(&root, "a")).unwrap();
        feed.forget("a");
        append(&dir, "a", 9);
        assert_eq!(feed.sync().unwrap(), 3);
        assert_eq!(
            summary(&feed.read_after(2, 10).unwrap()),
            [(3, "a".into(), 9)]
        );
        let _ = fs::remove_dir_all(dir);
    }
}
