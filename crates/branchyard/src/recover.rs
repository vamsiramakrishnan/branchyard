//! Startup reconciliation: turns whose engine stopped.
//!
//! A branch needs recovery when its lease is held by an engine that is no
//! longer running on this host, or whose lease expired, or when its record
//! says `running` and no engine holds its lease at all (a record from an
//! earlier version). Recovery takes the lease over with a new generation,
//! so the stopped engine's writes are fenced from then on, and then:
//!
//! 1. kills the turn's harness process group, only if its leader's pid and
//!    start time still match what was recorded on this host and boot;
//! 2. reads the turn's journal: a recorded `turn_end` is finished as the
//!    engine would have (the snapshot too, unless it was recorded); a
//!    submitted prompt with no recorded end becomes `interrupted`, its
//!    outcome unknown; a turn that never submitted its prompt becomes
//!    `interrupted` and says the turn never ran. Nothing is ever submitted
//!    again;
//! 3. records an [`Activity::Recovered`] and the final status, and releases
//!    the lease.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::engine::{self, Driven, End, STEP_START, STEP_SUBMIT, STEP_TURN_END};
use crate::record::{self, Recorder};
use crate::state::{now_ms, Lease, LeaseRow, Record, Taken, LEASE_TTL};
use crate::{proc, Activity, BranchStatus, Error, Event, NativeSession, Recovery, Yard};

/// Recover every branch that needs it.
pub(crate) fn all(yard: &Yard) -> Result<Vec<Recovery>, Error> {
    let store = yard.store();
    let now = now_ms();
    let mut recovered = Vec::new();
    let leases = store.backend().leases()?;
    for row in &leases {
        if let Some(why) = row.stale(now) {
            recovered.extend(lease(yard, row, &why)?);
        }
    }
    let held: BTreeSet<&str> = leases.iter().map(|row| row.branch.as_str()).collect();
    for record in store.list()? {
        if record.info.status == BranchStatus::Running && !held.contains(&*record.info.name) {
            recovered.extend(unowned(yard, record)?);
        }
    }
    Ok(recovered)
}

/// Recover `name` first if a stopped engine holds its lease, so a new turn
/// can start from what is known.
pub(crate) fn stale(yard: &Yard, name: &str) -> Result<(), Error> {
    let store = yard.store();
    let now = now_ms();
    for row in store.backend().leases()? {
        if row.branch == name {
            if let Some(why) = row.stale(now) {
                lease(yard, &row, &why)?;
            }
        }
    }
    Ok(())
}

/// Take over `row` and settle its turn.
fn lease(yard: &Yard, row: &LeaseRow, why: &str) -> Result<Option<Recovery>, Error> {
    let store = yard.store();
    let Some(fence) = store.backend().take_over(row, store.owner(), LEASE_TTL)? else {
        // Another engine recovered or renewed it first.
        return Ok(None);
    };
    let lease = Lease::new(store.clone(), fence.clone());
    let mut record = match store.read(&row.branch) {
        Ok(record) => record,
        Err(Error::UnknownBranch(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut killed = Vec::new();
    for process in store.backend().processes(&row.branch, row.turn)? {
        if process.host == proc::host() {
            killed.extend(proc::kill_group(process.pgid, &process.start));
        }
    }
    let steps = store.backend().steps(&row.branch, row.turn)?;
    let step = |name: &str| steps.iter().find(|s| s.step == name);
    let ended = step(STEP_TURN_END).and_then(|s| {
        let end = serde_json::from_value::<End>(s.outcome.clone()?).ok()?;
        let submitted = s.intent.get("submitted").and_then(Value::as_bool)?;
        Some((end, submitted))
    });
    let prompt = step(STEP_SUBMIT)
        .and_then(|s| s.intent.get("prompt")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| record.info.prompt.clone());
    let late = row
        .deadline_ms
        .filter(|deadline| *deadline <= now_ms())
        .map(|_| "; its max_duration deadline had passed")
        .unwrap_or_default();
    let (end, submitted, reason) = match ended {
        Some((end, submitted)) => (
            end,
            submitted,
            format!("{why}; the turn had ended, and recovery recorded its result{late}"),
        ),
        None if step(STEP_SUBMIT).is_some() => {
            let reason = format!(
                "{why}; the prompt had been submitted and the turn's outcome is unknown. \
                 It was not submitted again{late}"
            );
            (
                End::Lost {
                    reason: reason.clone(),
                },
                true,
                reason,
            )
        }
        None => {
            let started = match step(STEP_START) {
                Some(_) => "the prompt was submitted",
                None => "the harness was started",
            };
            let reason = format!("{why} before {started}; the turn never ran");
            (
                End::Lost {
                    reason: reason.clone(),
                },
                false,
                reason,
            )
        }
    };
    let mut recorder = Recorder::fenced(&store, &fence, None);
    recorder.record(Activity::Recovered {
        reason: reason.clone(),
        killed: killed.clone(),
    })?;
    let driven = Driven {
        end,
        submitted,
        session: last_session(yard, &row.branch),
        cost: None,
    };
    if let Err(error) = engine::conclude(yard, &prompt, &fence, &mut record, &mut recorder, driven)
    {
        record.info.status = BranchStatus::Failed {
            reason: format!("recovery could not finish the turn: {error}"),
        };
    }
    recorder.finish(lease, &record)?;
    Ok(Some(Recovery {
        branch: row.branch.clone(),
        status: record.info.status,
        reason,
        killed,
    }))
}

/// The session the harness last reported, which a resumed send needs.
fn last_session(yard: &Yard, name: &str) -> Option<NativeSession> {
    record::read(&yard.store(), name)
        .ok()?
        .into_iter()
        .rev()
        .find_map(|event| match event.activity {
            Activity::Harness(Event::SessionStarted { session, .. }) => Some(session),
            _ => None,
        })
}

/// A `running` record no engine holds a lease for.
fn unowned(yard: &Yard, mut record: Record) -> Result<Option<Recovery>, Error> {
    let store = yard.store();
    let reason = "no engine holds this branch's lease: it was running under an earlier version \
                  of Branchyard, or its engine stopped before taking one. What its last turn did \
                  is unknown; it was not submitted again"
        .to_owned();
    record.info.status = BranchStatus::Interrupted;
    if record.info.session.is_none() {
        record.info.session = last_session(yard, &record.info.name).map(|s| s.to_string());
    }
    let lease = match store.acquire(&record) {
        Ok(Taken::Granted(lease)) => lease,
        // An engine started a turn on it meanwhile.
        Ok(Taken::Stale) | Err(Error::Running(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let fence = lease.fence().clone();
    let mut recorder = Recorder::fenced(&store, &fence, None);
    recorder.record(Activity::Recovered {
        reason: reason.clone(),
        killed: Vec::new(),
    })?;
    recorder.finish(lease, &record)?;
    Ok(Some(Recovery {
        branch: record.info.name,
        status: record.info.status,
        reason,
        killed: Vec::new(),
    }))
}
