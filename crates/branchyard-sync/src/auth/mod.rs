//! Credentials and request signing for the cloud backends and KMS APIs,
//! using official Google ADC and the backend-specific AWS/Azure adapters.

pub mod aws;
pub mod azure;
pub mod google;
pub mod sigv4;
