//! The object store interface every backend implements, and [`open`],
//! which picks a backend from a remote's URL.
//!
//! A backend is plain storage with conditional writes; what the objects
//! mean (names, sealing, manifests) is decided above it, once. Keys are
//! relative paths of `a-z A-Z 0-9 . _ - ~` segments joined by `/`. A
//! generation is the backend's own version token: a GCS generation, an S3
//! or Azure ETag, a git commit, or the file backend's inode and time.
//!
//! The semantics every backend passes (the conformance suite,
//! [`conformance`]):
//!
//! - `put_if_absent` writes only when nothing is at the key, else
//!   [`Kind::Precondition`](crate::Kind::Precondition).
//! - `put_if_match` replaces only the generation named, else
//!   `Precondition`; a missing object is `Precondition` too.
//! - `delete_if_match` deletes only the generation named.
//! - A reader sees the old object or the new one, never part of one.
//! - `list` returns every key under a prefix, sorted, across pages.
//! - `resumable_put` is `put_if_absent` for large objects, in parts, and
//!   resumes from what a journal recorded when a previous try stopped.

pub mod azure;
pub mod conformance;
pub mod file;
pub mod gcs;
pub mod git;
pub mod managed;
pub mod memory;
pub mod s3;
pub(crate) mod xml;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};

/// A backend's version token for one object.
pub type Generation = String;

/// An object read whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    pub data: Vec<u8>,
    pub generation: Generation,
}

/// A listed object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub size: u64,
    pub generation: Generation,
    /// When it was written, when the backend says.
    pub modified_ms: Option<u64>,
}

/// Where a resumable upload records how far it got, so a later try
/// continues instead of starting over. The sync outbox keeps it in
/// SQLite; [`MemoryJournal`] is for tests.
pub trait UploadJournal: Send + Sync {
    fn load(&self, key: &str) -> Option<String>;
    fn save(&self, key: &str, state: &str);
    fn clear(&self, key: &str);
}

/// A journal that remembers nothing: every upload starts over.
pub struct NoJournal;

impl UploadJournal for NoJournal {
    fn load(&self, _: &str) -> Option<String> {
        None
    }
    fn save(&self, _: &str, _: &str) {}
    fn clear(&self, _: &str) {}
}

/// A journal in memory.
#[derive(Default)]
pub struct MemoryJournal(Mutex<BTreeMap<String, String>>);

impl MemoryJournal {
    pub fn entries(&self) -> BTreeMap<String, String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl UploadJournal for MemoryJournal {
    fn load(&self, key: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }
    fn save(&self, key: &str, state: &str) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_owned(), state.to_owned());
    }
    fn clear(&self, key: &str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }
}

/// Durable storage with conditional writes.
pub trait ObjectStore: Send + Sync {
    /// The remote's URL, without credentials, for people.
    fn url(&self) -> String;

    fn get(&self, key: &str) -> Result<Object>;

    /// `len` bytes from `start` (fewer at the end of the object).
    fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Vec<u8>>;

    /// The object's size and generation, or `None` when it is not there.
    fn stat(&self, key: &str) -> Result<Option<Entry>>;

    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<Generation>;

    fn put_if_match(&self, key: &str, data: &[u8], generation: &str) -> Result<Generation>;

    /// Every object under `prefix`, sorted by key.
    fn list(&self, prefix: &str) -> Result<Vec<Entry>>;

    fn delete_if_match(&self, key: &str, generation: &str) -> Result<()>;

    /// `put_if_absent` in parts, resuming from `journal`.
    fn resumable_put(
        &self,
        key: &str,
        data: &[u8],
        journal: &dyn UploadJournal,
    ) -> Result<Generation> {
        let _ = journal;
        self.put_if_absent(key, data)
    }
}

/// Refuse keys a backend could misread: empty segments, `.` and `..`,
/// characters outside `a-z A-Z 0-9 . _ - ~`.
pub fn check_key(key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && key.len() <= 512
        && key.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && !segment.starts_with('.')
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~'))
        });
    match ok {
        true => Ok(()),
        false => Err(Error::config(format!("{key:?} is not a valid object key"))),
    }
}

/// Like [`check_key`], for a list prefix: empty, or segments ending in
/// `/` or a partial last segment.
pub fn check_prefix(prefix: &str) -> Result<()> {
    if prefix.is_empty() {
        return Ok(());
    }
    check_key(prefix.trim_end_matches('/'))
}

/// A remote's location, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Location {
    File {
        root: std::path::PathBuf,
    },
    S3 {
        bucket: String,
        prefix: String,
        query: Vec<(String, String)>,
    },
    Gcs {
        bucket: String,
        prefix: String,
        query: Vec<(String, String)>,
    },
    Azure {
        account: String,
        container: String,
        prefix: String,
        query: Vec<(String, String)>,
    },
    Git {
        url: String,
    },
    Memory {
        name: String,
    },
}

fn split_query(url: &str) -> (&str, Vec<(String, String)>) {
    match url.split_once('?') {
        Some((base, query)) => (base, crate::util::query_pairs(query)),
        None => (url, Vec::new()),
    }
}

