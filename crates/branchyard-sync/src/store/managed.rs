//! A backend as sync uses it: every call retried with backoff and full
//! jitter when it fails transiently, bytes counted each way, and data
//! paced by the bandwidth budget. Concurrency is bounded by the callers
//! (the uploader's worker pool); this records the most calls in flight at
//! once, which tests check.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::error::Result;
use crate::pace::{Budget, Retrier};
use crate::stats::Stats;
use crate::store::{Entry, Generation, Object, ObjectStore, UploadJournal};

pub struct Managed {
    inner: Arc<dyn ObjectStore>,
    retrier: Arc<Retrier>,
    budget: Option<Arc<Budget>>,
    stats: Arc<Stats>,
    in_flight: AtomicU64,
    pub max_in_flight: AtomicU64,
}

struct Flight<'a>(&'a Managed);

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Managed {
    pub fn new(
        inner: Arc<dyn ObjectStore>,
        retrier: Arc<Retrier>,
        budget: Option<Arc<Budget>>,
        stats: Arc<Stats>,
    ) -> Managed {
        Managed {
            inner,
            retrier,
            budget,
            stats,
            in_flight: AtomicU64::new(0),
            max_in_flight: AtomicU64::new(0),
        }
    }

    pub fn inner(&self) -> &Arc<dyn ObjectStore> {
        &self.inner
    }

    fn fly(&self) -> Flight<'_> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        Flight(self)
    }

    fn pace(&self, bytes: usize) {
        if let Some(budget) = &self.budget {
            budget.take(bytes as u64);
        }
    }

    fn sent(&self, bytes: usize) {
        Stats::add(&self.stats.bytes_up, bytes as u64);
        Stats::add(&self.stats.objects_up, 1);
    }

    fn received(&self, bytes: usize) {
        Stats::add(&self.stats.bytes_down, bytes as u64);
        Stats::add(&self.stats.objects_down, 1);
    }

    fn retried<T>(&self, op: impl FnMut() -> Result<T>) -> Result<T> {
        let before = self.retrier.retries.load(Ordering::Relaxed);
        let out = self.retrier.run(op);
        let after = self.retrier.retries.load(Ordering::Relaxed);
        Stats::add(&self.stats.retries, after - before);
        out
    }
}

impl ObjectStore for Managed {
    fn url(&self) -> String {
        self.inner.url()
    }

    fn get(&self, key: &str) -> Result<Object> {
        let _f = self.fly();
        let object = self.retried(|| self.inner.get(key))?;
        self.pace(object.data.len());
        self.received(object.data.len());
        Ok(object)
    }

    fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        let _f = self.fly();
        let data = self.retried(|| self.inner.get_range(key, start, len))?;
        self.pace(data.len());
        self.received(data.len());
        Ok(data)
    }

    fn stat(&self, key: &str) -> Result<Option<Entry>> {
        let _f = self.fly();
        self.retried(|| self.inner.stat(key))
    }

    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<Generation> {
        let _f = self.fly();
        self.pace(data.len());
        let g = self.retried(|| self.inner.put_if_absent(key, data))?;
        self.sent(data.len());
        Ok(g)
    }

    fn put_if_match(&self, key: &str, data: &[u8], generation: &str) -> Result<Generation> {
        let _f = self.fly();
        self.pace(data.len());
        let g = self.retried(|| self.inner.put_if_match(key, data, generation))?;
        self.sent(data.len());
        Ok(g)
    }

    fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        let _f = self.fly();
        self.retried(|| self.inner.list(prefix))
    }

    fn delete_if_match(&self, key: &str, generation: &str) -> Result<()> {
        let _f = self.fly();
        self.retried(|| self.inner.delete_if_match(key, generation))
    }

    fn resumable_put(
        &self,
        key: &str,
        data: &[u8],
        journal: &dyn UploadJournal,
    ) -> Result<Generation> {
        let _f = self.fly();
        self.pace(data.len());
        let g = self.retried(|| self.inner.resumable_put(key, data, journal))?;
        self.sent(data.len());
        Ok(g)
    }
}
