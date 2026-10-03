//! Goals a judge verifies: a branch given a goal is not done when a turn
//! ends ready, but when a judge finds evidence that the goal is met. See
//! `docs/plans-and-goals.md`.
//!
//! When a branch with a goal ends a turn `ready`, the goal is checked:
//!
//! 1. **Deterministically** first: the branch's check passes on its exact
//!    candidate (when it has one) and its diff is not empty. Failing either
//!    is unmet, with what failed as the missing list, and no judge is asked.
//! 2. Then, when a judge is configured, by a **judge** (usually a
//!    [`HarnessJudge`] running read-only on a scratch branch) answering one
//!    strict JSON verdict `{"met": bool, "evidence": [...], "missing":
//!    [...]}` over the goal, the diff and a summary of the transcript. An
//!    invalid answer falls back to the deterministic result, saying why.
//!
//! Unmet, the branch gets a follow-up turn with the missing list, at most
//! `rounds` times and within its budget; met, it stays ready with the
//! evidence recorded. Every verdict is an [`Activity::Goal`] event.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::compare::AttemptCheck;
use crate::fleet::JudgeSpec;
use crate::judge::{HarnessJudge, Judge, JudgedBy};
use crate::record::Recorder;
use crate::state::{now_ms, Record};
use crate::{Activity, Branch, BranchStatus, Error, Event, RecordedEvent, TaskOptions, Yard};

/// The first line of a goal's follow-up turn.
pub const FOLLOW_UP_HEADER: &str = "[branchyard goal not met]";
/// Follow-up turns a goal gets when none is said.
pub const DEFAULT_ROUNDS: u32 = 2;
/// How much of the diff the judge's prompt quotes.
const DIFF_MAX: usize = 16_000;

/// A goal for a new branch; see [`TaskOptions::goal`].
#[derive(Clone)]
pub struct Goal {
    /// What must be true when the branch is done.
    pub text: String,
    /// Follow-up turns at most, after the first verdict that is unmet.
    pub rounds: u32,
    /// The judge harness, stored with the branch; `None` decides on the
    /// deterministic checks alone.
    pub judge: Option<JudgeSpec>,
    /// A judge of the SDK caller's own, used instead of `judge` for this
    /// call; never stored.
    pub custom: Option<Arc<dyn Judge>>,
}

impl Goal {
    pub fn new(text: impl Into<String>) -> Goal {
        Goal {
            text: text.into(),
            rounds: DEFAULT_ROUNDS,
            judge: None,
            custom: None,
        }
    }
}

impl std::fmt::Debug for Goal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Goal")
            .field("text", &self.text)
            .field("rounds", &self.rounds)
            .field("judge", &self.judge)
            .field("custom", &self.custom.as_ref().map(|j| j.name()))
            .finish()
    }
}

/// The goal's state, kept in the branch's record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct GoalState {
    pub text: String,
    pub rounds: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeSpec>,
    /// Follow-up turns sent so far.
    #[serde(default)]
    pub used: u32,
    /// Decided: met, or given up on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub met: Option<bool>,
}

/// A judge's verdict on a goal, parsed strictly.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalVerdict {
    pub met: bool,
    /// What shows the goal is met (or how far it got).
    pub evidence: Vec<String>,
    /// What is still missing; empty when met.
    pub missing: Vec<String>,
}

/// Most items in a verdict's lists.
const ITEMS_MAX: usize = 20;

/// Parse a goal verdict: the whole text one JSON object (optionally in one
/// fenced block) with exactly `met`, `evidence` and `missing`; non-empty
/// strings; met with evidence and nothing missing, or unmet with something
/// missing.
pub fn parse_goal_verdict(text: &str) -> Result<GoalVerdict, String> {
    let mut body = text.trim();
    if let Some(rest) = body.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        body = rest
            .strip_suffix("```")
            .ok_or("a fenced verdict must end with its fence")?
            .trim();
    }
    if !body.starts_with('{') {
        return Err("the answer is not a JSON object".into());
    }
    let verdict: GoalVerdict =
        serde_json::from_str(body).map_err(|e| format!("not a goal verdict: {e}"))?;
    for (what, items) in [
        ("evidence", &verdict.evidence),
        ("missing", &verdict.missing),
    ] {
        if items.len() > ITEMS_MAX {
            return Err(format!("{what} has more than {ITEMS_MAX} items"));
        }
        if items.iter().any(|i| i.trim().is_empty()) {
            return Err(format!("{what} has an empty item"));
        }
    }
    match verdict.met {
        true if verdict.evidence.is_empty() => Err("met needs evidence".into()),
        true if !verdict.missing.is_empty() => Err("met with something missing".into()),
        false if verdict.missing.is_empty() => Err("unmet needs what is missing".into()),
        _ => Ok(verdict),
    }
}

