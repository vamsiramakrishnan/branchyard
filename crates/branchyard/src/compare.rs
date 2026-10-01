//! Attempts side by side: the branches of a `by fan`, or any branches, with
//! what each cost, how long it ran, whether its check passes and what it
//! changed, including the files no other attempt touched.

use std::collections::BTreeMap;

use branchyard_workspace::{Check, CheckResult, Commit};
use serde::{Deserialize, Serialize};

use crate::state::now_ms;
use crate::{git, ops, Activity, BranchInfo, BranchStatus, Error, Event, RecordedEvent, Yard};

/// One attempt, as `by compare` shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub branch: String,
    pub harness: String,
    pub status: BranchStatus,
    pub turns: u32,
    pub cost_usd: Option<f64>,
    /// Input and output tokens the harness reported, summed over turns;
    /// `None` when it reported none.
    pub tokens: Option<u64>,
    /// Time spent in turns, from each prompt to the status that ended it.
    pub duration_ms: Option<u64>,
    pub check: AttemptCheck,
    pub candidate: Option<String>,
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
    /// Every path the candidate changes against the base, sorted.
    pub files: Vec<String>,
    /// The paths no other compared attempt changes.
    pub unique_files: Vec<String>,
}

/// An attempt's check. Serialized as an object tagged by `state`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AttemptCheck {
    /// The branch has no check.
    None,
    /// Not run by this comparison; ask for it with `run_checks`.
    NotRun,
    /// It passed at the merge (the branch is merged).
    PassedAtMerge,
    Passed,
    Failed {
        output_tail: String,
    },
    TimedOut {
        output_tail: String,
    },
    /// It could not be run.
    Error {
        message: String,
    },
    /// Nothing to check: no candidate.
    NoCandidate,
}

impl AttemptCheck {
    /// A short word for tables.
    pub fn word(&self) -> &'static str {
        match self {
            AttemptCheck::None => "none",
            AttemptCheck::NotRun => "not run",
            AttemptCheck::PassedAtMerge | AttemptCheck::Passed => "passed",
            AttemptCheck::Failed { .. } => "failed",
            AttemptCheck::TimedOut { .. } => "timed out",
            AttemptCheck::Error { .. } => "error",
            AttemptCheck::NoCandidate => "-",
        }
    }
}

/// An attempt from what a branch's record and events say, with the paths
/// its candidate changes. `unique_files` is left empty; see
/// [`mark_unique`].
pub fn attempt(info: &BranchInfo, events: &[RecordedEvent], files: Vec<String>) -> Attempt {
    let (files_changed, insertions, deletions) = info
        .candidate
        .as_ref()
        .map_or((0, 0, 0), |c| (c.files_changed, c.insertions, c.deletions));
    Attempt {
        branch: info.name.clone(),
        harness: info.profile.clone(),
        status: info.status.clone(),
        turns: info.turns,
        cost_usd: info.cost_usd,
        tokens: tokens(events),
        duration_ms: duration(events),
        check: match (&info.candidate, &info.status) {
            (None, _) => AttemptCheck::NoCandidate,
            (_, BranchStatus::Merged { .. }) => AttemptCheck::PassedAtMerge,
            _ => AttemptCheck::NotRun,
        },
        candidate: info.candidate.as_ref().map(|c| c.commit.clone()),
        files_changed,
        insertions,
        deletions,
        files,
        unique_files: Vec::new(),
    }
}

/// Fill each attempt's `unique_files`: its paths no other attempt has.
pub fn mark_unique(attempts: &mut [Attempt]) {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for attempt in attempts.iter() {
        for file in &attempt.files {
            *counts.entry(file.clone()).or_default() += 1;
        }
    }
    for attempt in attempts.iter_mut() {
        attempt.unique_files = attempt
            .files
            .iter()
            .filter(|f| counts.get(*f) == Some(&1))
            .cloned()
            .collect();
    }
}

/// Paths changed between two commits, from a unified diff's headers; for a
/// caller that has only the diff text.
pub fn diff_files(diff: &str) -> Vec<String> {
    let mut files: Vec<String> = diff
        .lines()
        .filter_map(|line| line.strip_prefix("diff --git a/"))
        .filter_map(|rest| rest.split_once(" b/").map(|(a, _)| a.to_owned()))
        .collect();
    files.sort();
    files.dedup();
    files
}

/// Summed input and output tokens: a turn's own reports, or for running
/// totals the growth within the session.
fn tokens(events: &[RecordedEvent]) -> Option<u64> {
    let mut seen = false;
    let mut total = 0u64;
    let mut session_high = 0u64;
    for event in events {
        match &event.activity {
            Activity::Harness(Event::SessionStarted { .. }) => session_high = 0,
            Activity::Harness(Event::UsageObserved { usage, .. }) => {
                let n = usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0);
                if usage.input_tokens.is_none() && usage.output_tokens.is_none() {
                    continue;
                }
                seen = true;
                if usage.cumulative {
                    if n > session_high {
                        total += n - session_high;
                        session_high = n;
                    }
                } else {
                    total += n;
                }
            }
            _ => {}
        }
    }
    seen.then_some(total)
}

/// Time from each prompt to the status that ended its turn; a turn still
/// running counts until now.
fn duration(events: &[RecordedEvent]) -> Option<u64> {
    let mut total = None::<u64>;
    let mut started = None;
    for event in events {
        match &event.activity {
            Activity::Prompt(_) => started = started.or(Some(event.at_ms)),
            Activity::Status(status) if *status != BranchStatus::Running => {
                if let Some(start) = started.take() {
                    *total.get_or_insert(0) += event.at_ms.saturating_sub(start);
                }
            }
            _ => {}
        }
    }
    if let Some(start) = started {
        *total.get_or_insert(0) += now_ms().saturating_sub(start);
    }
    total
}

