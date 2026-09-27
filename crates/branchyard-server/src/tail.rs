//! Incremental reading of one branch's event log,
//! `.branchyard/events/<name>.jsonl`, which the SDK documents as append-only
//! with one [`RecordedEvent`] per line.
//!
//! Reads resume at a byte offset and stop at the last complete line, so a
//! line the engine is still writing is read next time. A log that was
//! replaced (its branch removed and a new one created under the same name)
//! is detected by its inode, or by having shrunk, and read from the start.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use branchyard::RecordedEvent;

/// Most bytes read per call, so one huge log cannot take all memory.
const MAX_READ: u64 = 8 * 1024 * 1024;

/// The event log directory of the repository at `root`.
pub fn events_dir(root: &Path) -> PathBuf {
    root.join(".branchyard").join("events")
}

/// The event log of `branch` in the repository at `root`.
pub fn log_path(root: &Path, branch: &str) -> PathBuf {
    events_dir(root).join(format!("{branch}.jsonl"))
}

/// A position in one event log.
#[derive(Clone, Debug)]
pub struct LogTail {
    path: PathBuf,
    offset: u64,
    inode: Option<u64>,
}

/// One complete line read from the log.
#[derive(Debug)]
pub struct Line {
    /// Byte offset just past this line.
    pub end: u64,
    pub event: Result<RecordedEvent, String>,
}

impl LogTail {
    /// From the start of the log at `path`.
    pub fn new(path: PathBuf) -> LogTail {
        LogTail {
            path,
            offset: 0,
            inode: None,
        }
    }

    /// Resume at `offset` in the file with `inode`, as recorded earlier.
    pub fn at(path: PathBuf, inode: Option<u64>, offset: u64) -> LogTail {
        LogTail {
            path,
            offset,
            inode,
        }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn inode(&self) -> Option<u64> {
        self.inode
    }

    /// Lines appended since the last read. A missing log yields nothing.
    pub fn read(&mut self) -> io::Result<Vec<Line>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        let inode = inode(&meta);
        if inode != self.inode || meta.len() < self.offset {
            self.inode = inode;
            self.offset = 0;
        }
        if meta.len() == self.offset {
            return Ok(Vec::new());
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.take(MAX_READ).read_to_end(&mut bytes)?;
        let Some(last) = bytes.iter().rposition(|b| *b == b'\n') else {
            return Ok(Vec::new());
        };
        let mut lines = Vec::new();
        let mut start = 0;
        for end in bytes[..=last]
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'\n')
            .map(|(i, _)| i)
        {
            let text = &bytes[start..end];
            let offset = self.offset + end as u64 + 1;
            start = end + 1;
            if text.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            lines.push(Line {
                end: offset,
                event: serde_json::from_slice(text).map_err(|e| e.to_string()),
            });
        }
        self.offset += last as u64 + 1;
        Ok(lines)
    }
}

#[cfg(unix)]
fn inode(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(not(unix))]
fn inode(_: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::{Activity, BranchStatus};
    use std::io::Write;

    fn line(at_ms: u64) -> String {
        let event = RecordedEvent {
            at_ms,
            activity: Activity::Status(BranchStatus::Running),
        };
        format!("{}\n", serde_json::to_string(&event).unwrap())
    }

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("branchyard-tail-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ats(lines: &[Line]) -> Vec<u64> {
        lines
            .iter()
            .map(|l| l.event.as_ref().unwrap().at_ms)
            .collect()
    }

    #[test]
    fn reads_only_complete_new_lines() {
        let dir = temp("complete");
        let path = dir.join("b.jsonl");
        let mut tail = LogTail::new(path.clone());
        assert!(tail.read().unwrap().is_empty(), "missing log");
        let mut file = File::create(&path).unwrap();
        let second = line(2);
        write!(file, "{}{}", line(1), &second[..10]).unwrap();
        assert_eq!(ats(&tail.read().unwrap()), [1]);
        write!(file, "{}\n{}", &second[10..], line(3)).unwrap();
        let lines = tail.read().unwrap();
        assert_eq!(ats(&lines), [2, 3]);
        assert_eq!(lines[1].end, std::fs::metadata(&path).unwrap().len());
        assert!(tail.read().unwrap().is_empty());
        writeln!(file, "not json").unwrap();
        assert!(tail.read().unwrap()[0].event.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_replaced_log_is_read_from_the_start() {
        let dir = temp("replaced");
        let path = dir.join("b.jsonl");
        std::fs::write(&path, line(1) + &line(2)).unwrap();
        let mut tail = LogTail::new(path.clone());
        assert_eq!(tail.read().unwrap().len(), 2);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, line(7)).unwrap();
        assert_eq!(ats(&tail.read().unwrap()), [7]);
        let resumed = LogTail::at(path.clone(), tail.inode(), tail.offset());
        assert!(resumed.clone().read().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
