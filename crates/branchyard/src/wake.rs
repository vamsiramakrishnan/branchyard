//! Park and wake: a delegating branch whose turn ends while children it
//! delegated still run.
//!
//! Harnesses such as Claude Code move a long wait into a background task
//! and end their turn ("I'll get a notification when they finish"). A turn
//! that ends is not the end of the branch's work then: when the turn of a
//! branch that may delegate ends `ready` or `no_changes` while one of its
//! descendants is `running`, `waiting` for prerequisites or itself
//! waiting on its children, the branch is recorded
//! [`BranchStatus::WaitingOnChildren`] ([`park`]) instead of finished.
//!
//! When the last of them settles, its next turn starts on its own
//! ([`look`]), with a prompt that says what each child did and what the
//! parent can do next ([`summary`]). It waits for all of them rather than
//! the first, so one wake carries every result and a parent with several
//! children is not woken once per child; a parent that wants to act on
//! each child as it finishes waits in its turn instead (`by wait --any`).
//! A `blocked` descendant does not keep the parent parked, since only the
//! parent can unblock it; it is listed in the wake.
//!
//! Whichever engine settles the last child looks at its parked ancestors
//! (`crate::graph::settled` calls [`settled`]), in any process: the state
//! is durable, and starting the wake is a compare-and-swap from
//! `waiting_on_children` to `running` with the first lease
//! ([`crate::graph::GraphBackend::claim_if`]), so exactly one engine wakes
//! it. The wake runs under the options of the parked turn when the
//! process that ran it starts it (registered in the yard's hub), else
//! under those of the turn that settled the last child, which are the
//! parent's policy and tools; its limits are the parked turn's, stored
//! with the branch. A wait for the subtree (`by run`, `Branch::wait_subtree`)
//! wakes what it finds parked and waits for that turn too; after a crash,
//! `Yard::resume_graph` (`by graph resume`, a server's recovery tick) does.
//!
//! A wake is bounded: by the envelope's `max_wakes` (consecutive automatic
//! wakes; a turn something else starts resets the count), and by the
//! parent's cost and turn limits, checked before it starts. A parent that
//! cannot be woken ends as its parked turn did, with a warning saying why.

use std::collections::BTreeSet;

use branchyard_support::best_effort;
use serde::{Deserialize, Serialize};

use crate::delegation::{self, Limits};
use crate::projection::lock;
use crate::record::{self, Recorder};
use crate::state::{Record, Store};
use crate::{Activity, BranchStatus, Budget, Error, RecordedEvent, TaskOptions, Yard};
use branchyard_support::time::now_ms;

/// How the note a turn after a lost one starts with opens.
pub(crate) const RECOVERED_OPEN: &str = "<branchyard-recovered>";

/// Characters of each child's last message a wake prompt quotes.
const LAST_MESSAGE_MAX: usize = 600;

/// What a parked branch's turn ended with, stored with it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Parked {
    /// When its turn ended.
    pub since_ms: u64,
    /// The status its turn ended with: what it settles to if it is not
    /// woken.
    pub ended: BranchStatus,
    /// The descendants it waits on, as they were when it parked.
    pub on: Vec<String>,
    /// The limits its turns run under, for a wake started elsewhere.
    pub budget: Limits,
}

pub(crate) fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Whether a branch in `status` will still run: what keeps its parked
/// ancestors parked.
pub(crate) fn unsettled(status: &BranchStatus) -> bool {
    matches!(
        status,
        BranchStatus::Running | BranchStatus::Waiting | BranchStatus::WaitingOnChildren
    )
}

pub(crate) fn is_parked(status: &BranchStatus) -> bool {
    *status == BranchStatus::WaitingOnChildren
}

