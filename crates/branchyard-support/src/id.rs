//! Identifiers.

use ulid::Ulid;

use crate::rng::{fill_random, EntropyError};

/// A new ULID, 26 characters of Crockford base 32: 48 bits of milliseconds
/// and 80 random bits, so ids sort by creation time.
///
/// Fails when the operating system's random generator does, rather than
/// minting an id from zeros.
pub fn new_ulid() -> Result<String, EntropyError> {
    let mut random = [0u8; 10];
    fill_random(&mut random)?;
    Ok(ulid_from_parts(crate::time::now_ms(), random))
}

/// The ULID of `ms` milliseconds and 80 bits of `random` bytes.
pub fn ulid_from_parts(ms: u64, random: [u8; 10]) -> String {
    let mut bytes = [0u8; 16];
    bytes[6..].copy_from_slice(&random);
    Ulid::from_parts(ms, u128::from_be_bytes(bytes)).to_string()
}