fn bucket_and_prefix(rest: &str, url: &str) -> Result<(String, String)> {
    let rest = rest.trim_end_matches('/');
    let (bucket, prefix) = match rest.split_once('/') {
        Some((b, p)) => (b.to_owned(), p.trim_matches('/').to_owned()),
        None => (rest.to_owned(), String::new()),
    };
    if bucket.is_empty() {
        return Err(Error::config(format!("{url:?} names no bucket")));
    }
    if !prefix.is_empty() {
        check_key(&prefix).map_err(|_| Error::config(format!("{url:?} has an unusable prefix")))?;
    }
    Ok((bucket, prefix))
}

impl Location {
    pub fn parse(url: &str) -> Result<Location> {
        if let Some(path) = url.strip_prefix("file://") {
            if !path.starts_with('/') {
                return Err(Error::config(format!(
                    "{url:?} must name an absolute path, as file:///path"
                )));
            }
            return Ok(Location::File { root: path.into() });
        }
        if let Some(rest) = url.strip_prefix("git+") {
            if !["https://", "http://", "ssh://", "file://"]
                .iter()
                .any(|s| rest.starts_with(s))
            {
                return Err(Error::config(format!(
                    "{url:?} must be git+https://, git+ssh:// or git+file://"
                )));
            }
            return Ok(Location::Git {
                url: rest.to_owned(),
            });
        }
        if let Some(name) = url.strip_prefix("mem://") {
            return Ok(Location::Memory {
                name: name.to_owned(),
            });
        }
        let (base, query) = split_query(url);
        if let Some(rest) = base.strip_prefix("s3://") {
            let (bucket, prefix) = bucket_and_prefix(rest, url)?;
            return Ok(Location::S3 {
                bucket,
                prefix,
                query,
            });
        }
        if let Some(rest) = base.strip_prefix("gs://") {
            let (bucket, prefix) = bucket_and_prefix(rest, url)?;
            return Ok(Location::Gcs {
                bucket,
                prefix,
                query,
            });
        }
        if let Some(rest) = base.strip_prefix("az://") {
            let (account, rest) = rest.split_once('/').ok_or_else(|| {
                Error::config(format!("{url:?} must be az://account/container[/prefix]"))
            })?;
            let (container, prefix) = bucket_and_prefix(rest, url)?;
            if account.is_empty() {
                return Err(Error::config(format!("{url:?} names no account")));
            }
            return Ok(Location::Azure {
                account: account.to_owned(),
                container,
                prefix,
                query,
            });
        }
        Err(Error::config(format!(
            "{url:?} is not a sync remote: use gs://, s3://, az://, file:// or git+..."
        )))
    }
}

/// Join a remote's prefix and a key.
pub(crate) fn join(prefix: &str, key: &str) -> String {
    match prefix.is_empty() {
        true => key.to_owned(),
        false => format!("{prefix}/{key}"),
    }
}

/// Open the backend `url` names.
pub fn open(url: &str) -> Result<Arc<dyn ObjectStore>> {
    Ok(match Location::parse(url)? {
        Location::File { root } => Arc::new(file::FileStore::open(&root)?),
        Location::S3 {
            bucket,
            prefix,
            query,
        } => Arc::new(s3::S3Store::new(&bucket, &prefix, &query)?),
        Location::Gcs {
            bucket,
            prefix,
            query,
        } => Arc::new(gcs::GcsStore::new(&bucket, &prefix, &query)?),
        Location::Azure {
            account,
            container,
            prefix,
            query,
        } => Arc::new(azure::AzureStore::new(
            &account, &container, &prefix, &query,
        )?),
        Location::Git { url } => Arc::new(git::GitStore::open(&url, None)?),
        Location::Memory { name } => memory::MemoryStore::named(&name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_locations() {
        assert!(check_key("tasks/abc/manifest").is_ok());
        assert!(check_key("chunks/ab/0123").is_ok());
        for bad in ["", "/a", "a/", "a//b", "a/../b", ".hidden", "a b", "a?b"] {
            assert!(check_key(bad).is_err(), "{bad}");
        }
        assert_eq!(
            Location::parse("gs://bucket/some/prefix").unwrap(),
            Location::Gcs {
                bucket: "bucket".into(),
                prefix: "some/prefix".into(),
                query: vec![]
            }
        );
        match Location::parse("s3://b?endpoint=http%3A%2F%2F127.0.0.1%3A9000&region=auto").unwrap()
        {
            Location::S3 {
                bucket,
                prefix,
                query,
            } => {
                assert_eq!((bucket.as_str(), prefix.as_str()), ("b", ""));
                assert_eq!(
                    query[0],
                    ("endpoint".into(), "http://127.0.0.1:9000".into())
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            Location::parse("az://acct/cont/p").unwrap(),
            Location::Azure { .. }
        ));
        assert!(matches!(
            Location::parse("git+ssh://git@host/r.git").unwrap(),
            Location::Git { .. }
        ));
        assert!(Location::parse("file://relative").is_err());
        assert!(Location::parse("ftp://x").is_err());
        assert!(Location::parse("az://acct").is_err());
    }
}
