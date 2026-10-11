//! The lifecycle of a branch that a trigger keeps alive: when it last
//! ended a turn, parking it while it rests, and starting its harness
//! session over once it has rested long. What decides when, and applies
//! these, is the server's trigger dispatcher (`docs/triggers.md`); this
//! module is the engine's side of it, and works on any branch.
//!
//! Nothing here adds a column. "When it last ended a turn" is read from
//! the end of the branch's event log, newest first, and where it stands
//! (idle, dormant) is the newest lifecycle event after that: a turn that
//! ends later makes the older events stale.

use crate::ops;
use crate::record::Recorder;
use crate::state::{Record, Store};
use crate::{Activity, Branch, BranchStatus, Error, Event, LifecycleState, Yard};

/// Events read per page while looking back through a log.
const PAGE: usize = 256;
/// The turns a fresh session's handoff quotes, newest.
const HANDOFF_TURNS: usize = 6;
/// Longest prompt or reply quoted in a handoff, in characters.
const QUOTE_MAX: usize = 1200;

/// Where a branch stands, as its record and the end of its event log say.
#[derive(Clone, Debug, PartialEq)]
pub struct Resting {
    /// The branch's status now.
    pub status: BranchStatus,
    /// Milliseconds since the Unix epoch when the branch last ended a
    /// turn, or was reincarnated, whichever is later; its creation time
    /// when it has done neither.
    pub last_turn_end_ms: u64,
    /// The newest lifecycle state recorded since then: `Idle` or
    /// `Dormant`; `None` when the branch has not rested since.
    pub state: Option<LifecycleState>,
}

impl Resting {
    /// Whether a turn runs on the branch or is owed to it: running, not
    /// started, waiting for its plan to be approved or for its children.
    /// Such a branch is not resting.
    pub fn engaged(&self) -> bool {
        matches!(
            self.status,
            BranchStatus::Running
                | BranchStatus::Waiting
                | BranchStatus::Blocked { .. }
                | BranchStatus::AwaitingPlanApproval
                | BranchStatus::WaitingOnChildren
        )
    }

    /// Whether the branch is settled for good: merged or discarded. It
    /// takes no more turns, so it has no lifecycle.
    pub fn finished(&self) -> bool {
        matches!(
            self.status,
            BranchStatus::Merged { .. } | BranchStatus::Discarded { .. }
        )
    }
}

/// What [`Yard::park`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parked {
    /// What it released, one line each: the kept sandbox destroyed, or a
    /// warning that it could not be. Empty when the branch held nothing.
    pub released: Vec<String>,
}

impl Yard {
    /// When `branch` last ended a turn and where it stands in its
    /// lifecycle: see [`Resting`].
    pub fn resting(&self, branch: &str) -> Result<Resting, Error> {
        let store = self.store();
        let record = store.read(branch)?;
        resting_of(&store, &record)
    }

    /// Park `branch`, which is not running a turn: release what it holds
    /// (its kept sandbox, destroyed; the next turn gets a fresh one) and
    /// record [`Activity::Lifecycle`] with `state`, `Idle` or `Dormant`.
    /// The record is written even when there was nothing to release. The
    /// branch's worktree, session and history are untouched, so the next
    /// send continues it as before. Refused while a turn runs on it, and
    /// for a branch that is settled.
    pub fn park(&self, branch: &str, state: LifecycleState, reason: &str) -> Result<Parked, Error> {
        if !matches!(state, LifecycleState::Idle | LifecycleState::Dormant) {
            return Err(Error::Denied(format!(
                "a branch is parked as idle or dormant, not {state}"
            )));
        }
        let store = self.store();
        let (record, lease) = ops::hold(self, branch)?;
        let resting = resting_of(&store, &record)?;
        refuse_unless_resting(branch, &resting)?;
        let released =
            crate::snapshots::discard_kept(self, &record, &format!("the branch went {state}"));
        let said = describe(&released);
        let mut recorder = Recorder::fenced(&store, lease.fence(), None);
        recorder.record(Activity::Lifecycle {
            state,
            reason: with_released(reason, &said),
            idle_since_ms: resting.last_turn_end_ms,
        })?;
        for activity in released {
            recorder.record(activity)?;
        }
        // Released without a status event: parking is not a turn, so it
        // must not read as one the next time the log is looked at.
        lease.finish(None, None)?;
        Ok(Parked { released: said })
    }

