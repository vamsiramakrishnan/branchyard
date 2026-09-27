//! The Branchyard bridge: an exec endpoint for sandboxes whose runtime has
//! no exec API, such as Agent Substrate actors.
//!
//! The `branchyard-bridge` binary runs inside the sandbox and listens for
//! WebSocket connections routed to it ([`server`]). Each connection carries
//! a per-attempt credential ([`credential`]) and one request in the
//! [`protocol`]: start a process with piped stdio, move a file or a
//! directory tree in or out, end an attempt, or tear everything down. The
//! host side ([`client`]) turns an exec into a [`branchyard_sandbox::Process`].
//!
//! Unix only.

#![cfg(unix)]

pub mod client;
pub mod credential;
pub mod protocol;
pub mod server;
pub mod tree;
pub mod ws;

pub use client::{BridgeProcess, Endpoint};
pub use credential::{Claims, Identity, Refusal, Signer, Verifier, KEY_ENV};
