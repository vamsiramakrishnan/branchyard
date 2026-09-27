//! Input for a running turn: [`crate::Branch::steer`].
//!
//! A steer is queued in the durable store, bound to the turn that holds
//! the branch's lease, exactly as a cancel is: the engine running that
//! turn, in this process or another, polls for it, writes it to the
//! harness through [`branchyard_harness::Driver::steer`], and records what
//! became of it. It is never delivered to a later turn, and never turned
//! into an interrupt: a profile that cannot take input mid-turn is refused
//! up front.

use std::time::Duration;

use branchyard_harness::profiles::{self, Profile};
use branchyard_harness::Rejected;

use crate::recover;
use crate::state::{SteerRow, Store};
use crate::{Error, Steer, SteerState, Yard};

/// The most text one steer carries.
pub(crate) const MAX_TEXT: usize = 64 * 1024;

/// Why `profile` cannot take input during a running turn, if it cannot.
pub(crate) fn refusal(profile: &Profile) -> Option<String> {
    let mut driver = profile.driver();
    if driver.capabilities().steer {
        return None;
    }
    Some(match driver.steer("") {
        Err(Rejected::Unsupported(why)) => why,
        _ => "its driver does not offer steering".into(),
    })
}

/// Queue `text` from `by` for `name`'s running turn.
pub(crate) fn request(yard: &Yard, name: &str, text: &str, by: &str) -> Result<Steer, Error> {
    if text.trim().is_empty() {
        return Err(Error::Denied("steered input needs some text".into()));
    }
    if text.len() > MAX_TEXT {
        return Err(Error::Denied(format!(
            "steered input is limited to {MAX_TEXT} bytes"
        )));
    }
    let store = yard.store();
    let record = store.read(name)?;
    let profile = profiles::by_id(&record.info.profile)
        .ok_or_else(|| Error::UnknownHarness(record.info.profile.clone()))?;
    if let Some(why) = refusal(profile) {
        return Err(Error::Unsupported(format!(
            "{} cannot take input during a running turn: {why}",
            profile.id
        )));
    }
    // A lease its engine left behind is no running turn.
    recover::stale(yard, name)?;
    let id = store
        .backend()
        .request_steer(name, by, text)?
        .ok_or_else(|| Error::NotRunning(name.to_owned()))?;
    state(&store, name, id)
}

/// A steer as it stands. One still pending whose turn no longer holds the
/// lease was never delivered, and is reported refused.
pub(crate) fn state(store: &Store, name: &str, id: u64) -> Result<Steer, Error> {
    let row = store
        .backend()
        .steer(name, id)?
        .ok_or_else(|| Error::State(format!("{name} has no steered input {id}")))?;
    let mut steer = public(row.clone());
    if steer.state == SteerState::Pending {
        let held = store
            .backend()
            .leases()?
            .iter()
            .any(|l| l.branch == name && l.turn == row.turn && l.owner.is_some());
        if !held {
            steer.state = SteerState::Refused {
                reason: "the turn ended before its engine delivered it".into(),
            };
        }
    }
    Ok(steer)
}

/// Wait up to `timeout` for the steer to leave [`SteerState::Pending`].
pub(crate) fn wait(store: &Store, name: &str, id: u64, timeout: Duration) -> Result<Steer, Error> {
    let found = store.wait(timeout, || {
        let steer = state(store, name, id)?;
        Ok((steer.state != SteerState::Pending).then_some(steer))
    })?;
    match found {
        Some(steer) => Ok(steer),
        None => state(store, name, id),
    }
}

pub(crate) fn public(row: SteerRow) -> Steer {
    Steer {
        id: row.id,
        branch: row.branch,
        by: row.by,
        text: row.text,
        requested_at_ms: row.requested_ms,
        state: row.state,
    }
}
