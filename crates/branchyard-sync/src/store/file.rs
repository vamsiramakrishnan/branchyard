//! `file:///path`: a directory, the reference backend. Every test run
//! passes the conformance suite against it.
//!
//! An object is the file at its key. A write goes to a temporary file in
//! `.tmp/`, is flushed to disk, and is renamed over the key while the
//! writer holds an exclusive lock on `.lock`, after checking the
//! precondition under the same lock; the directory is flushed after. A
//! reader opens the file once, so it reads the old object or the new one.
//! The generation is the file's inode, modification time and size, which
//! a rename always changes. Writers on one machine (or one shared file
//! system with working `flock`) take turns on the lock.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::store::{
    check_key, check_prefix, Entry, Generation, Object, ObjectStore, UploadJournal,
};

/// Bytes written per step of a resumable upload.
pub const PART: usize = 8 << 20;

pub struct FileStore {
    root: PathBuf,
    part: usize,
}

impl FileStore {
    pub fn open(root: &Path) -> Result<FileStore> {
        fs::create_dir_all(root.join(".tmp"))
            .map_err(|e| Error::local(format!("{}: {e}", root.display())))?;
        Ok(FileStore {
            root: root.to_path_buf(),
            part: PART,
        })
    }

    /// Resumable uploads in parts of `bytes` (tests use small parts).
    pub fn with_part_size(mut self, bytes: usize) -> FileStore {
        self.part = bytes.max(1);
        self
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        check_key(key)?;
        Ok(self.root.join(key))
    }

