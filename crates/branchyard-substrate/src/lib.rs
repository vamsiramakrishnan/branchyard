//! Agent Substrate as a Branchyard sandbox provider.
//!
//! The gRPC client is generated at build time from Substrate's `ateapi.proto`,
//! vendored unmodified under `vendor/substrate/`. This crate maps that API onto
//! the vendor-independent types in `branchyard-sandbox`; no Substrate source is
//! translated.
//!
//! This profile is **unqualified**. It has been exercised only against an
//! in-process fake of the `Control` service, not against a Substrate cluster.
//! Substrate has no exec API: a harness in a Substrate actor is reached through
//! routed network ingress, so profiles that require process pipes are rejected
//! at admission.

mod capabilities;
mod provider;

pub use capabilities::{capabilities, state, TemplateError};
pub use provider::{ActorHandle, CheckpointRef, Error, SubstrateProvider};

/// Types and client generated from the vendored `ateapi.proto`.
#[allow(clippy::all, missing_docs)]
pub mod pb {
    tonic::include_proto!("ateapi");
}
