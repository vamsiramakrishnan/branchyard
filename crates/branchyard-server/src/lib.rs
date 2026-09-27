//! The Branchyard server: serves one or more repositories, each under a
//! name, over an authenticated HTTP JSON API with Server-Sent Events, and
//! runs the `branchyard` engine in-process.
//!
//! This is local mode behind an API. It changes where the engine is
//! reached, not how harnesses run: they are local processes of the
//! server's operating-system user, with no isolation beyond it, whoever
//! submits the work. See `docs/server.md`.
//!
//! What it guarantees:
//!
//! - Every `/v1` request needs a configured bearer token, compared in
//!   constant time; tokens are never logged.
//! - A long operation (task, send, fork, merge) is saved to the operation
//!   registry before `202 Accepted` is returned and before it starts, and
//!   runs on a server thread: disconnecting the client has no effect on it
//!   (design invariant 1).
//! - A repeated idempotency key from the same caller returns the original
//!   operation and never starts a second (invariant 4); the same key with a
//!   different request is refused.
//! - Operation status survives a restart. An operation that was queued or
//!   running when the server stopped is recorded as `interrupted`.
//! - The activity feed is durable and gap-free: a stream resumed from a
//!   cursor continues with the next entry.
//! - Plain HTTP binds only to loopback unless TLS is configured or
//!   `--insecure-bind` is given.
//!
//! What it does not guarantee:
//!
//! - Isolation between callers, or from the server's user. Any token holder
//!   can make harnesses act as that user; `allow_client_commands` also lets
//!   them choose executables.
//! - Cancellation. The SDK has no cancel operation, so neither has the API;
//!   budgets bound a turn.
//! - Branch state after a crash. The engine's record of a branch that was
//!   running when the server died still says `running`, and its harness may
//!   have outlived the server.
//! - Multiple servers per data directory, or PostgreSQL. Operations persist
//!   through the [`store::OperationStore`] trait to a JSON-lines file; the
//!   PostgreSQL and PGMQ store of `docs/design.md` §8 is the next step.

pub mod api;
pub mod auth;
pub mod cli;
pub mod config;
pub mod error;
pub mod feed;
pub mod ops;
pub mod serve;
pub mod store;

pub use config::Config;
pub use serve::{start, Handle, Running, Stopped};
