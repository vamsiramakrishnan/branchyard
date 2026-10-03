//! What sync did in this process: bytes and objects each way, manifest
//! swaps and the conflicts among them, retries, divergences recorded as
//! conflict branches, and reads refused for integrity. `/metrics` and
//! `by sync status` read a [`Snapshot`].

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Default, Debug)]
pub struct Stats {
    pub bytes_up: AtomicU64,
    pub bytes_down: AtomicU64,
    pub objects_up: AtomicU64,
    pub objects_down: AtomicU64,
    pub swaps: AtomicU64,
    pub swap_conflicts: AtomicU64,
    pub retries: AtomicU64,
    pub divergences: AtomicU64,
    pub corrupt: AtomicU64,
    pub errors: AtomicU64,
    pub syncs: AtomicU64,
}

/// The counters at one moment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub objects_up: u64,
    pub objects_down: u64,
    pub swaps: u64,
    pub swap_conflicts: u64,
    pub retries: u64,
    pub divergences: u64,
    pub corrupt: u64,
    pub errors: u64,
    pub syncs: u64,
}

impl Stats {
    pub fn add(counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Snapshot {
            bytes_up: get(&self.bytes_up),
            bytes_down: get(&self.bytes_down),
            objects_up: get(&self.objects_up),
            objects_down: get(&self.objects_down),
            swaps: get(&self.swaps),
            swap_conflicts: get(&self.swap_conflicts),
            retries: get(&self.retries),
            divergences: get(&self.divergences),
            corrupt: get(&self.corrupt),
            errors: get(&self.errors),
            syncs: get(&self.syncs),
        }
    }
}

impl Snapshot {
    /// Each counter by name, for writers that loop over them.
    pub fn pairs(&self) -> [(&'static str, u64); 11] {
        [
            ("bytes_up", self.bytes_up),
            ("bytes_down", self.bytes_down),
            ("objects_up", self.objects_up),
            ("objects_down", self.objects_down),
            ("swaps", self.swaps),
            ("swap_conflicts", self.swap_conflicts),
            ("retries", self.retries),
            ("divergences", self.divergences),
            ("corrupt", self.corrupt),
            ("errors", self.errors),
            ("syncs", self.syncs),
        ]
    }

    /// The sum of two snapshots (a persisted total and this process's).
    pub fn plus(&self, other: &Snapshot) -> Snapshot {
        Snapshot {
            bytes_up: self.bytes_up + other.bytes_up,
            bytes_down: self.bytes_down + other.bytes_down,
            objects_up: self.objects_up + other.objects_up,
            objects_down: self.objects_down + other.objects_down,
            swaps: self.swaps + other.swaps,
            swap_conflicts: self.swap_conflicts + other.swap_conflicts,
            retries: self.retries + other.retries,
            divergences: self.divergences + other.divergences,
            corrupt: self.corrupt + other.corrupt,
            errors: self.errors + other.errors,
            syncs: self.syncs + other.syncs,
        }
    }

    pub fn minus(&self, other: &Snapshot) -> Snapshot {
        Snapshot {
            bytes_up: self.bytes_up.saturating_sub(other.bytes_up),
            bytes_down: self.bytes_down.saturating_sub(other.bytes_down),
            objects_up: self.objects_up.saturating_sub(other.objects_up),
            objects_down: self.objects_down.saturating_sub(other.objects_down),
            swaps: self.swaps.saturating_sub(other.swaps),
            swap_conflicts: self.swap_conflicts.saturating_sub(other.swap_conflicts),
            retries: self.retries.saturating_sub(other.retries),
            divergences: self.divergences.saturating_sub(other.divergences),
            corrupt: self.corrupt.saturating_sub(other.corrupt),
            errors: self.errors.saturating_sub(other.errors),
            syncs: self.syncs.saturating_sub(other.syncs),
        }
    }
}
