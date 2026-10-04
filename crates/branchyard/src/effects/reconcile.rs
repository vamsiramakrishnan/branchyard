//! Reconciliation: entries whose outcome is not known are settled by
//! asking the upstream, never by calling again.
//!
//! - A `begun` entry whose turn is no longer running (its engine stopped
//!   between writing the entry and recording the answer) becomes
//!   `unknown`. This needs no network, so recovery does it.
//! - An `unknown` entry with a lookup is looked up through the gateway,
//!   under the branch's grant: the call the gateway's report named (a
//!   failed call still reports one), or, for an answer lost entirely, the
//!   lookup the tool's contract declares, resolved when the call was made
//!   from its arguments and its id (the idempotency key). An answer is
//!   `confirmed`; `not_found`, or nothing, is `failed`. Without a lookup,
//!   or when the lookup cannot answer, it stays `unknown` and is shown to
//!   the person; the gateway's audit log can settle it too
//!   (`by effects reconcile --audit`).
//! - A `confirmed` entry whose inverse's deadline passed, with no
//!   compensation left, is `expired`.
//!
//! Run on recovery (the first step), by `by effects reconcile`, by the
//! gateway's supervisor and the server on a timer.

use serde::{Deserialize, Serialize};

use super::ask::note;
use super::mcp::{CallError, Client};
use super::{EffectActivity, EffectEntry, EffectMove, EffectState};
use crate::{Error, Yard};
use branchyard_support::time::now_ms;

/// What one reconciliation did.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reconciled {
    /// Entries settled, with the state each reached.
    pub settled: Vec<(String, EffectState)>,
    /// Entries still `unknown`, with why.
    pub unknown: Vec<(String, String)>,
}

/// Whether `entry`'s turn may still be running: its branch's lease is held
/// by a live engine and the running turn is the entry's.
fn turn_running(yard: &Yard, entry: &EffectEntry, now: u64) -> bool {
    let store = yard.store();
    let Ok(record) = store.read(&entry.branch) else {
        return false;
    };
    let held = store
        .backend()
        .leases()
        .unwrap_or_default()
        .into_iter()
        .any(|l| l.branch == entry.branch && l.owner.is_some() && l.stale(now).is_none());
    held && record.info.turns + 1 == entry.turn
}

/// Move `begun` entries whose turn is not running to `unknown`. No network.
pub(crate) fn orphans(yard: &Yard, now: u64) -> Result<Vec<String>, Error> {
    let ledger = yard.store();
    let mut moved = Vec::new();
    for entry in ledger.effects().effects(None)? {
        if entry.state != EffectState::Begun || turn_running(yard, &entry, now) {
            continue;
        }
        let change = EffectMove::to(EffectState::Unknown).detail(
            "its turn ended before the call's answer was recorded; it may or may not have happened",
        );
        if let Some(done) =
            ledger
                .effects()
                .move_effect(&entry.id, &[EffectState::Begun], &change, now)?
        {
            note(yard, &done.branch, EffectActivity::of(&done));
            moved.push(done.id);
        }
    }
    Ok(moved)
}

/// Reconcile the whole ledger at `now`.
pub(crate) fn run(yard: &Yard, now: u64) -> Result<Reconciled, Error> {
    let mut report = Reconciled::default();
    orphans(yard, now)?;
    let ledger = yard.store();
    for entry in ledger.effects().effects(None)? {
        match entry.state {
            EffectState::Unknown => match lookup(yard, &entry) {
                Ok(Some(change)) => {
                    if let Some(done) = ledger.effects().move_effect(
                        &entry.id,
                        &[EffectState::Unknown],
                        &change,
                        now,
                    )? {
                        note(yard, &done.branch, EffectActivity::of(&done));
                        report.settled.push((done.id, done.state));
                    }
                }
                Ok(None) => report.unknown.push((
                    entry.id.clone(),
                    "its operation declares no lookup; check upstream and decide".into(),
                )),
                Err(why) => report.unknown.push((entry.id.clone(), why)),
            },
            EffectState::Confirmed
                if entry.expired_at(now) && entry.compensation_at(now).is_none() =>
            {
                let change =
                    EffectMove::to(EffectState::Expired).detail("its undo's deadline passed");
                if let Some(done) = ledger.effects().move_effect(
                    &entry.id,
                    &[EffectState::Confirmed],
                    &change,
                    now,
                )? {
                    note(yard, &done.branch, EffectActivity::of(&done));
                    report.settled.push((done.id, done.state));
                }
            }
            _ => {}
        }
    }
    Ok(report)
}

/// Ask the upstream whether `entry`'s call happened: `None` when there is
/// no lookup to ask.
fn lookup(yard: &Yard, entry: &EffectEntry) -> Result<Option<EffectMove>, String> {
    let Some(lookup) = &entry.lookup else {
        return Ok(None);
    };
    let record = yard
        .store()
        .read(&entry.branch)
        .map_err(|e| format!("its branch is gone ({e}); check upstream and decide"))?;
    let (gateway, token) = crate::connectors::branch_token(yard, &record, entry.turn)?;
    let called = Client::new(&gateway.url, token)
        .follow_up(&lookup.tool, &lookup.arguments, None)
        .map_err(|e: CallError| format!("the lookup: {e}"))?;
    Ok(Some(match called.found() {
        Some(true) => EffectMove::to(EffectState::Confirmed).detail(format!(
            "the lookup ({}) found it: the call happened",
            lookup.operation
        )),
        Some(false) => EffectMove::to(EffectState::Failed).detail(format!(
            "the lookup ({}) did not find it: the call did not happen",
            lookup.operation
        )),
        None => return Err(format!("the lookup failed: {}", called.answer())),
    }))
}

/// [`run`] now.
pub(crate) fn now(yard: &Yard) -> Result<Reconciled, Error> {
    run(yard, now_ms())
}
