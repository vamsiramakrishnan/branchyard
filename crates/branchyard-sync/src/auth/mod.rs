//! Credentials and request signing for the cloud backends and KMS APIs,
//! written here against the published specifications: no cloud SDK is in
//! `Cargo.lock`, and these are small.

pub mod aws;
pub mod azure;
pub mod google;
pub mod sigv4;
