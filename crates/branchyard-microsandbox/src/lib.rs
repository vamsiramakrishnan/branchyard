//! A [`SandboxProvider`](branchyard_sandbox::SandboxProvider) on the
//! Microsandbox microVM runtime, through its public Rust SDK.
//!
//! `MicrosandboxProvider` (cargo feature `microsandbox`, off by default)
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
//!   into new sandboxes ([`plan::capabilities`]). The SDK's pause and
//!   resume, live branching through temporary full checkpoints and
//!   full-memory snapshots are declared only when opted in
//!   ([`plan::capabilities_with`], `MicrosandboxProvider::with_live_branch`),
//!   until the ignored KVM tests qualify them. In-place restore is never
//!   declared.
//!
//! What it does not guarantee:
//!
//! - Complete qualification. The dated live KVM run in
//!   `docs/remote-validation-2026-10-28.md` exercises the pinned runtime,
//!   including live branches, but the hard memory-ceiling assertion still
//!   fails. The `#[ignore]` tests in `tests/microsandbox.rs` remain the M2
//!   gate. See `docs/providers.md` for setup and `docs/sandbox-snapshots.md`
//!   for the engine's snapshot behavior.
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

/// What a build without the SDK tells a person to run to get one, from a
/// checkout. Building it needs Rust 1.94 or newer and `libcap-ng`'s
/// development files; running a microVM needs KVM.
pub const REBUILD: &str =
    "cargo install --locked --path crates/branchyard-cli --features microsandbox";

/// The sentence every refusal of a build without the SDK ends with, so a
/// person is never left with only the name of a cargo feature.
pub const REBUILD_HELP: &str = "rebuild from a checkout with `cargo install --locked --path \
     crates/branchyard-cli --features microsandbox` (it needs Rust 1.94 or newer and \
     libcap-ng-dev to build, and KVM to run a microVM); see docs/modes.md#sandbox";

/// Check that this process can open Linux's KVM device for read and write.
///
/// This does not start a VM or qualify the runtime. Other platforms retain the
/// SDK's platform-specific checks.
///
/// # Errors
///
/// On Linux, reports a missing or inaccessible `/dev/kvm` with a recovery step.
pub fn host_readiness() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        kvm_access(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .map(|_| ()),
        )?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn kvm_access(access: std::io::Result<()>) -> Result<(), String> {
    access.map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound =>
            "KVM device /dev/kvm is missing; enable KVM or nested virtualization on this host before running Microsandbox".into(),
        std::io::ErrorKind::PermissionDenied =>
            "this process cannot read/write /dev/kvm; grant your user access to the kvm group, start a new login session, then rerun by up".into(),
        _ => format!("cannot open /dev/kvm for read/write: {error}; fix host KVM access, then rerun by up"),
    })
}

#[cfg(any(feature = "microsandbox", test))]
pub(crate) fn socket_path_help(message: &str) -> Option<String> {
    message.contains("Unix socket path is too long").then(|| format!(
        "{message}; set MSB_HOME to a short, user-owned directory (for example /tmp/msb-<uid>), initialize the pinned msb runtime there, then rerun the command"))
}

#[cfg(test)]
mod host_tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn kvm_refusals_distinguish_a_missing_device_from_login_permissions() {
        use std::io::{Error, ErrorKind};
        assert!(super::kvm_access(Ok(())).is_ok());
        let missing = super::kvm_access(Err(Error::from(ErrorKind::NotFound))).unwrap_err();
        assert!(missing.contains("nested virtualization"));
        let denied = super::kvm_access(Err(Error::from(ErrorKind::PermissionDenied))).unwrap_err();
        assert!(denied.contains("kvm group"));
        assert!(denied.contains("new login session"));
        let other = super::kvm_access(Err(Error::from(ErrorKind::Other))).unwrap_err();
        assert!(other.contains("read/write"));
    }

    #[test]
    fn only_the_known_socket_path_failure_gets_msb_home_recovery() {
        let hint = super::socket_path_help(
            "Unix socket path is too long: 109 bytes; paths must be shorter than 107 bytes",
        )
        .unwrap();
        assert!(hint.contains("MSB_HOME"));
        assert!(hint.contains("user-owned"));
        assert!(super::socket_path_help("runtime could not start").is_none());
    }
}
