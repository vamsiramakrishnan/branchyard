//! Sync: Branchyard's task repositories replicated to durable storage you
//! choose (Google Cloud Storage, S3 and S3-compatible stores, Azure Blob, a
//! git remote, or a directory), so a task survives its machine, follows
//! you to another, and can be run by a server. See `docs/sync.md` and the
//! sync sections of `docs/task-repos.md`.
//!
//! - **Local first.** The local repository is the working copy. A durable
//!   outbox ([`outbox`]) records what needs pushing; a background
//!   [`replicator`] drains it, never on a turn's hot path.
//! - **Immutable objects, one mutable pointer.** Git packs, chunks, chunk
//!   indexes and conversation segments are uploaded once under
//!   content-derived names; each task has one manifest
//!   ([`manifest::Manifest`]) replaced by compare-and-swap ([`engine`]).
//!   A reader sees the old state or the new, never part of one.
//! - **Divergence becomes branches.** A swap that loses fetches, merges
//!   (fast-forwards), and tries again with backoff and full jitter; two
//!   machines that moved one ref apart record
//!   `refs/heads/conflict/<device>/<n>`. Last-writer-wins is never used.
//! - **One runner at a time.** [`lease`]: a lease object per task attempt,
//!   written with a precondition and an expiry, renewed while it runs, and
//!   registered in the service registry.
//! - **Encrypted before it leaves.** [`seal`]: envelope encryption with a
//!   data key per object, wrapped by a tenant key from a passphrase or a
//!   KMS ([`kms`]); keyed-hash names.
//! - **Verified on every read**, scrubbed by sample ([`scrub`]), and
//!   **collected safely** by two-phase mark and sweep with a grace period,
//!   retention, legal holds and a quota ([`gc`]).
//! - **Backends** ([`store`]): one [`ObjectStore`] interface, conformance
//!   tested on every backend; in-process stand-ins for the cloud APIs are
//!   in [`testing`] (feature `testing`).
//! - **What is synced** is a [`SyncSource`]: a git directory, a task ID and
//!   a chunk directory. [`source::BranchSource`] adapts today's branches;
//!   a task repository implements the same trait.

pub mod auth;
pub mod config;
pub mod engine;
pub mod error;
pub mod gc;
pub mod git;
pub mod http;
pub mod kms;
pub mod lease;
pub mod manifest;
pub mod outbox;
pub mod pace;
pub mod replicator;
pub mod scrub;
pub mod seal;
pub mod source;
pub mod stats;
pub mod store;
#[cfg(feature = "testing")]
pub mod testing;
pub mod util;
pub mod yard;

pub use config::SyncConfig;
pub use engine::{Remote, Settings, SyncReport};
pub use error::{Error, Kind, Result};
pub use source::{BranchSource, ChunkId, SyncSource};
pub use store::{Entry, Generation, Object, ObjectStore};