/// The attempts named, in order, and, with `run_checks`, each one's check
/// run on its exact candidate in a private worktree.
pub(crate) fn compare(
    yard: &Yard,
    names: &[String],
    run_checks: bool,
) -> Result<Vec<Attempt>, Error> {
    let store = yard.store();
    let mut attempts = Vec::new();
    for name in names {
        let record = store.read(name)?;
        let events = crate::record::read(&store, name)?;
        let files = match &record.info.candidate {
            Some(candidate) => {
                git::changed_paths(&yard.root, &record.info.base, &candidate.commit)?
            }
            None => Vec::new(),
        };
        let mut attempt = attempt(&record.info, &events, files);
        match (&record.check, &record.info.candidate) {
            (None, Some(_)) => attempt.check = AttemptCheck::None,
            (Some(argv), Some(candidate)) if run_checks => {
                let check = Check {
                    argv: argv.clone(),
                    timeout: ops::CHECK_TIMEOUT,
                };
                attempt.check = match yard
                    .repo
                    .check_commit(&Commit(candidate.commit.clone()), &check)
                {
                    Ok(CheckResult::Passed { .. }) => AttemptCheck::Passed,
                    Ok(CheckResult::Failed { output_tail }) => AttemptCheck::Failed { output_tail },
                    Ok(CheckResult::TimedOut { output_tail }) => {
                        AttemptCheck::TimedOut { output_tail }
                    }
                    Err(error) => AttemptCheck::Error {
                        message: error.to_string(),
                    },
                };
            }
            _ => {}
        }
        attempts.push(attempt);
    }
    mark_unique(&mut attempts);
    Ok(attempts)
}

/// The branches `by fan` started as `<name>-<harness>`, oldest first:
/// top-level branches whose name is `name`, a hyphen and the harness or
/// profile it runs, all with one prompt. A routed fan's repeated harness
/// is `<name>-<harness>-<n>`.
pub(crate) fn fan(yard: &Yard, name: &str) -> Result<Vec<String>, Error> {
    let infos = yard.branches()?;
    let siblings: Vec<&BranchInfo> = infos
        .iter()
        .filter(|info| info.depth == 0 && info.parent.is_none())
        .filter(|info| {
            [&info.harness, &info.profile].iter().any(|id| {
                let stem = format!("{name}-{id}");
                info.name == stem
                    || info
                        .name
                        .strip_prefix(&format!("{stem}-"))
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
        })
        .collect();
    let Some(first) = siblings.first() else {
        return Err(Error::UnknownBranch(format!(
            "{name} (no branches named {name}-<harness> from one by fan)"
        )));
    };
    Ok(siblings
        .iter()
        .filter(|info| info.prompt == first.prompt)
        .map(|info| info.name.clone())
        .collect())
}

/// The diff from attempt `a`'s candidate to `b`'s.
pub(crate) fn between(yard: &Yard, a: &str, b: &str) -> Result<String, Error> {
    let store = yard.store();
    let commit = |name: &str| -> Result<String, Error> {
        let record = store.read(name)?;
        Ok(match record.info.candidate {
            Some(candidate) => candidate.commit,
            None => record.info.base,
        })
    };
    git::diff(&yard.root, &commit(a)?, &commit(b)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Usage;

    fn at(at_ms: u64, activity: Activity) -> RecordedEvent {
        RecordedEvent { at_ms, activity }
    }

    fn usage(cumulative: bool, input: u64, output: u64) -> Activity {
        Activity::Harness(Event::UsageObserved {
            turn: Some(1),
            usage: Usage {
                cumulative,
                input_tokens: Some(input),
                output_tokens: Some(output),
                cached_input_tokens: None,
                cost_usd: None,
            },
        })
    }

    #[test]
    fn tokens_and_duration_come_from_the_events() {
        let events = [
            at(1_000, Activity::Prompt("a".into())),
            at(1_100, usage(true, 10, 5)),
            at(1_200, usage(true, 30, 10)),
            at(1_500, Activity::Status(BranchStatus::Ready)),
            at(2_000, Activity::Prompt("b".into())),
            at(2_100, usage(false, 7, 3)),
            at(2_250, Activity::Status(BranchStatus::NoChanges)),
        ];
        assert_eq!(tokens(&events), Some(40 + 10));
        assert_eq!(duration(&events), Some(500 + 250));
        assert_eq!(tokens(&events[..1]), None);
    }

    #[test]
    fn unique_files_are_those_no_other_attempt_changed() {
        let info = |name: &str| BranchInfo {
            name: name.into(),
            git_branch: format!("by/{name}"),
            worktree: "/w".into(),
            prompt: "p".into(),
            harness: "h".into(),
            profile: "h".into(),
            session: None,
            parent: None,
            children: Vec::new(),
            depth: 0,
            base: "b".into(),
            candidate: None,
            status: BranchStatus::Ready,
            turns: 1,
            cost_usd: None,
            created_at: 0,
            stalled: false,
            superseded_by: None,
        };
        let mut attempts = vec![
            attempt(&info("a"), &[], vec!["x".into(), "y".into()]),
            attempt(&info("b"), &[], vec!["y".into(), "z".into()]),
        ];
        mark_unique(&mut attempts);
        assert_eq!(attempts[0].unique_files, ["x"]);
        assert_eq!(attempts[1].unique_files, ["z"]);
        assert_eq!(
            diff_files("diff --git a/p q b/p q\n+x\ndiff --git a/r b/r\n"),
            ["p q", "r"]
        );
    }
}