    fn lock(&self) -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(".lock"))?;
        file.lock()?;
        Ok(file)
    }

    fn temp(&self) -> PathBuf {
        let name = crate::util::hex(&crate::util::random_bytes(12).unwrap_or_default());
        self.root.join(".tmp").join(name)
    }

    fn write_temp(&self, data: &[u8]) -> Result<PathBuf> {
        let temp = self.temp();
        let mut file = File::create(&temp)?;
        file.write_all(data)?;
        file.sync_all()?;
        Ok(temp)
    }

    /// Rename `temp` over `key` if `check` passes on the current
    /// generation, under the lock.
    fn commit(
        &self,
        key: &str,
        temp: &Path,
        check: impl FnOnce(Option<Generation>) -> Result<()>,
    ) -> Result<Generation> {
        let path = self.path(key)?;
        let _lock = self.lock()?;
        let current = generation_of(&path)?;
        if let Err(e) = check(current) {
            let _ = fs::remove_file(temp);
            return Err(e);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(temp, &path)?;
        if let Some(parent) = path.parent() {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        generation_of(&path)?.ok_or_else(|| Error::local(format!("{key} vanished")))
    }
}

fn generation_of(path: &Path) -> Result<Option<Generation>> {
    match fs::metadata(path) {
        Ok(meta) => Ok(Some(generation(&meta))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(unix)]
fn generation(meta: &fs::Metadata) -> Generation {
    use std::os::unix::fs::MetadataExt;
    format!(
        "{:x}-{:x}.{:x}-{:x}",
        meta.ino(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.size()
    )
}

#[cfg(not(unix))]
fn generation(meta: &fs::Metadata) -> Generation {
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{modified:x}-{:x}", meta.len())
}

fn modified_ms(meta: &fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
}

fn missing(key: &str) -> Error {
    Error::not_found(format!("{key} is not in the remote"))
}

impl ObjectStore for FileStore {
    fn url(&self) -> String {
        format!("file://{}", self.root.display())
    }

    fn get(&self, key: &str) -> Result<Object> {
        let path = self.path(key)?;
        let mut file = File::open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => missing(key),
            _ => e.into(),
        })?;
        let meta = file.metadata()?;
        let mut data = Vec::with_capacity(meta.len() as usize);
        file.read_to_end(&mut data)?;
        Ok(Object {
            data,
            generation: generation(&meta),
        })
    }

    fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        let path = self.path(key)?;
        let mut file = File::open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => missing(key),
            _ => e.into(),
        })?;
        file.seek(SeekFrom::Start(start))?;
        let mut data = Vec::new();
        file.take(len).read_to_end(&mut data)?;
        Ok(data)
    }

    fn stat(&self, key: &str) -> Result<Option<Entry>> {
        let path = self.path(key)?;
        match fs::metadata(&path) {
            Ok(meta) => Ok(Some(Entry {
                key: key.to_owned(),
                size: meta.len(),
                generation: generation(&meta),
                modified_ms: modified_ms(&meta),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<Generation> {
        self.path(key)?;
        let temp = self.write_temp(data)?;
        self.commit(key, &temp, |current| match current {
            None => Ok(()),
            Some(_) => Err(Error::precondition(format!("{key} already exists"))),
        })
    }

    fn put_if_match(&self, key: &str, data: &[u8], expected: &str) -> Result<Generation> {
        self.path(key)?;
        let temp = self.write_temp(data)?;
        self.commit(key, &temp, |current| match current {
            Some(g) if g == expected => Ok(()),
            Some(g) => Err(Error::precondition(format!(
                "{key} is at generation {g}, not {expected}"
            ))),
            None => Err(Error::precondition(format!("{key} does not exist"))),
        })
    }

    fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        check_prefix(prefix)?;
        // Walk the deepest directory the prefix names fully.
        let dir_part = match prefix.rfind('/') {
            Some(i) => &prefix[..i],
            None => "",
        };
        let start = match dir_part.is_empty() {
            true => self.root.clone(),
            false => self.root.join(dir_part),
        };
        let mut out = Vec::new();
        let mut stack = vec![start];
        while let Some(dir) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => continue,
                Err(e) => return Err(e.into()),
            };
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                let meta = match fs::metadata(&path) {
                    Ok(meta) => meta,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                if meta.is_dir() {
                    stack.push(path);
                    continue;
                }
                let key = path
                    .strip_prefix(&self.root)
                    .map_err(|_| Error::local("listed outside the root"))?
                    .to_string_lossy()
                    .replace('\\', "/");
                if key.starts_with(prefix) {
                    out.push(Entry {
                        key,
                        size: meta.len(),
                        generation: generation(&meta),
                        modified_ms: modified_ms(&meta),
                    });
                }
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn delete_if_match(&self, key: &str, expected: &str) -> Result<()> {
        let path = self.path(key)?;
        let _lock = self.lock()?;
        match generation_of(&path)? {
            Some(g) if g == expected => {
                fs::remove_file(&path)?;
                // Remove directories left empty, up to the root.
                let mut dir = path.parent().map(Path::to_path_buf);
                while let Some(d) = dir {
                    if d == self.root || fs::remove_dir(&d).is_err() {
                        break;
                    }
                    dir = d.parent().map(Path::to_path_buf);
                }
                Ok(())
            }
            Some(g) => Err(Error::precondition(format!(
                "{key} is at generation {g}, not {expected}"
            ))),
            None => Err(Error::precondition(format!("{key} does not exist"))),
        }
    }

    /// Appends parts to `.tmp/<key hash>.partial`, recording the length
    /// written in the journal, then commits it like `put_if_absent`.
    fn resumable_put(
        &self,
        key: &str,
        data: &[u8],
        journal: &dyn UploadJournal,
    ) -> Result<Generation> {
        self.path(key)?;
        let id = crate::util::hex(&blake3::hash(key.as_bytes()).as_bytes()[..12]);
        let partial = self.root.join(".tmp").join(format!("{id}.partial"));
        let digest = crate::util::hex(&blake3::hash(data).as_bytes()[..16]);
        let mut done: u64 = journal
            .load(key)
            .and_then(|state| {
                let (d, n) = state.split_once(':')?;
                (d == digest).then(|| n.parse().ok()).flatten()
            })
            .unwrap_or(0);
        if fs::metadata(&partial).map(|m| m.len()).unwrap_or(0) < done {
            done = 0;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&partial)?;
        file.set_len(done)?;
        file.seek(SeekFrom::Start(done))?;
        while (done as usize) < data.len() {
            let end = (done as usize + self.part).min(data.len());
            file.write_all(&data[done as usize..end])?;
            file.sync_data()?;
            done = end as u64;
            journal.save(key, &format!("{digest}:{done}"));
        }
        drop(file);
        let result = self.commit(key, &partial, |current| match current {
            None => Ok(()),
            Some(_) => Err(Error::precondition(format!("{key} already exists"))),
        });
        journal.clear(key);
        result
    }
}
