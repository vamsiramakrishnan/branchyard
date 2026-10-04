//! Retries with exponential backoff and full jitter, a bandwidth budget
//! (a token bucket), and the sleeping both do, which tests replace with a
//! clock they move by hand.

use branchyard_support::LockExt as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use branchyard::services::Clock;

use crate::error::Result;
use branchyard_support::rng::SplitMix64;

/// How waiting happens.
pub trait Sleeper: Send + Sync {
    fn sleep(&self, duration: Duration);
}

/// The thread sleeps.
pub struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Moves a manual clock instead of waiting, and remembers how long it
/// "slept" in all.
pub struct ManualSleeper {
    pub clock: Arc<AtomicU64>,
    pub slept_ms: AtomicU64,
}

impl ManualSleeper {
    pub fn new(clock: Arc<AtomicU64>) -> ManualSleeper {
        ManualSleeper {
            clock,
            slept_ms: AtomicU64::new(0),
        }
    }
}

impl Sleeper for ManualSleeper {
    fn sleep(&self, duration: Duration) {
        let ms = duration.as_millis() as u64;
        self.clock.fetch_add(ms, Ordering::SeqCst);
        self.slept_ms.fetch_add(ms, Ordering::SeqCst);
    }
}

/// Exponential backoff with full jitter: before try `n` (from 1), wait a
/// uniformly random time in `0..=min(cap, base * 2^n)`.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub attempts: u32,
    pub base: Duration,
    pub cap: Duration,
    pub seed: u64,
}

impl Default for RetryPolicy {
    fn default() -> RetryPolicy {
        RetryPolicy {
            attempts: 6,
            base: Duration::from_millis(200),
            cap: Duration::from_secs(30),
            seed: branchyard_support::rng::fresh_seed(),
        }
    }
}

impl RetryPolicy {
    /// The wait before retry `n` (1-based), drawn from `rng`.
    pub fn delay(&self, n: u32, rng: &mut SplitMix64) -> Duration {
        let ceiling = self
            .base
            .as_millis()
            .saturating_mul(1u128 << n.min(30))
            .min(self.cap.as_millis()) as u64;
        Duration::from_millis(rng.below_or_at(ceiling))
    }
}

/// Runs operations with retries, counting each retry.
pub struct Retrier {
    pub policy: RetryPolicy,
    pub sleeper: Arc<dyn Sleeper>,
    rng: Mutex<SplitMix64>,
    pub retries: AtomicU64,
}

impl Retrier {
    pub fn new(policy: RetryPolicy, sleeper: Arc<dyn Sleeper>) -> Retrier {
        let rng = Mutex::new(SplitMix64::new(policy.seed));
        Retrier {
            policy,
            sleeper,
            rng,
            retries: AtomicU64::new(0),
        }
    }

    /// The next jittered wait before retry `n`.
    pub fn backoff(&self, n: u32) -> Duration {
        let mut rng = self.rng.lock_recovering("rng");
        self.policy.delay(n, &mut rng)
    }

    /// Run `op` until it succeeds, fails for good, or runs out of tries.
    /// Only [`Kind::Transient`](crate::Kind::Transient) errors are tried
    /// again; every operation retried here is idempotent by construction
    /// (content-addressed puts, conditional writes, reads).
    pub fn run<T>(&self, mut op: impl FnMut() -> Result<T>) -> Result<T> {
        let mut n = 0;
        loop {
            match op() {
                Err(e) if e.retryable() && n + 1 < self.policy.attempts => {
                    n += 1;
                    self.retries.fetch_add(1, Ordering::Relaxed);
                    self.sleeper.sleep(self.backoff(n));
                }
                other => return other,
            }
        }
    }
}

/// A token bucket of bytes: at most `rate` bytes a second on average, in
/// bursts of at most one second's worth.
pub struct Budget {
    rate: u64,
    clock: Clock,
    sleeper: Arc<dyn Sleeper>,
    state: Mutex<(f64, u64)>,
}

impl Budget {
    pub fn new(rate: u64, clock: Clock, sleeper: Arc<dyn Sleeper>) -> Budget {
        let now = clock.now();
        Budget {
            rate: rate.max(1),
            clock,
            sleeper,
            state: Mutex::new((rate.max(1) as f64, now)),
        }
    }

    pub fn rate(&self) -> u64 {
        self.rate
    }

    /// Take `bytes`, waiting until the bucket has them.
    pub fn take(&self, bytes: u64) {
        let mut state = self.state.lock_recovering("state");
        let capacity = self.rate as f64;
        let now = self.clock.now();
        let (tokens, last) = *state;
        let tokens =
            (tokens + now.saturating_sub(last) as f64 * self.rate as f64 / 1000.0).min(capacity);
        let after = tokens - bytes as f64;
        if after >= 0.0 {
            *state = (after, now);
            return;
        }
        // Wait out the deficit while holding the bucket, so takers queue.
        let wait_ms = ((-after) * 1000.0 / self.rate as f64).ceil() as u64;
        self.sleeper.sleep(Duration::from_millis(wait_ms));
        *state = (0.0, self.clock.now().max(now + wait_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_full_jitter_under_a_cap() {
        let policy = RetryPolicy {
            attempts: 5,
            base: Duration::from_millis(100),
            cap: Duration::from_millis(1000),
            seed: 42,
        };
        let mut rng = SplitMix64::new(1);
        for n in 1..12 {
            let ceiling = (100u64 << n).min(1000);
            for _ in 0..50 {
                assert!(policy.delay(n, &mut rng).as_millis() as u64 <= ceiling);
            }
        }
        // Full jitter: waits spread over the whole range.
        let waits: Vec<u64> = (0..200)
            .map(|_| policy.delay(10, &mut rng).as_millis() as u64)
            .collect();
        assert!(waits.iter().any(|w| *w < 200) && waits.iter().any(|w| *w > 800));
    }

    #[test]
    fn retries_only_transient_errors() {
        let (clock, cell) = Clock::manual(0);
        let _ = clock;
        let sleeper = Arc::new(ManualSleeper::new(cell));
        let retrier = Retrier::new(
            RetryPolicy {
                attempts: 4,
                base: Duration::from_millis(10),
                cap: Duration::from_millis(100),
                seed: 7,
            },
            sleeper.clone(),
        );
        let mut calls = 0;
        let out: Result<u32> = retrier.run(|| {
            calls += 1;
            match calls < 3 {
                true => Err(crate::Error::transient("flaky")),
                false => Ok(calls),
            }
        });
        assert_eq!(out.unwrap(), 3);
        assert_eq!(retrier.retries.load(Ordering::SeqCst), 2);
        let mut calls = 0;
        let out: Result<()> = retrier.run(|| {
            calls += 1;
            Err(crate::Error::refused("no"))
        });
        assert!(out.is_err());
        assert_eq!(calls, 1);
        let mut calls = 0;
        let out: Result<()> = retrier.run(|| {
            calls += 1;
            Err(crate::Error::transient("down"))
        });
        assert!(out.is_err());
        assert_eq!(calls, 4, "gives up after the policy's attempts");
    }

    #[test]
    fn the_budget_holds_the_rate() {
        let (clock, cell) = Clock::manual(1_000_000);
        let sleeper = Arc::new(ManualSleeper::new(cell));
        let budget = Budget::new(100_000, clock, sleeper.clone());
        // One second's burst is free; the next 900 KB take 9 seconds.
        for _ in 0..10 {
            budget.take(100_000);
        }
        let slept = sleeper.slept_ms.load(Ordering::SeqCst);
        assert!((8_900..=9_100).contains(&slept), "slept {slept} ms");
    }
}
