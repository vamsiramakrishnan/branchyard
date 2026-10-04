//! One runner per task attempt: a lease object in the remote,
//! `leases/<task>/<attempt>`, written with a precondition and an expiry.
//!
//! - **Acquire.** Create it if absent (`put_if_absent`); take it over if
//!   its expiry (plus the allowed clock skew) has passed, conditional on
//!   the generation read; renew it if this holder already has it.
//!   Anything else is [`Kind::LeaseHeld`], naming the holder.
//! - **Renew.** Conditional on the generation this holder last wrote. A
//!   renewal that finds another generation means the lease was taken
//!   after it ran out: the holder has lost it and must stop.
//!   [`LeaseKeeper`] renews from a thread of its own every third of the
//!   lease and records a loss, and why ([`LeaseKeeper::lost_reason`]); the
//!   runner polls it and stops (a server cancels the run, see
//!   `docs/sync.md#servers`).
//! - **Release.** Delete it, conditional on the generation.
//!
//! Each lease carries an epoch, one more on every take-over, which a
//! runner can use as a fencing token. A held lease is also registered in
//! the service registry (kind `sync_lease`), so `by services` shows who
//! runs what. Expiry compares clocks across machines: the allowed skew
//! (30 seconds by default) is the bound sync assumes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use branchyard::services::{Capability, Registration, Service, ServiceOwner, ServiceStore};
use serde::{Deserialize, Serialize};

use crate::engine::Remote;
use crate::error::{Error, Kind, Result};
use crate::manifest::{check_task_id, lease_key};
use crate::store::{Generation, ObjectStore as _};

/// The registry kind a held lease registers as.
pub const KIND_SYNC_LEASE: &str = "sync_lease";

/// What the lease object holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub task: String,
    pub attempt: String,
    /// Who holds it: a device and a process, unique per acquisition.
    pub holder: String,
    pub device: String,
    pub epoch: u64,
    pub acquired_ms: u64,
    pub renewed_ms: u64,
    pub expires_ms: u64,
}

/// A held lease.
#[derive(Debug)]
pub struct Lease {
    key: String,
    record: LeaseRecord,
    generation: Generation,
    ttl: Duration,
}

impl Lease {
    pub fn record(&self) -> &LeaseRecord {
        &self.record
    }

    pub fn epoch(&self) -> u64 {
        self.record.epoch
    }
}

fn check_attempt(attempt: &str) -> Result<()> {
    check_task_id(attempt).map_err(|_| Error::config(format!("{attempt:?} is not an attempt name")))
}

impl Remote {
    fn lease_key_of(&self, task: &str, attempt: &str) -> String {
        let sealer = self.sealer();
        lease_key(&sealer.task_dir(task), &sealer.keyed("attempt", attempt))
    }