/// How a goal was decided.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalCheck {
    /// The branch's check on its candidate: `passed`, `failed`, `none`, ...
    pub check: String,
    /// Whether the candidate changes anything.
    pub changes: bool,
    /// Whether the deterministic checks passed.
    pub passed: bool,
}

/// Goal activity on a branch, recorded as [`Activity::Goal`]. Serialized
/// as an object tagged by `type`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GoalActivity {
    /// The branch has this goal, checked when a turn ends ready.
    Set {
        goal: String,
        rounds: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        judge: Option<String>,
    },
    /// A verdict, after the branch's turn `turn` (`round` 0 for the first,
    /// then one per follow-up).
    Verdict {
        round: u32,
        turn: u32,
        met: bool,
        evidence: Vec<String>,
        missing: Vec<String>,
        deterministic: GoalCheck,
        /// `deterministic`, the judge, or the fallback from it.
        by: JudgedBy,
    },
    /// Unmet with no follow-up left: the branch failed.
    Exhausted { rounds: u32, missing: Vec<String> },
}

impl GoalActivity {
    /// One line for logs.
    pub fn describe(&self) -> String {
        match self {
            GoalActivity::Set {
                goal,
                rounds,
                judge,
            } => format!(
                "goal: {goal} (up to {rounds} follow-up turn(s), judged by {})",
                judge.as_deref().unwrap_or("the deterministic checks")
            ),
            GoalActivity::Verdict {
                round,
                met,
                evidence,
                missing,
                by,
                ..
            } => {
                let mut line = match met {
                    true => format!("goal met (round {round}, {})", by.describe()),
                    false => format!("goal not met (round {round}, {})", by.describe()),
                };
                if *met && !evidence.is_empty() {
                    line.push_str(&format!(": {}", evidence.join("; ")));
                }
                if !met {
                    line.push_str(&format!("; missing: {}", missing.join("; ")));
                }
                if let JudgedBy::Fallback { error, .. } = by {
                    line.push_str(&format!("; the judge's answer was not used: {error}"));
                }
                line
            }
            GoalActivity::Exhausted { rounds, missing } => format!(
                "goal not met after {rounds} follow-up turn(s); missing: {}",
                missing.join("; ")
            ),
        }
    }
}

/// A branch's goal and its latest verdict, as `by show` reports it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalInfo {
    pub goal: String,
    pub rounds: u32,
    /// Follow-up turns sent.
    pub used: u32,
    /// `None` until decided.
    pub met: Option<bool>,
    pub evidence: Vec<String>,
    pub missing: Vec<String>,
    /// Who decided the latest verdict.
    pub by: Option<String>,
}

/// The goal `events` describe.
pub fn from_events(events: &[RecordedEvent]) -> Option<GoalInfo> {
    let mut info: Option<GoalInfo> = None;
    for event in events {
        let Activity::Goal(activity) = &event.activity else {
            continue;
        };
        match activity.as_ref() {
            GoalActivity::Set { goal, rounds, .. } => {
                info = Some(GoalInfo {
                    goal: goal.clone(),
                    rounds: *rounds,
                    used: 0,
                    met: None,
                    evidence: Vec::new(),
                    missing: Vec::new(),
                    by: None,
                });
            }
            GoalActivity::Verdict {
                round,
                met,
                evidence,
                missing,
                by,
                ..
            } => {
                if let Some(info) = info.as_mut() {
                    info.used = *round;
                    info.met = met.then_some(true);
                    info.evidence = evidence.clone();
                    info.missing = missing.clone();
                    info.by = Some(by.describe());
                }
            }
            GoalActivity::Exhausted { rounds, missing } => {
                if let Some(info) = info.as_mut() {
                    info.used = *rounds;
                    info.met = Some(false);
                    info.missing = missing.clone();
                }
            }
        }
    }
    info
}

