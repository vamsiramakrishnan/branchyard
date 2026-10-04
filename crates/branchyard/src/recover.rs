//! Startup reconciliation: turns whose engine stopped.
//!
//! A branch needs recovery when its lease is held by an engine that is no
//! longer running on this host, or whose lease expired, or when its record
//! says `running` and no engine holds its lease at all (a record from an
//! earlier version). Recovery takes the lease over with a new generation,
//! so the stopped engine's writes are fenced from then on, and then:
//!
//! 1. kills the turn's harness process group, only if its leader's pid and
//!    start time still match what was recorded on this host and boot, and
//!    deletes the Substrate actor the turn journaled, if it still exists;
//! 2. reads the turn's journal: a recorded `turn_end` is finished as the
//!    engine would have (the snapshot too, unless it was recorded); a
//!    submitted prompt with no recorded end becomes `interrupted`, its
//!    outcome unknown; a turn that never submitted its prompt becomes
//!    `interrupted` and says the turn never ran. Nothing is ever submitted
//!    again;
//! 3. records an [`Activity::Recovered`] and the final status, and releases
//!    the lease.
//!
//! It also frees names reserved by an engine that stopped before it created
//! the branch: gone from this host, or reserved longer ago than
//! [`crate::state::RESERVATION_TTL`].

use std::collections::BTreeSet;

use serde_json::Value;

use crate::engine::{self, Driven, End, STEP_START, STEP_SUBMIT, STEP_TURN_END};
use crate::record::{self, Recorder};
use crate::state::{Lease, LeaseRow, Record, Taken, LEASE_TTL};
use crate::{
    placement, proc, Activity, BranchStatus, Error, Event, NativeSession, RecordedEvent, Recovery,
    Yard,
};
use branchyard_support::time::now_ms;