    /// Remove `branch` as its lifecycle's last step: [`Yard::remove`],
    /// with [`Activity::Lifecycle`] `deleted` recorded first (it stays in
    /// the repository's event feed after the branch is gone).
    pub fn retire(&self, branch: &str, reason: &str) -> Result<(), Error> {
        let store = self.store();
        let record = store.read(branch)?;
        let resting = resting_of(&store, &record)?;
        refuse_unless_resting(branch, &resting)?;
        let note = Activity::Lifecycle {
            state: LifecycleState::Deleted,
            reason: reason.to_owned(),
            idle_since_ms: resting.last_turn_end_ms,
        };
        ops::remove_noting(self, branch, &crate::RemoveOptions::default(), Some(note))?;
        Ok(())
    }
}

impl Branch {
    /// Start this branch's harness session over, in place: the same
    /// branch, name, worktree and original prompt, with the harness's
    /// session retired. Its next turn starts a fresh session whose prompt
    /// begins with a handoff (the original task, what the last turns asked
    /// and answered, the candidate) and then the prompt it is sent, as
    /// after a rewind a session cannot continue from. Records
    /// [`Activity::Lifecycle`] `reincarnated` with `reason`.
    ///
    /// This is not [`Branch::reincarnate`], which makes a new branch under
    /// a new name; a trigger's branch keeps its name. Refused while a turn
    /// runs on it, and for a branch that is settled.
    pub fn reincarnate_in_place(&self, reason: &str) -> Result<(), Error> {
        let name = &self.info().name;
        let store = self.yard.store();
        let (mut record, lease) = ops::hold(&self.yard, name)?;
        let resting = resting_of(&store, &record)?;
        refuse_unless_resting(name, &resting)?;
        // The kept sandbox holds the old conversation's state.
        let released =
            crate::snapshots::discard_kept(&self.yard, &record, "the branch was reincarnated");
        let brief = handoff(&store, &record, reason)?;
        record.info.session = None;
        record.info.stalled = false;
        record.context = Some(brief);
        let mut recorder = Recorder::fenced(&store, lease.fence(), None);
        recorder.record(Activity::Lifecycle {
            state: LifecycleState::Reincarnated,
            reason: format!(
                "{reason}; its harness session was retired, and its next turn starts a fresh \
                 one on the same worktree with a handoff of the turns before"
            ),
            idle_since_ms: resting.last_turn_end_ms,
        })?;
        for activity in released {
            recorder.record(activity)?;
        }
        lease.finish(Some(&record), None)?;
        Ok(())
    }
}

fn refuse_unless_resting(name: &str, resting: &Resting) -> Result<(), Error> {
    if resting.engaged() {
        return Err(Error::Running(name.to_owned()));
    }
    if let BranchStatus::Merged { target, .. } = &resting.status {
        return Err(Error::Denied(format!(
            "{name} was merged into {target} and takes no more turns"
        )));
    }
    if let BranchStatus::Discarded { reason } = &resting.status {
        return Err(Error::Denied(format!(
            "{name} was discarded ({reason}) and takes no more turns"
        )));
    }
    Ok(())
}