    /// The lease on an attempt as the remote has it, live or not.
    pub fn lease_state(&self, task: &str, attempt: &str) -> Result<Option<LeaseRecord>> {
        check_task_id(task)?;
        check_attempt(attempt)?;
        match self.read(&self.lease_key_of(task, attempt)) {
            Ok((plain, _)) => Ok(Some(serde_json::from_slice(&plain)?)),
            Err(e) if e.is(Kind::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Take the lease on `attempt` of `task` for `ttl`, as `holder`.
    pub fn acquire_lease(
        &self,
        task: &str,
        attempt: &str,
        holder: &str,
        ttl: Duration,
    ) -> Result<Lease> {
        check_task_id(task)?;
        check_attempt(attempt)?;
        let key = self.lease_key_of(task, attempt);
        let sealer = self.write_sealer()?;
        let skew = self.settings.skew.as_millis() as u64;
        for _ in 0..4 {
            let now = self.clock.now();
            let mut record = LeaseRecord {
                task: task.to_owned(),
                attempt: attempt.to_owned(),
                holder: holder.to_owned(),
                device: self.settings.device.clone(),
                epoch: 1,
                acquired_ms: now,
                renewed_ms: now,
                expires_ms: now + ttl.as_millis() as u64,
            };
            let written = match self.read(&key) {
                Err(e) if e.is(Kind::NotFound) => self
                    .store
                    .put_if_absent(&key, &sealer.seal(&key, &serde_json::to_vec(&record)?)?),
                Err(e) => return Err(e),
                Ok((plain, generation)) => {
                    let held: LeaseRecord = serde_json::from_slice(&plain)?;
                    if held.holder == holder {
                        record.epoch = held.epoch;
                        record.acquired_ms = held.acquired_ms;
                    } else if held.expires_ms + skew <= now {
                        record.epoch = held.epoch + 1;
                    } else {
                        return Err(Error::new(
                            Kind::LeaseHeld,
                            format!(
                                "{task} {attempt} is run by {} on {} for {} more seconds",
                                held.holder,
                                held.device,
                                (held.expires_ms + skew - now).div_ceil(1000)
                            ),
                        ));
                    }
                    self.store.put_if_match(
                        &key,
                        &sealer.seal(&key, &serde_json::to_vec(&record)?)?,
                        &generation,
                    )
                }
            };
            match written {
                Ok(generation) => {
                    return Ok(Lease {
                        key,
                        record,
                        generation,
                        ttl,
                    })
                }
                // Someone else wrote between our read and write: look again.
                Err(e) if e.is(Kind::Precondition) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::new(
            Kind::LeaseHeld,
            format!("{task} {attempt}: the lease kept changing hands"),
        ))
    }

    /// Extend a held lease. [`Kind::LeaseHeld`] when it was lost.
    pub fn renew_lease(&self, lease: &mut Lease) -> Result<()> {
        let now = self.clock.now();
        let mut record = lease.record.clone();
        record.renewed_ms = now;
        record.expires_ms = now + lease.ttl.as_millis() as u64;
        let sealer = self.write_sealer()?;
        match self.store.put_if_match(
            &lease.key,
            &sealer.seal(&lease.key, &serde_json::to_vec(&record)?)?,
            &lease.generation,
        ) {
            Ok(generation) => {
                lease.generation = generation;
                lease.record = record;
                Ok(())
            }
            Err(e) if e.is(Kind::Precondition) => Err(Error::new(
                Kind::LeaseHeld,
                format!(
                    "the lease on {} {} was lost",
                    lease.record.task, lease.record.attempt
                ),
            )),
            Err(e) => Err(e),
        }
    }

    /// Give a held lease up. A lease already lost is left to its new
    /// holder.
    pub fn release_lease(&self, lease: Lease) -> Result<()> {
        match self.store.delete_if_match(&lease.key, &lease.generation) {
            Ok(()) => Ok(()),
            Err(e) if e.is(Kind::Precondition) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

struct Stop {
    stopped: Mutex<bool>,
    wake: Condvar,
}

/// A lease renewed from a thread of its own every third of its time, and
/// released when dropped; registered in a service registry while held.
pub struct LeaseKeeper {
    remote: Arc<Remote>,
    lease: Arc<Mutex<Option<Lease>>>,
    lost: Arc<AtomicBool>,
    reason: Arc<Mutex<Option<String>>>,
    stop: Arc<Stop>,
    thread: Option<JoinHandle<()>>,
    registration: Option<Registration>,
}

impl LeaseKeeper {
    /// Acquire and keep a lease. With `registry`, the lease is registered
    /// as a `sync_lease` service for as long as it is held.
    pub fn start(
        remote: Arc<Remote>,
        task: &str,
        attempt: &str,
        holder: &str,
        ttl: Duration,
        registry: Option<Arc<dyn ServiceStore>>,
    ) -> Result<LeaseKeeper> {
        let lease = remote.acquire_lease(task, attempt, holder, ttl)?;
        let registration = match registry {
            Some(store) => {
                let service = Service::new(KIND_SYNC_LEASE, ServiceOwner::this_process())
                    .with("task", Capability::from(task))
                    .with("attempt", Capability::from(attempt))
                    .with("remote", Capability::from(remote.url()))
                    .with("epoch", Capability::Number(lease.epoch() as i64));
                Some(
                    Registration::start(store, service, ttl, remote.clock().clone())
                        .map_err(|e| Error::local(format!("registering the lease: {e}")))?,
                )
            }
            None => None,
        };
        let lease = Arc::new(Mutex::new(Some(lease)));
        let lost = Arc::new(AtomicBool::new(false));
        let reason = Arc::new(Mutex::new(None));
        let stop = Arc::new(Stop {
            stopped: Mutex::new(false),
            wake: Condvar::new(),
        });
        let thread = {
            let (remote, lease, lost, reason, stop) = (
                remote.clone(),
                lease.clone(),
                lost.clone(),
                reason.clone(),
                stop.clone(),
            );
            std::thread::Builder::new()
                .name("by-sync-lease".into())
                .spawn(move || loop {
                    let guard = stop.stopped.lock().unwrap_or_else(|e| e.into_inner());
                    let (guard, _) = stop
                        .wake
                        .wait_timeout(guard, ttl / 3)
                        .unwrap_or_else(|e| e.into_inner());
                    if *guard {
                        return;
                    }
                    drop(guard);
                    let mut slot = lease.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(held) = slot.as_mut() {
                        match remote.renew_lease(held) {
                            Ok(()) => {}
                            Err(e) if e.is(Kind::LeaseHeld) => {
                                record_loss(&remote, held, &e, &lost, &reason);
                                *slot = None;
                                return;
                            }
                            // A transient failure: try again next time.
                            Err(_) => {}
                        }
                    }
                })
                .map_err(|e| Error::local(format!("lease thread: {e}")))?
        };
        Ok(LeaseKeeper {
            remote,
            lease,
            lost,
            reason,
            stop,
            thread: Some(thread),
            registration,
        })
    }

    /// Whether the lease was lost (another runner took it after a renewal
    /// failed to land in time). A runner that lost its lease stops.
    pub fn lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Why the lease was lost (and who holds it now, when the remote says),
    /// once it was.
    pub fn lost_reason(&self) -> Option<String> {
        if !self.lost() {
            return None;
        }
        self.reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .or_else(|| Some("the lease was lost".into()))
    }

    pub fn record(&self) -> Option<LeaseRecord> {
        self.lease
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|l| l.record.clone())
    }

    /// Renew now.
    pub fn renew(&self) -> Result<()> {
        let mut slot = self.lease.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_mut() {
            Some(lease) => self.remote.renew_lease(lease).inspect_err(|e| {
                if e.is(Kind::LeaseHeld) {
                    record_loss(&self.remote, lease, e, &self.lost, &self.reason);
                }
            }),
            None => Err(Error::new(Kind::LeaseHeld, "the lease was lost")),
        }
    }
}

impl Drop for LeaseKeeper {
    fn drop(&mut self) {
        *self.stop.stopped.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.stop.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(lease) = self.lease.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = self.remote.release_lease(lease);
        }
        drop(self.registration.take());
    }
}

/// Record that `lease` was lost, naming who holds it now when the remote
/// says.
fn record_loss(
    remote: &Remote,
    lease: &Lease,
    error: &Error,
    lost: &AtomicBool,
    reason: &Mutex<Option<String>>,
) {
    let now = match remote.lease_state(&lease.record.task, &lease.record.attempt) {
        Ok(Some(held)) => format!(
            "; {} on {} holds it now (epoch {})",
            held.holder, held.device, held.epoch
        ),
        _ => String::new(),
    };
    *reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("{}{now}", error.message));
    lost.store(true, Ordering::SeqCst);
}

/// A unique holder name for this process.
pub fn holder_for_this_process(device: &str) -> String {
    let (host, pid, _) = branchyard::process_identity();
    let nonce = hex::encode(crate::util::random_bytes(4).unwrap_or_default());
    format!(
        "{device}:{}:{pid}:{nonce}",
        crate::engine::sanitize_device(&host)
    )
}