/// Recover every branch that needs it. A branch that cannot be recovered
/// does not stop the others; the first such error is returned after all
/// were tried.
pub(crate) fn all(yard: &Yard) -> Result<Vec<Recovery>, Error> {
    let store = yard.store();
    let now = now_ms();
    let mut recovered = Vec::new();
    let mut failed = None;
    let leases = store.backend().leases()?;
    for row in &leases {
        if let Some(why) = row.stale(now) {
            match lease(yard, row, &why) {
                Ok(done) => recovered.extend(done),
                Err(error) => failed = failed.or(Some(error)),
            }
        }
    }
    let held: BTreeSet<&str> = leases.iter().map(|row| row.branch.as_str()).collect();
    for record in store.list()? {
        if record.info.status == BranchStatus::Running && !held.contains(&*record.info.name) {
            match unowned(yard, record) {
                Ok(done) => recovered.extend(done),
                Err(error) => failed = failed.or(Some(error)),
            }
        }
    }
    // Names reserved by an engine that stopped before creating the branch.
    for row in store.backend().reservations()? {
        if row.stale(now).is_some() {
            if let Err(error) = store.backend().reclaim(&row) {
                failed = failed.or(Some(error));
            }
        }
    }
    // Warm pool slots a stopped process was making or claiming, and
    // directories in the pool with no record.
    crate::pool::reclaim(yard);
    // Services whose owner stopped: what Branchyard started for them. A
    // registry that cannot be read never keeps the repository from
    // opening; `by services gc` says why.
    if yard.has_services() {
        let _ = yard.reclaim_services();
    }
    // Effects whose turn stopped between writing the ledger and recording
    // the gateway's answer: unknown, for reconciliation, never retried.
    if let Err(error) = crate::effects::reconcile::orphans(yard, now) {
        failed = failed.or(Some(error));
    }
    match failed {
        Some(error) => Err(error),
        None => Ok(recovered),
    }
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

/// Settle `name` if no live engine is running its turn: a lease a stopped
/// engine left, or a `running` record no engine holds a lease for. Nothing
/// happens while a live engine, in any process, holds its lease.
pub(crate) fn settle(yard: &Yard, name: &str) -> Result<(), Error> {
    let store = yard.store();
    let leases = store.backend().leases()?;
    match leases.iter().find(|row| row.branch == name) {
        Some(row) => {
            if let Some(why) = row.stale(now_ms()) {
                lease(yard, row, &why)?;
            }
        }
        None => {
            let record = match store.read(name) {
                Ok(record) => record,
                // Removed meanwhile: nothing is running.
                Err(Error::UnknownBranch(_)) => return Ok(()),
                Err(error) => return Err(error),
            };
            if record.info.status == BranchStatus::Running {
                unowned(yard, record)?;
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
    // A rewind is finished from its journaled intent, never left half-done.
    if let Some(rewind) = step(crate::checkpoint::STEP_REWIND) {
        let recovery = crate::checkpoint::recover(yard, lease, record, &rewind.intent, why)?;
        crate::graph::settled(yard, &row.branch, None);
        return Ok(Some(recovery));
    }
    // What carries the start's marker: a harness spawned just before the
    // engine stopped, whose pid was never recorded, and anything that left
    // the harness's process group.
    // The workspace setup's commands carry a marker of their own.
    for started in [step(STEP_START), step(crate::workspace::STEP_SETUP)]
        .into_iter()
        .flatten()
    {
        let here = started.intent.get("host").and_then(Value::as_str) == Some(proc::host());
        if let (true, Some(marker)) = (here, started.intent.get("spawn").and_then(Value::as_str)) {
            for pid in proc::kill_marked(marker) {
                if !killed.contains(&pid) {
                    killed.push(pid);
                }
            }
        }
    }
    // Setup cut short: the record still says it is not ready, so the
    // branch's next turn runs it again from the start.
    let setup = match step(crate::workspace::STEP_SETUP) {
        Some(s) if s.outcome.is_none() => {
            "; its workspace setup was cut short and runs again, from the start, before its \
             next turn"
        }
        _ => "",
    };
    let parked = step(crate::snapshots::STEP_PARK).and_then(|s| s.outcome.clone());
    let mut sandbox = step(placement::STEP_SANDBOX)
        .and_then(|s| placement::recover(yard, &record, &s.intent, parked.as_ref()))
        .map(|done| format!("; {done}"))
        .unwrap_or_default();
    if let Some(done) =
        crate::snapshots::recover_steps(yard, &record, step(crate::snapshots::STEP_SNAPSHOT))
    {
        sandbox.push_str(&format!("; {done}"));
    }
    if let Some(done) =
        crate::environments::recover_step(yard, step(crate::environments::STEP_ENVIRONMENT))
    {
        sandbox.push_str(&format!("; {done}"));
    }
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
            format!("{why}; the turn had ended, and recovery recorded its result{late}{sandbox}"),
        ),
        None if step(STEP_SUBMIT).is_some() => {
            let reason = format!(
                "{why}; the prompt had been submitted and the turn's outcome is unknown. \
                 It was not submitted again{late}{sandbox}"
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
            let reason = format!("{why} before {started}; the turn never ran{setup}{sandbox}");
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
        cost: turn_cost(yard, &row.branch),
        metered: metered_turn(yard, &row.branch),
    };
    if let Err(error) = engine::conclude(yard, &prompt, &fence, &mut record, &mut recorder, driven)
    {
        record.info.status = BranchStatus::Failed {
            reason: format!("recovery could not finish the turn: {error}"),
        };
    }
    recorder.finish(lease, &record)?;
    // What waits for it is blocked now, or, if it had settled, may start.
    crate::graph::settled(yard, &row.branch, None);
    Ok(Some(Recovery {
        branch: row.branch.clone(),
        status: record.info.status,
        reason,
        killed,
    }))
}

/// The highest cumulative cost the harness reported since the turn's prompt,
/// as the engine would have kept it: spend is recorded whether or not the
/// turn's outcome is known.
fn turn_cost(yard: &Yard, name: &str) -> Option<f64> {
    cost_since_prompt(&record::read(&yard.store(), name).ok()?)
}

/// What the turn's calls through the model gateway cost, from the calls
/// it recorded after its gateway started; `None` when it had none.
fn metered_turn(yard: &Yard, name: &str) -> Option<f64> {
    let events = record::read(&yard.store(), name).ok()?;
    let start = events.iter().rposition(|event| {
        matches!(&event.activity, Activity::Model(m)
            if matches!(m.as_ref(), crate::models::ModelActivity::Gateway { .. }))
    })?;
    Some(
        events[start + 1..]
            .iter()
            .filter_map(|event| match &event.activity {
                Activity::Model(m) => match m.as_ref() {
                    crate::models::ModelActivity::Call(call) => call.cost_usd,
                    _ => None,
                },
                _ => None,
            })
            .sum(),
    )
}

fn cost_since_prompt(events: &[RecordedEvent]) -> Option<f64> {
    let start = events
        .iter()
        .rposition(|event| matches!(event.activity, Activity::Prompt(_)))
        .map_or(0, |i| i + 1);
    events[start..]
        .iter()
        .filter_map(|event| match &event.activity {
            Activity::Harness(Event::UsageObserved { usage, .. }) if usage.cumulative => {
                usage.cost_usd
            }
            _ => None,
        })
        .reduce(f64::max)
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
    let reason = "its record says running but no engine holds its lease: it ran under an \
                  earlier version of Branchyard, or its engine failed without settling it. What \
                  its last turn did is unknown; nothing was submitted again"
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
    crate::graph::settled(yard, &record.info.name, None);
    Ok(Some(Recovery {
        branch: record.info.name,
        status: record.info.status,
        reason,
        killed: Vec::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_harness::Usage;

    fn usage(cumulative: bool, cost: f64) -> RecordedEvent {
        RecordedEvent {
            at_ms: 0,
            activity: Activity::Harness(Event::UsageObserved {
                turn: None,
                usage: Usage {
                    cumulative,
                    cost_usd: Some(cost),
                    ..Usage::default()
                },
            }),
        }
    }

    fn prompt() -> RecordedEvent {
        RecordedEvent {
            at_ms: 0,
            activity: Activity::Prompt("p".into()),
        }
    }

    #[test]
    fn a_recovered_turn_keeps_the_highest_cumulative_cost_since_its_prompt() {
        let events = [
            prompt(),
            usage(true, 5.0),
            prompt(),
            usage(true, 1.0),
            usage(false, 9.0),
            usage(true, 2.5),
            usage(true, 2.0),
        ];
        assert_eq!(cost_since_prompt(&events), Some(2.5));
        assert_eq!(cost_since_prompt(&events[..3]), None, "none reported yet");
        assert_eq!(cost_since_prompt(&[]), None);
    }
}