fn describe(released: &[Activity]) -> Vec<String> {
    released
        .iter()
        .filter_map(|activity| match activity {
            Activity::Sandbox(event) => Some(event.describe()),
            Activity::Warning(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn with_released(reason: &str, released: &[String]) -> String {
    match released.is_empty() {
        true => format!("{reason}; it held nothing to release"),
        false => format!("{reason}; {}", released.join("; ")),
    }
}

/// Look back through `record`'s log for the end of its last turn and the
/// lifecycle events after it.
fn resting_of(store: &Store, record: &Record) -> Result<Resting, Error> {
    let name = &record.info.name;
    let backend = store.backend();
    let mut end = backend.event_count(name)?;
    let mut last = record.created_ms;
    let mut state = None;
    'back: while end > 0 {
        let start = end.saturating_sub(PAGE as u64);
        let page = backend.events_since(name, start, PAGE)?;
        for (_, event) in page.iter().rev() {
            match &event.activity {
                Activity::Lifecycle {
                    state: LifecycleState::Reincarnated,
                    ..
                } => {
                    last = event.at_ms;
                    break 'back;
                }
                Activity::Lifecycle {
                    state: rested @ (LifecycleState::Idle | LifecycleState::Dormant),
                    ..
                } => {
                    state.get_or_insert(*rested);
                }
                Activity::Status(status) if *status != BranchStatus::Running => {
                    last = event.at_ms;
                    break 'back;
                }
                Activity::Harness(Event::TurnEnded { .. }) => {
                    last = event.at_ms;
                    break 'back;
                }
                _ => {}
            }
        }
        end = start;
    }
    Ok(Resting {
        status: record.info.status.clone(),
        last_turn_end_ms: last,
        state,
    })
}

/// What a fresh session on `record`'s worktree starts with: the original
/// task, the candidate, and the last few turns' prompts and replies,
/// newest last. Bounded however long the branch has lived.
fn handoff(store: &Store, record: &Record, why: &str) -> Result<String, Error> {
    let name = &record.info.name;
    let backend = store.backend();
    // Newest first: replies come in pieces, so a turn is complete when
    // its prompt is reached.
    let mut turns: Vec<(String, String)> = Vec::new();
    let mut pieces: Vec<String> = Vec::new();
    let mut end = backend.event_count(name)?;
    'back: while end > 0 {
        let start = end.saturating_sub(PAGE as u64);
        for (_, event) in backend.events_since(name, start, PAGE)?.iter().rev() {
            match &event.activity {
                Activity::Harness(Event::MessageDelta { text, .. }) => pieces.push(text.clone()),
                Activity::Prompt(text) => {
                    let reply: String = pieces.drain(..).rev().collect();
                    turns.push((crate::checkpoint::asked_text(text).to_owned(), reply));
                    if turns.len() == HANDOFF_TURNS {
                        break 'back;
                    }
                }
                _ => {}
            }
        }
        end = start;
    }
    turns.reverse();
    let mut out = format!(
        "This continues the work of branch {name}, which went quiet ({why}). Its harness \
         session was retired, so this is a fresh session on the same worktree. The working tree is as the \
         last turn left it; check it (`git status`, `git log`) before going on.\n\n### The \
         original task\n{}\n",
        quote(&record.info.prompt)
    );
    if let Some(candidate) = &record.info.candidate {
        let short = candidate.commit.get(..10).unwrap_or(&candidate.commit);
        out.push_str(&format!(
            "\nIts candidate is commit {short}: {} file(s) changed against its base, +{} -{}.\n",
            candidate.files_changed, candidate.insertions, candidate.deletions
        ));
    }
    let ran = record.info.turns as usize;
    if turns.is_empty() {
        out.push_str("\n(No earlier prompts were recorded.)\n");
    } else {
        out.push_str(&format!(
            "\nIt has run {ran} turn(s). The last {} asked and answered:\n",
            turns.len()
        ));
    }
    let first = ran.saturating_sub(turns.len()) + 1;
    for (n, (asked, replied)) in turns.iter().enumerate() {
        out.push_str(&format!(
            "\n### Turn {}\nAsked: {}\n",
            first + n,
            quote(asked)
        ));
        if !replied.trim().is_empty() {
            out.push_str(&format!("Replied: {}\n", quote(replied)));
        }
    }
    Ok(out)
}

fn quote(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(QUOTE_MAX) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_is_cut_on_a_character_boundary() {
        let long = "é".repeat(QUOTE_MAX + 10);
        let cut = quote(&long);
        assert_eq!(cut.chars().count(), QUOTE_MAX + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(quote("  short \n"), "short");
    }

    #[test]
    fn only_a_branch_with_nothing_owed_is_resting() {
        let at = |status| Resting {
            status,
            last_turn_end_ms: 0,
            state: None,
        };
        for engaged in [
            BranchStatus::Running,
            BranchStatus::WaitingOnChildren,
            BranchStatus::AwaitingPlanApproval,
        ] {
            assert!(at(engaged).engaged());
        }
        for resting in [
            BranchStatus::Ready,
            BranchStatus::NoChanges,
            BranchStatus::Interrupted,
            BranchStatus::Failed { reason: "x".into() },
        ] {
            let resting = at(resting);
            assert!(!resting.engaged() && !resting.finished());
        }
        assert!(at(BranchStatus::Discarded { reason: "x".into() }).finished());
        assert!(describe(&[]).is_empty());
        assert_eq!(
            with_released("no turn for 30m", &[]),
            "no turn for 30m; it held nothing to release"
        );
        assert_eq!(
            with_released("no turn for 30m", &["sandbox: not kept (x)".into()]),
            "no turn for 30m; sandbox: not kept (x)"
        );
    }
}