/// After a turn of `record` ended: park it when it may delegate, may be
/// woken, ended `ready` or `no_changes`, and a descendant still runs.
/// `budget` is the turn's own, stored for a wake started elsewhere.
pub(crate) fn park(
    yard: &Yard,
    record: &mut Record,
    recorder: &mut Recorder,
    budget: &Budget,
) -> Result<(), Error> {
    if !matches!(
        record.info.status,
        BranchStatus::Ready | BranchStatus::NoChanges
    ) {
        return Ok(());
    }
    let Some(grant) = record.grant.as_ref().filter(|g| g.can_spawn()) else {
        return Ok(());
    };
    let max_wakes = grant.envelope.max_wakes;
    if max_wakes == 0 {
        return Ok(());
    }
    let store = yard.store();
    let on: Vec<String> = delegation::descendants(&store, &record.info.name)?
        .into_iter()
        .filter(|info| unsettled(&info.status))
        .map(|info| info.name)
        .collect();
    if on.is_empty() {
        return Ok(());
    }
    if record.wakes >= max_wakes {
        recorder.record(Activity::Warning(format!(
            "its turn ended while {} still run, but it was woken {} times in a row, its \
             envelope's max_wakes; it is not woken again. Wait for them (`by wait`) or send it a \
             prompt",
            on.join(", "),
            record.wakes
        )))?;
        return Ok(());
    }
    recorder.record(Activity::Delegation {
        tool: "wait".into(),
        branch: record.info.name.clone(),
        outcome: format!(
            "its turn ended while {} still run; it waits on them, and its next turn starts \
             when they settle (automatic wake {} of at most {max_wakes})",
            on.join(", "),
            record.wakes + 1
        ),
        refused: false,
    })?;
    record.parked = Some(Parked {
        since_ms: now_ms(),
        ended: record.info.status.clone(),
        on,
        budget: Limits {
            max_usd: budget.max_usd,
            max_turns: budget.max_turns,
            max_duration_ms: budget
                .max_duration
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        },
    });
    record.info.status = BranchStatus::WaitingOnChildren;
    Ok(())
}

/// A turn of `record` is about to be recorded parked: its wake in this
/// process runs under `options`.
pub(crate) fn remember(yard: &Yard, record: &Record, options: &TaskOptions) {
    let mut held = lock(&yard.hub.wake_options);
    match is_parked(&record.info.status) {
        true => held.insert(record.info.name.clone(), options.clone()),
        false => held.remove(&record.info.name),
    };
}

/// `name`'s state changed (a turn ended, it was removed, blocked or
/// recovered): wake each parked ancestor whose descendants have all
/// settled. `options` are the turn's that changed it, if a turn did.
/// Errors are logged: they leave the ancestor to the next look.
pub(crate) fn settled(yard: &Yard, name: &str, options: Option<&TaskOptions>) {
    let store = yard.store();
    let mut seen = BTreeSet::new();
    let mut next = match store.backend().read(name) {
        Ok(Some(record)) => record.info.parent,
        // Removed: its parent is unknown here, so every parked branch is
        // looked at.
        Ok(None) => {
            for parked in parked_branches(&store) {
                best_effort("wake a parked branch", look(yard, &parked, options));
            }
            return;
        }
        Err(_) => None,
    };
    while let Some(parent) = next {
        if !seen.insert(parent.clone()) {
            break;
        }
        let Ok(Some(record)) = store.backend().read(&parent) else {
            break;
        };
        if is_parked(&record.info.status) {
            best_effort("wake a parked branch", look(yard, &parent, options));
        }
        next = record.info.parent;
    }
}

fn parked_branches(store: &Store) -> Vec<String> {
    store
        .list()
        .map(|records| {
            records
                .into_iter()
                .filter(|r| is_parked(&r.info.status))
                .map(|r| r.info.name)
                .collect()
        })
        .unwrap_or_default()
}

/// Wake every parked branch in the repository whose descendants have
/// settled, under `options` unless this process ran its parked turn.
/// Returns those woken.
pub(crate) fn resume(yard: &Yard, options: &TaskOptions) -> Result<Vec<String>, Error> {
    let mut woken = Vec::new();
    for name in parked_branches(&yard.store()) {
        if look(yard, &name, Some(options))? {
            woken.push(name);
        }
    }
    Ok(woken)
}

