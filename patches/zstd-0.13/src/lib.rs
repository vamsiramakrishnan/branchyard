//! zstd 0.13's API for Microsandbox, served by zstd 0.14.
//!
//! `msb_krun_vmm` (through `microsandbox`, behind the optional
//! `microsandbox` feature) requires `zstd = "0.13"` and calls one function
//! of it, `zstd::stream::copy_decode`, to unpack an x86_64 kernel image.
//! zstd 0.13 sits on zstd-safe 7, which RustSec advisories cover
//! (RUSTSEC-2026-0391 and RUSTSEC-2026-0395 to -0406) with no fixed 0.13
//! release; every `msb_krun_vmm` up to 0.1.42 still asks for 0.13. The
//! workspace's `[patch.crates-io]` puts this crate in its place: it
//! re-exports zstd 0.14.3 (on the fixed zstd-safe 8.2), whose `copy_decode`
//! has the same signature, so no zstd-safe 7 is built at all.
//!
//! Remove it, and the patch, when `msb_krun_vmm` asks for zstd 0.14: cargo
//! then warns that the patch is unused. `tests/test_supply_chain.py` checks
//! that the lockfile resolves zstd 0.13 here and holds no zstd-safe 7.

pub use upstream::*;
