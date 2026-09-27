//! A [`SandboxProvider`](branchyard_sandbox::SandboxProvider) on the
//! Microsandbox microVM runtime, through its public Rust SDK.
//!
//! [`MicrosandboxProvider`] (cargo feature `microsandbox`, off by default)
//! boots one microVM per sandbox from an OCI image with CPU and memory
//! limits, binds host directories into it (the branch worktree at a fixed
//! guest path), runs execs with piped stdio through the in-guest agent, and
//! stops and destroys it. It pins `microsandbox` 0.7.3 with only its `local`
//! and `net` features: no cloud backend, no build-time runtime download.
//!
//! What it guarantees, by the SDK and guest-agent source it pins:
//!
//! - Hardware-virtualized separation from the host, as far as the runtime
//!   provides it: the guest sees only its image, the bound directories and
//!   the variables an exec passes. Nothing from this process's environment
//!   is forwarded.
//! - Every exec is a session leader in the guest, so `Process::kill` (the
//!   agent signals the process group) and `Process::teardown` (an in-guest
//!   `sh` listing and killing the group) reach its descendants; `stop` and
//!   `destroy` end the whole guest.
//! - Declared capabilities are exec plus live disk checkpoints that branch
//!   into new sandboxes ([`plan::capabilities`]). The SDK's full-memory
//!   snapshots, live branching and in-place restore are not declared.
//!
//! What it does not guarantee:
//!
//! - Qualification. Nothing here has run on a KVM host in this repository;
//!   the `#[ignore]` tests in `tests/microsandbox.rs` are the M2 gate. See
//!   `docs/providers.md` for the setup they need.
//! - That a checkpoint includes the workspace. Bound host directories are
//!   outside the guest's root disk.
//! - Network policy. The guest gets the runtime's default network; egress
//!   restriction is a separate, unimplemented capability.
//! - Bounded memory for output: the SDK buffers exec events without bound
//!   while a reader is slow.
//! - Ownership of files the guest writes into a bound directory; record it
//!   during qualification.
//!
//! Without the feature the crate still builds: [`plan`], [`bridge`] and
//! [`plan::capabilities`] hold everything that does not need the SDK, and
//! are tested here.

pub mod bridge;
pub mod plan;

#[cfg(feature = "microsandbox")]
mod provider;

#[cfg(feature = "microsandbox")]
pub use provider::{error, event, failure_kind, state, MicrosandboxProvider};

/// The pinned SDK version, which must match the installed `msb` runtime.
pub const SDK_VERSION: &str = "0.7.3";

/// Whether this build includes the SDK.
pub const ENABLED: bool = cfg!(feature = "microsandbox");
