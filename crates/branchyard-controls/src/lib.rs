//! Harness identity, extracted from pinned upstream sources.
//!
//! This crate does not launch processes or authorize sessions. The caller must
//! bind every session to its tenant, run, workspace, and qualified harness
//! version. [`resume`] keeps only the registry check
//! [`harness`]'s tests validate against; see its module comment for why the
//! upstream CLI-recipe builder it once carried was removed rather than kept
//! unused.
pub mod harness;
pub mod resume;
