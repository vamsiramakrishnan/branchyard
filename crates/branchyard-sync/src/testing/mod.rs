//! Hermetic stand-ins for tests (feature `testing`): HTTP servers on
//! loopback that implement S3, Cloud Storage, Azure Blob, the three KMS
//! APIs and the cloud token sources with their real conditional and
//! authentication semantics ([`s3`], [`gcs`], [`azure`], [`cloud`]), and
//! store wrappers that fail or slow down on purpose.

pub mod azure;
pub mod cloud;
pub mod gcs;
pub mod s3;
pub mod server;

use branchyard_support::LockExt as _;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::store::{Entry, Generation, Object, ObjectStore, UploadJournal};

/// A store that fails writes on purpose: as though the process died
/// before (or while) writing what a rule names.
pub struct FaultyStore {
    pub inner: Arc<dyn ObjectStore>,
    /// Writes of keys containing this text fail.
    pub fail_writes_of: Mutex<Option<String>>,
    /// After this many more successful writes, every write fails.
    pub writes_left: Mutex<Option<usize>>,
    pub writes: AtomicUsize,
}

impl FaultyStore {
    pub fn new(inner: Arc<dyn ObjectStore>) -> FaultyStore {
        FaultyStore {
            inner,
            fail_writes_of: Mutex::new(None),
            writes_left: Mutex::new(None),
            writes: AtomicUsize::new(0),
        }
    }

    pub fn fail_writes_of(&self, text: Option<&str>) {
        *self.fail_writes_of.lock_recovering("fail_writes_of") = text.map(str::to_owned);
    }

    pub fn crash_after(&self, writes: Option<usize>) {
        *self.writes_left.lock_recovering("writes_left") = writes;
    }

    fn write(&self, key: &str) -> Result<()> {
        if let Some(text) = self
            .fail_writes_of
            .lock_recovering("fail_writes_of")
            .as_deref()
        {
            if key.contains(text) {
                return Err(Error::refused(format!(
                    "simulated crash before writing {key}"
                )));
            }
        }
        let mut left = self.writes_left.lock_recovering("writes_left");
        if let Some(n) = left.as_mut() {
            if *n == 0 {
                return Err(Error::refused(format!(
                    "simulated crash before writing {key}"
                )));
            }
            *n -= 1;
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl ObjectStore for FaultyStore {
    fn url(&self) -> String {
        self.inner.url()
    }
    fn get(&self, key: &str) -> Result<Object> {
        self.inner.get(key)
    }
    fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        self.inner.get_range(key, start, len)
    }
    fn stat(&self, key: &str) -> Result<Option<Entry>> {
        self.inner.stat(key)
    }
    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<Generation> {
        self.write(key)?;
        self.inner.put_if_absent(key, data)
    }
    fn put_if_match(&self, key: &str, data: &[u8], generation: &str) -> Result<Generation> {
        self.write(key)?;
        self.inner.put_if_match(key, data, generation)
    }
    fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        self.inner.list(prefix)
    }
    fn delete_if_match(&self, key: &str, generation: &str) -> Result<()> {
        self.inner.delete_if_match(key, generation)
    }
    fn resumable_put(
        &self,
        key: &str,
        data: &[u8],
        journal: &dyn UploadJournal,
    ) -> Result<Generation> {
        self.write(key)?;
        self.inner.resumable_put(key, data, journal)
    }
}

/// A store whose writes of keys containing `slow` wait until `wanted`
/// writes are in flight together (or a second passes), recording the most
/// at once, so a test can see the concurrency bound reached and held.
pub struct SlowStore {
    pub inner: Arc<dyn ObjectStore>,
    pub slow: String,
    pub wanted: usize,
    in_flight: AtomicUsize,
    pub max: AtomicUsize,
    pub bytes: AtomicU64,
}

impl SlowStore {
    pub fn new(inner: Arc<dyn ObjectStore>, slow: &str, wanted: usize) -> SlowStore {
        SlowStore {
            inner,
            slow: slow.to_owned(),
            wanted,
            in_flight: AtomicUsize::new(0),
            max: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    fn enter(&self, key: &str, bytes: usize) -> bool {
        if !key.contains(&self.slow) {
            return false;
        }
        self.bytes.fetch_add(bytes as u64, Ordering::SeqCst);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(now, Ordering::SeqCst);
        let start = std::time::Instant::now();
        while self.in_flight.load(Ordering::SeqCst) < self.wanted
            && start.elapsed() < Duration::from_secs(1)
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        // Stay a moment, so others that would exceed the bound overlap.
        std::thread::sleep(Duration::from_millis(10));
        true
    }

    fn leave(&self, entered: bool) {
        if entered {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl ObjectStore for SlowStore {
    fn url(&self) -> String {
        self.inner.url()
    }
    fn get(&self, key: &str) -> Result<Object> {
        self.inner.get(key)
    }
    fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        self.inner.get_range(key, start, len)
    }
    fn stat(&self, key: &str) -> Result<Option<Entry>> {
        self.inner.stat(key)
    }
    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<Generation> {
        let e = self.enter(key, data.len());
        let out = self.inner.put_if_absent(key, data);
        self.leave(e);
        out
    }
    fn put_if_match(&self, key: &str, data: &[u8], generation: &str) -> Result<Generation> {
        self.inner.put_if_match(key, data, generation)
    }
    fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        self.inner.list(prefix)
    }
    fn delete_if_match(&self, key: &str, generation: &str) -> Result<()> {
        self.inner.delete_if_match(key, generation)
    }
    fn resumable_put(
        &self,
        key: &str,
        data: &[u8],
        journal: &dyn UploadJournal,
    ) -> Result<Generation> {
        let e = self.enter(key, data.len());
        let out = self.inner.resumable_put(key, data, journal);
        self.leave(e);
        out
    }
}

/// Run git in `dir` for a test, panicking on failure; returns stdout.
pub fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A new repository at `dir` with one commit on `main`.
pub fn repo(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).expect("mkdir");
    git(dir, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(dir.join("README.md"), "hello\n").expect("write");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", "first"]);
}

/// Commit `files` on the branch checked out in `dir`; returns the commit.
pub fn commit(dir: &std::path::Path, message: &str, files: &[(&str, &str)]) -> String {
    for (path, text) in files {
        let path = dir.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, text).expect("write");
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "--allow-empty", "-m", message]);
    git(dir, &["rev-parse", "HEAD"])
}
