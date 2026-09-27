//! The Branchyard server: serves one or more repositories, each under a
//! name, over an authenticated HTTP JSON API with Server-Sent Events, and
//! runs the `branchyard` engine in-process.
//!
//! This is local mode behind an API. It changes where the engine is
//! reached, not how harnesses run: by default they are local processes of
//! the server's operating-system user, with no isolation beyond it,
//! whoever submits the work. The operator can allow requests to name the
//! Microsandbox or Substrate provider, to give harnesses delegation, and to
//! run profiles with unapproved tools; each is refused otherwise. See
//! `docs/server.md` and `docs/surfaces.md`.
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
//! - Branch state survives a crash: at start, and every 30 seconds, the
//!   engine recovers branches whose turn's engine stopped (this server's
//!   previous process, or a local `by`), killing a harness process group
//!   that outlived it when its pid and start time still match, and ending
//!   the branch `interrupted` when its outcome is unknown. Nothing is
//!   submitted again.
//! - Cancellation is durable: `POST .../branches/{b}/cancel` records a
//!   request that the engine running the turn observes, in this process or
//!   another on the same repository. So is steering: `POST
//!   .../branches/{b}/steer` queues input that engine delivers into the
//!   running turn, where the harness supports it.
//! - The activity feed is the engine's store, read from a cursor: a stream
//!   resumed from a cursor continues with the next entry, across restarts.
//! - Plain HTTP binds only to loopback unless TLS is configured or
//!   `--insecure-bind` is given.
//!
//! What it does not guarantee:
//!
//! - Isolation between callers, or from the server's user. Any token holder
//!   can make harnesses act as that user; `allow_client_commands` also lets
//!   them choose executables.
//! - Resuming a turn after a crash: a recovered turn is interrupted, never
//!   continued or resubmitted.
//! - Multiple servers per data directory or database schema. Operations
//!   persist through the [`store::OperationStore`] trait to SQLite in the
//!   data directory, or with the `postgres` feature and `database`, to
//!   PostgreSQL alongside the branch state (`docs/durability.md`).

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
