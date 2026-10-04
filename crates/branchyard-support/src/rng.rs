//! Randomness: one seedable generator, and the operating system's entropy
//! with its failure kept.
//!
//! [`SplitMix64`] is the generator for jitter, sampling, routing and fixed
//! tables. It is small, and a seed gives the same sequence on every
//! platform and in every release, which is why it is not `rand`'s `SmallRng`
//! (whose stream may change between versions): a seeded route, a retry
//! schedule in a test and the chunking table in `tasks::large` all promise
//! to repeat. It is not for keys; use [`fill_random`] or the crates that
//! sign for that.
//!
//! [`fill_random`] and [`entropy_seed`] ask the operating system through
//! `getrandom` and return its failure as an [`EntropyError`]; nothing here
//! swallows it. [`fresh_seed`] is the one infallible form, for seeds where a
//! weaker fallback is better than none, and it logs when it falls back.

use std::fmt;

const GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

/// One SplitMix64 step: the state after `state` and the word it yields.
/// A `const fn`, so a fixed table can be built at compile time.
pub const fn splitmix64(state: u64) -> (u64, u64) {
    let state = state.wrapping_add(GAMMA);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (state, z ^ (z >> 31))
}

/// SplitMix64: a small generator whose seeded sequence is fixed forever.
#[derive(Clone, Debug)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> SplitMix64 {
        SplitMix64(seed)
    }

    /// A generator seeded from the operating system, or from [`fresh_seed`]'s
    /// fallback when it fails.
    pub fn from_entropy() -> SplitMix64 {
        SplitMix64(fresh_seed())
    }

    pub fn next_u64(&mut self) -> u64 {
        let (state, word) = splitmix64(self.0);
        self.0 = state;
        word
    }

    /// Uniform in `0..=max` (with the small modulo bias a jitter does not
    /// mind).
    pub fn below_or_at(&mut self, max: u64) -> u64 {
        if max == u64::MAX {
            return self.next_u64();
        }
        self.next_u64() % (max + 1)
    }

    /// Uniform in `0..n`; `n` of zero yields zero.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// Uniform in the open interval (0, 1).
    pub fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
}

/// The operating system's random generator failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntropyError(String);

impl fmt::Display for EntropyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the system random generator failed: {}", self.0)
    }
}

impl std::error::Error for EntropyError {}

impl From<EntropyError> for std::io::Error {
    fn from(error: EntropyError) -> std::io::Error {
        std::io::Error::other(error)
    }
}

/// Fill `buf` from the operating system's secure generator.
pub fn fill_random(buf: &mut [u8]) -> Result<(), EntropyError> {
    getrandom::fill(buf).map_err(|e| EntropyError(e.to_string()))
}

/// A 64-bit seed from the operating system.
pub fn entropy_seed() -> Result<u64, EntropyError> {
    let mut bytes = [0u8; 8];
    fill_random(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

/// A seed for jitter and routing that must not be the same every run: the
/// operating system's, else (logged) one mixed from the clock and process.
pub fn fresh_seed() -> u64 {
    entropy_seed().unwrap_or_else(|error| {
        tracing::warn!(%error, "seeding from the clock and process id instead");
        let mixed = crate::time::now_nanos() as u64 ^ (u64::from(std::process::id()) << 32);
        splitmix64(mixed).1
    })
}