/// If `name` is parked and nothing below it will still run, start its
/// next turn on a thread of this process, or settle it as its turn ended
/// when a limit forbids the wake. True when a turn started. A parked
/// branch whose wake has no options to run under here (no turn of this
/// process parked it, and none was given) is left parked.
pub(crate) fn look(yard: &Yard, name: &str, given: Option<&TaskOptions>) -> Result<bool, Error> {
    let store = yard.store();
    let Some(record) = store.backend().read(name)? else {
        return Ok(false);
    };
    if !is_parked(&record.info.status) {
        return Ok(false);
    }
    let below = delegation::descendants(&store, name)?;
    if below.iter().any(|info| unsettled(&info.status)) {
        return Ok(false);
    }
    let parked = record.parked.clone().unwrap_or_else(|| Parked {
        since_ms: now_ms(),
        ended: match record.info.candidate {
            Some(_) => BranchStatus::Ready,
            None => BranchStatus::NoChanges,
        },
        on: Vec::new(),
        budget: Limits::default(),
    });
    let budget = Budget {
        max_usd: parked.budget.max_usd,
        max_turns: parked.budget.max_turns,
        max_duration: parked
            .budget
            .max_duration_ms
            .map(std::time::Duration::from_millis),
        ..Budget::default()
    };
    let max_wakes = record
        .grant
        .as_ref()
        .map_or(0, |grant| grant.envelope.max_wakes);
    // Settled without a wake from here on: its options are not needed.
    let forget = || lock(&yard.hub.wake_options).remove(name);
    if record.wakes >= max_wakes {
        forget();
        unpark(
            &store,
            &record,
            &parked,
            format!(
                "its children settled, but it was woken {} times in a row, its envelope's \
                 max_wakes; it was not woken again",
                record.wakes
            ),
        )?;
        return Ok(false);
    }
    if let Some(limit) = crate::engine::exhausted(&store, &record, &budget) {
        forget();
        unpark(
            &store,
            &record,
            &parked,
            format!("its children settled, but its {limit} is spent; it was not woken"),
        )?;
        return Ok(false);
    }
    let held = lock(&yard.hub.wake_options).get(name).cloned();
    let options = match (held, given) {
        (Some(options), _) => options,
        (None, Some(given)) => TaskOptions {
            budget: budget.clone(),
            ..crate::graph::child_options(given)
        },
        (None, None) => return Ok(false),
    };
    let prepared = match crate::run::prepare_wake(yard, name, &options) {
        Ok(Some(prepared)) => prepared,
        // Another engine woke it, or something else started a turn.
        Ok(None) | Err(Error::Running(_)) => return Ok(false),
        Err(error) => {
            forget();
            unpark(
                &store,
                &record,
                &parked,
                format!("its children settled, but its next turn could not start: {error}"),
            )?;
            return Err(error);
        }
    };
    forget();
    let prompt = summary(&store, &record, &parked, max_wakes)?;
    delegation::start_turn(yard, prepared, prompt, options)?;
    Ok(true)
}

/// Settle a parked branch as its turn ended, with `why` on its log.
fn unpark(store: &Store, record: &Record, parked: &Parked, why: String) -> Result<(), Error> {
    let mut settled = record.clone();
    settled.info.status = parked.ended.clone();
    settled.parked = None;
    settled.wakes = 0;
    let event = RecordedEvent {
        at_ms: now_ms(),
        activity: Activity::Status(settled.info.status.clone()),
    };
    if store.graph().settle_if(&settled, &event, is_parked)? {
        store.notify();
        if let Ok(mut recorder) = Recorder::open(store, &record.info.name, None) {
            best_effort(
                "record the activity",
                recorder.record(Activity::Warning(why)),
            );
        }
    }
    Ok(())
}

