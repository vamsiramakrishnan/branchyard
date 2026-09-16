//! Harness control recipes extracted from pinned upstream sources.
//!
//! This crate does not launch processes or authorize sessions. The caller must
//! bind every session to its tenant, run, workspace, and qualified harness
//! version. Resume recipes describe CLI syntax, not a headless driver.
pub mod resume;
