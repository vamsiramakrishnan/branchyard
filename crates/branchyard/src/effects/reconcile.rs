//! Reconciliation: entries whose outcome is not known are settled by
//! asking the upstream, never by calling again.
//!
//! - A `begun` entry whose turn is no longer running (its engine stopped
//!   between writing the entry and recording the answer) becomes
//!   `unknown`. This needs no network, so recovery does it.
//! - An `unknown` entry whose operation declared a lookup is looked up
//!   through the gateway, under the branch's grant, by its id (the
//!   idempotency key): found is `confirmed` (with the undo the gateway
//!   gives), not found is `failed`. Without a lookup, or when the lookup
//!   cannot answer, it stays `unknown` and is shown to the person.
//! - A `confirmed` entry whose inverse's deadline passed is `expired`.
//!
//! Run on recovery (the first step), by `by effects reconcile`, by the
//! gateway's supervisor and the server on a timer.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::ask::note;
use super::mcp::{CallError, Client};
use super::proxy::described;
use super::{EffectActivity, EffectEntry, EffectMove, EffectState};
use crate::state::now_ms;
use crate::{Error, Yard};

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
            EffectState::Confirmed if entry.expired_at(now) => {
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

/// Ask the upstream whether `entry`'s call happened: `None` when it
/// declares no lookup.
fn lookup(yard: &Yard, entry: &EffectEntry) -> Result<Option<EffectMove>, String> {
    let Some(lookup) = &entry.lookup else {
        return Ok(None);
    };
    let record = yard
        .store()
        .read(&entry.branch)
        .map_err(|e| format!("its branch is gone ({e}); check upstream and decide"))?;
    let (gateway, token) = crate::connectors::branch_token(yard, &record, entry.turn)?;
    let mut arguments = match &lookup.arguments {
        Value::Object(map) => Value::Object(map.clone()),
        _ => json!({}),
    };
    arguments["idempotency_key"] = json!(entry.id);
    let tool = format!("{}__{}", entry.connector, lookup.operation);
    let called = Client::new(&gateway.url, token)
        .call(&tool, &arguments, &json!({}), None)
        .map_err(|e: CallError| format!("the lookup: {e}"))?;
    if !called.ok() {
        return Err(format!("the lookup failed: {}", called.answer()));
    }
    let found = called.meta.as_ref().and_then(|m| m.found).or_else(|| {
        called
            .result
            .as_ref()
            .and_then(|r| r.get("structuredContent"))
            .and_then(|s| s.get("found"))
            .and_then(Value::as_bool)
    });
    Ok(Some(match found {
        Some(true) => {
            let found = called.meta.as_ref().and_then(|m| m.found_effect.as_deref());
            let mut change = match found {
                Some(meta) => described(meta, EffectState::Confirmed),
                None => EffectMove::to(EffectState::Confirmed),
            };
            change.detail = Some("the lookup found it: the call happened".into());
            change
        }
        Some(false) => EffectMove::to(EffectState::Failed)
            .detail("the lookup did not find it: the call did not happen"),
        None => return Err("the lookup did not say whether it found the call".into()),
    }))
}

/// [`run`] now.
pub(crate) fn now(yard: &Yard) -> Result<Reconciled, Error> {
    run(yard, now_ms())
}