/// The prompt of a wake: what each child did, and what the parent can do.
pub(crate) fn summary(
    store: &Store,
    record: &Record,
    parked: &Parked,
    max_wakes: u32,
) -> Result<String, Error> {
    let mut text = String::from("<branchyard-wake>\n");
    text.push_str(&format!(
        "Branchyard started this turn: your last turn ended while children you delegated were \
         still running, and they have all settled now (automatic wake {} of at most {max_wakes}).",
        record.wakes + 1
    ));
    text.push('\n');
    let mut others = Vec::new();
    for child in &record.info.children {
        let Ok(Some(found)) = store.backend().read(child) else {
            continue;
        };
        let info = &found.info;
        let waited = parked.on.iter().any(|name| name == child);
        if !waited {
            // Merged or set aside, there is nothing left to do with it.
            if !matches!(
                info.status,
                BranchStatus::Merged { .. } | BranchStatus::Discarded { .. }
            ) {
                others.push(format!("{child} ({})", status_text(&info.status)));
            }
            continue;
        }
        text.push_str(&format!("\n- {child}: {}", status_text(&info.status)));
        match &info.candidate {
            Some(c) => {
                text.push_str(&format!(
                    "; candidate {} file{} +{} -{} at {}",
                    c.files_changed,
                    if c.files_changed == 1 { "" } else { "s" },
                    c.insertions,
                    c.deletions,
                    &c.commit[..c.commit.len().min(10)]
                ));
            }
            None => text.push_str("; no candidate"),
        }
        match info.cost_usd {
            Some(cost) => {
                text.push_str(&format!("; cost ${cost:.2}"));
            }
            None => text.push_str("; cost unknown"),
        }
        text.push('\n');
        let events = record::read(store, child).unwrap_or_default();
        let last = delegation::last_message(&events);
        let last = last.trim();
        if !last.is_empty() {
            let count = last.chars().count();
            let quoted: String = match count > LAST_MESSAGE_MAX {
                true => {
                    let tail: String = last.chars().skip(count - LAST_MESSAGE_MAX).collect();
                    format!("…{tail}")
                }
                false => last.to_owned(),
            };
            text.push_str(&format!("  last message: {}", quoted.replace('\n', "\n  ")));
            text.push('\n');
        }
    }
    if !others.is_empty() {
        text.push_str(&format!("\nYour other children: {}.", others.join(", ")));
        text.push('\n');
    }
    text.push_str(
        "\nWhat you can do now:\n\
         - Integrate the children you want, together when they share a check: `by integrate a b` \
         (MCP: `propose_integration` with `branches`). It merges them in order, runs the check \
         once on the result and moves your branch once; a candidate your branch already contains \
         is recorded as merged.\n\
         - Continue a child: `by send <child> \"<prompt>\"`; read more with `by inspect <child>` \
         or `by events <child>`.\n\
         - Delegate more, or wait within this turn: `by wait [<child>...] [--any]`.\n\
         If you end this turn while children still run, you are woken again when they settle.\n\
         </branchyard-wake>",
    );
    Ok(text)
}

fn status_text(status: &BranchStatus) -> String {
    match status {
        BranchStatus::Failed { reason } => format!("failed: {reason}"),
        BranchStatus::Blocked { reason } => format!("blocked: {reason}"),
        BranchStatus::BudgetExceeded { limit } => format!("stopped at its {limit} limit"),
        BranchStatus::Merged { target, .. } => format!("merged into {target}"),
        BranchStatus::Interrupted => "interrupted".into(),
        BranchStatus::Ready => "ready".into(),
        BranchStatus::NoChanges => "no changes".into(),
        BranchStatus::AwaitingPlanApproval => {
            "its plan awaits your approval (`by plan approve`)".into()
        }
        BranchStatus::Running => "running".into(),
        BranchStatus::Waiting => "waiting".into(),
        BranchStatus::WaitingOnChildren => "waiting on its children".into(),
        BranchStatus::Discarded { reason } => format!("discarded: {reason}"),
    }
}

/// For a branch whose last turn its engine lost: what its next turn's
/// prompt starts with, saying what happened and where its children are.
pub(crate) fn recovered_note(store: &Store, record: &Record, reason: &str) -> String {
    let mut text = format!("{RECOVERED_OPEN}\n");
    text.push_str(&format!(
        "Your previous turn was cut off: the Branchyard engine running it stopped ({reason}). \
         Tool calls made after that failed; Claude Code reports them as `Tool permission request \
         failed: AbortError: Stream closed`. Your worktree keeps what was written; check it \
         (`git status`, `git log`) before going on."
    ));
    text.push('\n');
    let children: Vec<String> = record
        .info
        .children
        .iter()
        .filter_map(|child| store.backend().read(child).ok().flatten())
        .map(|child| format!("{} ({})", child.info.name, status_text(&child.info.status)))
        .collect();
    if !children.is_empty() {
        text.push_str(&format!(
            "Your children now: {}. `by children` and `by inspect <child>` say more.",
            children.join(", ")
        ));
        text.push('\n');
    }
    text.push_str("</branchyard-recovered>\n\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_turns_that_will_run_keep_a_parent_parked() {
        assert!(unsettled(&BranchStatus::Running));
        assert!(unsettled(&BranchStatus::Waiting));
        assert!(unsettled(&BranchStatus::WaitingOnChildren));
        for settled in [
            BranchStatus::Ready,
            BranchStatus::NoChanges,
            BranchStatus::Interrupted,
            BranchStatus::AwaitingPlanApproval,
            BranchStatus::Blocked { reason: "x".into() },
            BranchStatus::Failed { reason: "x".into() },
            BranchStatus::Discarded { reason: "x".into() },
        ] {
            assert!(!unsettled(&settled), "{settled:?}");
        }
    }
}
