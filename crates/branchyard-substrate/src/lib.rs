//! Agent Substrate as a Branchyard sandbox provider.
//!
//! The gRPC client is generated at build time from Substrate's `ateapi.proto`,
//! vendored unmodified under `vendor/substrate/`. [`Actors`] maps that API's
//! actor lifecycle; no Substrate source is translated.
//!
//! Substrate has no exec API. [`SubstrateProvider`] therefore reaches a
//! harness through the Branchyard bridge (`branchyard-bridge`), which the
//! actor template runs ([`template`]) and the router forwards to, with a
//! per-attempt credential the host signs. An actor has no host mounts, so
//! code crosses by git bundle and directory tree ([`transfer`]).
//!
//! This profile is **unqualified**. It has been exercised only against the
//! in-process fake in [`fake`] (feature `fake`), not against a Substrate
//! cluster; see `docs/substrate.md`.

mod actors;
mod capabilities;
#[cfg(feature = "fake")]
pub mod fake;
mod provider;
pub mod template;
pub mod transfer;

pub use actors::{ActorHandle, Actors, CheckpointRef, Error};
pub use capabilities::{capabilities, runs_bridge, state, TemplateError};
pub use provider::{Config, Quiesce, SubstrateProvider};

/// Types and client generated from the vendored `ateapi.proto`.
#[allow(clippy::all, missing_docs)]
pub mod pb {
    tonic::include_proto!("ateapi");
}