/// Record `goal` on `record`, newly created with its first turn's lease.
pub(crate) fn begin(
    yard: &Yard,
    record: &mut Record,
    fence: &crate::state::Fence,
    goal: &Goal,
) -> Result<(), Error> {
    if goal.text.trim().is_empty() {
        return Err(Error::Unsupported("a goal needs text".into()));
    }
    record.goal = Some(GoalState {
        text: goal.text.trim().to_owned(),
        rounds: goal.rounds,
        judge: goal.judge.clone(),
        used: 0,
        met: None,
    });
    let store = yard.store();
    store.write_fenced(record, fence)?;
    let judge = goal.custom.as_ref().map(|j| j.name()).or_else(|| {
        goal.judge
            .as_ref()
            .map(|j| format!("harness {}", j.harness))
    });
    store.append(
        &record.info.name,
        &RecordedEvent {
            at_ms: now_ms(),
            activity: Activity::Goal(Box::new(GoalActivity::Set {
                goal: goal.text.trim().to_owned(),
                rounds: goal.rounds,
                judge,
            })),
        },
        Some(fence),
    )?;
    Ok(())
}

/// A short summary of the branch's turns: each prompt's start and each
/// turn's last reply, bounded.
fn transcript(events: &[RecordedEvent]) -> String {
    let mut turns: Vec<(String, String)> = Vec::new();
    for event in events {
        match &event.activity {
            Activity::Prompt(text) => turns.push((text.chars().take(400).collect(), String::new())),
            Activity::Harness(Event::MessageDelta { text, .. }) => {
                if let Some((_, reply)) = turns.last_mut() {
                    reply.push_str(text);
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    for (index, (prompt, reply)) in turns.iter().enumerate() {
        let count = reply.chars().count();
        let reply: String = match count > 800 {
            true => reply.chars().skip(count - 800).collect(),
            false => reply.clone(),
        };
        out.push_str(&format!(
            "### Turn {}\nPrompt: {}\nLast reply: {}\n",
            index + 1,
            prompt.trim(),
            reply.trim()
        ));
    }
    let count = out.chars().count();
    match count > 8000 {
        true => out.chars().skip(count - 8000).collect(),
        false => out,
    }
}

/// The prompt a goal judge gets.
pub fn judge_prompt(
    goal: &str,
    task: &str,
    check: &GoalCheck,
    diff: &str,
    transcript: &str,
    rubric: Option<&str>,
) -> String {
    let count = diff.chars().count();
    let diff = match count > DIFF_MAX {
        true => format!(
            "{}\n... ({} more characters)",
            diff.chars().take(DIFF_MAX).collect::<String>(),
            count - DIFF_MAX
        ),
        false => diff.to_owned(),
    };
    let mut text = format!(
        "You are checking whether a coding branch met its goal. Read only: do not change, \
         create or run anything. Decide only from the evidence below: the goal is met when the \
         diff and the transcript show every part of it done.\n\n## Goal\n{goal}\n\n## Task\n\
         {task}\n\n## Deterministic checks\nThe branch's check: {}. The diff changes \
         something: {}.\n",
        check.check, check.changes
    );
    if let Some(rubric) = rubric {
        text.push_str(&format!("\n## Rubric\n{rubric}\n"));
    }
    text.push_str(&format!(
        "\n## Transcript summary\n{}\n\n## Diff\n```diff\n{}\n```\n",
        transcript.trim_end(),
        diff.trim_end()
    ));
    text.push_str(
        "\n## Answer\nReply with only one JSON object and nothing else, of this shape:\n\
         {\"met\": true or false, \"evidence\": [\"what shows it, citing files or the \
         transcript\"], \"missing\": [\"what is still to do; empty when met\"]}\n",
    );
    text
}

/// The prompt of a follow-up turn.
pub fn follow_up_prompt(goal: &str, missing: &[String]) -> String {
    let list: Vec<String> = missing.iter().map(|m| format!("- {m}")).collect();
    format!(
        "{FOLLOW_UP_HEADER}\nThe goal of this branch is not met yet. Goal: {goal}\n\nStill \
         missing:\n{}\n\nFinish it on this branch.",
        list.join("\n")
    )
}

/// Decide `name`'s goal now: the deterministic checks, then the judge.
fn verify(
    yard: &Yard,
    name: &str,
    state: &GoalState,
    custom: Option<&Arc<dyn Judge>>,
    options: &TaskOptions,
) -> Result<(GoalVerdict, GoalCheck, JudgedBy), Error> {
    let attempts = crate::compare::compare(yard, &[name.to_owned()], true)?;
    let attempt = &attempts[0];
    let changes = attempt.candidate.is_some() && attempt.files_changed > 0;
    let (check_word, check_failed) = match &attempt.check {
        AttemptCheck::Passed | AttemptCheck::PassedAtMerge => ("passed".to_owned(), None),
        AttemptCheck::None => ("none configured".to_owned(), None),
        AttemptCheck::Failed { output_tail } => (
            "failed".to_owned(),
            Some(format!(
                "the branch's check passes (it fails: {})",
                tail(output_tail)
            )),
        ),
        AttemptCheck::TimedOut { output_tail } => (
            "timed out".to_owned(),
            Some(format!(
                "the branch's check passes (it timed out: {})",
                tail(output_tail)
            )),
        ),
        AttemptCheck::Error { message } => (
            "could not run".to_owned(),
            Some(format!("the branch's check runs (it could not: {message})")),
        ),
        other => (other.word().to_owned(), None),
    };
    let check = GoalCheck {
        check: check_word.clone(),
        changes,
        passed: changes && check_failed.is_none(),
    };
    if !check.passed {
        let mut missing = Vec::new();
        if !changes {
            missing.push("a change: the branch's candidate changes nothing".to_owned());
        }
        missing.extend(check_failed);
        return Ok((
            GoalVerdict {
                met: false,
                evidence: Vec::new(),
                missing,
            },
            check,
            JudgedBy::Deterministic,
        ));
    }
    let deterministic = GoalVerdict {
        met: true,
        evidence: vec![format!(
            "the branch's check {check_word}; its candidate changes {} file(s), +{} -{}",
            attempt.files_changed, attempt.insertions, attempt.deletions
        )],
        missing: Vec::new(),
    };
    let judge: Option<Arc<dyn Judge>> = match (custom, &state.judge) {
        (Some(custom), _) => Some(custom.clone()),
        (None, Some(spec)) => Some(Arc::new(HarnessJudge {
            spec: spec.clone(),
            options: judge_options(options),
        })),
        (None, None) => None,
    };
    let Some(judge) = judge else {
        return Ok((deterministic, check, JudgedBy::Deterministic));
    };
    let store = yard.store();
    let record = store.read(name)?;
    let events = crate::record::read(&store, name)?;
    let diff = match &record.info.candidate {
        Some(c) => crate::git::diff(yard.root(), &record.info.base, &c.commit)?,
        None => String::new(),
    };
    let prompt = judge_prompt(
        &state.text,
        &record.info.prompt,
        &check,
        &diff,
        &transcript(&events),
        state.judge.as_ref().and_then(|j| j.rubric.as_deref()),
    );
    let answer = judge
        .verdict(yard, &prompt, &[format!("goal {name}")])
        .map_err(|e| e.to_string())
        .and_then(|text| parse_goal_verdict(&text));
    Ok(match answer {
        Ok(verdict) => (verdict, check, JudgedBy::Judge { name: judge.name() }),
        Err(error) => (
            deterministic,
            check,
            JudgedBy::Fallback {
                name: judge.name(),
                error,
            },
        ),
    })
}

/// The judge's branch options: the task's isolation, provider and
/// approvals, no goal or plan of its own.
fn judge_options(options: &TaskOptions) -> TaskOptions {
    TaskOptions {
        plan: false,
        goal: None,
        ..options.clone()
    }
}

fn tail(output: &str) -> String {
    let lines: Vec<&str> = output.trim_end().lines().collect();
    let start = lines.len().saturating_sub(3);
    lines[start..].join(" / ")
}

/// Settle the goal of `branch` after its turn: while it is ready with an
/// undecided goal, verify it, and send a follow-up turn with what is
/// missing, until it is met, the rounds run out, or a turn ends otherwise.
/// Returns the branch as it ends.
pub(crate) fn pursue(yard: &Yard, branch: Branch, options: &TaskOptions) -> Result<Branch, Error> {
    let name = branch.info().name.clone();
    let custom = options.goal.as_ref().and_then(|g| g.custom.clone());
    let mut branch = branch;
    loop {
        let record = yard.store().read(&name)?;
        let Some(state) = record.goal.clone() else {
            return Ok(branch);
        };
        if state.met.is_some() || record.info.status != BranchStatus::Ready {
            return Ok(branch);
        }
        let (verdict, check, by) = verify(yard, &name, &state, custom.as_ref(), options)?;
        let (mut record, lease) = crate::ops::hold(yard, &name)?;
        let store = yard.store();
        let mut recorder = Recorder::fenced(&store, lease.fence(), options.observer.clone());
        recorder.record(Activity::Goal(Box::new(GoalActivity::Verdict {
            round: state.used,
            turn: record.info.turns,
            met: verdict.met,
            evidence: verdict.evidence.clone(),
            missing: verdict.missing.clone(),
            deterministic: check,
            by,
        })))?;
        let goal = record.goal.as_mut().expect("read above");
        if verdict.met {
            goal.met = Some(true);
            store.write_fenced(&record, lease.fence())?;
            lease.finish(None, None)?;
            return yard.branch(&name);
        }
        if goal.used >= goal.rounds {
            goal.met = Some(false);
            let rounds = goal.rounds;
            recorder.record(Activity::Goal(Box::new(GoalActivity::Exhausted {
                rounds,
                missing: verdict.missing.clone(),
            })))?;
            record.info.status = BranchStatus::Failed {
                reason: format!(
                    "its goal was not met after {rounds} follow-up turn(s); missing: {}",
                    verdict.missing.join("; ")
                ),
            };
            recorder.finish(lease, &record)?;
            let _ = crate::fleet::observe(yard, &name, None);
            return yard.branch(&name);
        }
        goal.used += 1;
        let prompt = follow_up_prompt(&goal.text, &verdict.missing);
        store.write_fenced(&record, lease.fence())?;
        lease.finish(None, None)?;
        branch = crate::run::send(yard, &name, &prompt, &send_options(options))?;
    }
}

/// A follow-up's options: the run's, as a send takes them.
pub(crate) fn send_options(options: &TaskOptions) -> TaskOptions {
    TaskOptions {
        name: None,
        base: None,
        harness: None,
        plan: false,
        goal: None,
        workspace: None,
        ..options.clone()
    }
}

/// Pursue the goals of several branches at once, as a fan's.
pub(crate) fn pursue_all(
    yard: &Yard,
    branches: Vec<Branch>,
    options: &TaskOptions,
) -> Result<Vec<Branch>, Error> {
    let any = branches.iter().any(|b| {
        yard.store()
            .read(&b.info().name)
            .is_ok_and(|r| r.goal.is_some())
    });
    if !any {
        return Ok(branches);
    }
    let results: Vec<Result<Branch, Error>> = std::thread::scope(|scope| {
        let handles: Vec<_> = branches
            .into_iter()
            .map(|branch| scope.spawn(move || pursue(yard, branch, options)))
            .collect();
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(result) => result,
                Err(panic) => std::panic::resume_unwind(panic),
            })
            .collect()
    });
    results.into_iter().collect()
}

/// `name`'s goal, from its events.
pub(crate) fn info(yard: &Yard, name: &str) -> Result<Option<GoalInfo>, Error> {
    let events = crate::record::read(&yard.store(), name)?;
    Ok(from_events(&events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_are_parsed_strictly() {
        let met =
            parse_goal_verdict(r#"{"met": true, "evidence": ["a.txt has it"], "missing": []}"#)
                .unwrap();
        assert!(met.met);
        let unmet = parse_goal_verdict(
            "```json\n{\"met\": false, \"evidence\": [], \"missing\": [\"tests\"]}\n```",
        )
        .unwrap();
        assert_eq!(unmet.missing, ["tests"]);
        for bad in [
            "yes, it is met",
            r#"{"met": true, "evidence": [], "missing": []}"#,
            r#"{"met": true, "evidence": ["x"], "missing": ["y"]}"#,
            r#"{"met": false, "evidence": ["x"], "missing": []}"#,
            r#"{"met": "yes", "evidence": ["x"], "missing": []}"#,
            r#"{"met": true, "evidence": ["x"], "missing": [], "score": 9}"#,
            r#"{"met": true, "evidence": [" "], "missing": []}"#,
            r#"{"met": true, "evidence": ["x"]}"#,
        ] {
            assert!(parse_goal_verdict(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn follow_ups_carry_the_missing_list_and_the_goal() {
        let text = follow_up_prompt("tests pass", &["a test for x".into(), "docs".into()]);
        assert!(text.starts_with(FOLLOW_UP_HEADER));
        assert!(text.contains("Goal: tests pass"));
        assert!(text.contains("- a test for x\n- docs"));
    }
}
