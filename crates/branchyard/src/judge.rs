//! Judging attempts at one task: each candidate's check on its exact
//! candidate, a deterministic score from what `by compare` gathers, and
//! optionally a judge harness's verdict, then a ranking and a proposed
//! pick. See `docs/fleet.md`.
//!
//! The deterministic score needs no model. A judge harness runs read-only
//! (every tool request denied) on a scratch branch that is removed after,
//! and must answer one strict JSON object; anything else falls back to the
//! deterministic score, saying why.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::compare::{Attempt, AttemptCheck};
use crate::fleet::{self, FleetActivity, JudgeMark, JudgeSpec};
use crate::{git, Activity, BranchStatus, Error, Event, Policy, RecordedEvent, TaskOptions, Yard};

/// How much of each candidate's diff the judge's prompt quotes.
pub const DIFF_MAX: usize = 16_000;

/// Something that reads a judging prompt and answers with a verdict's
/// text: a harness ([`HarnessJudge`]), or anything an SDK caller plugs in.
pub trait Judge: Send + Sync {
    /// How results name it, such as `harness claude-code`.
    fn name(&self) -> String;
    /// The verdict's text for `prompt`, which lists `candidates`.
    fn verdict(&self, yard: &Yard, prompt: &str, candidates: &[String]) -> Result<String, Error>;
}

/// A harness as the judge: run on a scratch branch from `HEAD` with every
/// tool request denied, its last message taken as the verdict, the branch
/// removed after.
#[derive(Clone)]
pub struct HarnessJudge {
    pub spec: JudgeSpec,
    /// Isolation, provider and approvals for the judge's branch, from the
    /// task; its policy is replaced by one that denies every tool.
    pub options: TaskOptions,
}

impl Judge for HarnessJudge {
    fn name(&self) -> String {
        let mut candidate = fleet::FleetCandidate::new(&self.spec.harness);
        candidate.model = self.spec.model.clone();
        candidate.effort = self.spec.effort;
        format!("harness {}", candidate.label())
    }

    fn verdict(&self, yard: &Yard, prompt: &str, candidates: &[String]) -> Result<String, Error> {
        let mut provision = self.options.provision.clone().unwrap_or_default();
        if self.spec.model.is_some() {
            provision.model = self.spec.model.clone();
        }
        if self.spec.effort.is_some() {
            provision.effort = self.spec.effort;
        }
        let options = TaskOptions {
            harness: Some(self.spec.harness.clone()),
            command: self.spec.command.clone().or(self.options.command.clone()),
            provision: (!provision.is_empty()).then_some(provision),
            policy: Policy::deny_all(),
            name: None,
            base: None,
            check: None,
            delegation: None,
            seats: None,
            workspace: None,
            plan: false,
            goal: None,
            ..self.options.clone()
        };
        let stem = format!("judge {}", candidates.first().map_or("", String::as_str));
        let spec = crate::run::AttemptSpec {
            label: None,
            harness: options.harness.clone(),
            command: options.command.clone(),
            provision: options.provision.clone(),
            budget: None,
            events: vec![Activity::Fleet(Box::new(FleetActivity::Judging {
                candidates: candidates.to_vec(),
            }))],
        };
        let named = TaskOptions {
            name: Some(crate::names::plan_one(
                &yard.store(),
                yard.root(),
                None,
                &stem,
                &BTreeSet::new(),
            )?),
            ..options
        };
        let branch = crate::run::run_attempts(yard, prompt, &named, vec![spec])?.remove(0);
        let name = branch.info().name.clone();
        let events = branch.events();
        let status = branch.info().status.clone();
        // A scratch branch: nothing of it stays.
        let _ = yard.remove(&name);
        let events = events?;
        match status {
            BranchStatus::Ready | BranchStatus::NoChanges => Ok(message(&events)),
            other => Err(Error::Harness(format!(
                "the judge's branch {name} ended {}",
                serde_json::to_value(&other)
                    .ok()
                    .and_then(|v| v["state"].as_str().map(str::to_owned))
                    .unwrap_or_default()
            ))),
        }
    }
}

/// Every message delta of the last turn, whole.
fn message(events: &[RecordedEvent]) -> String {
    let start = events
        .iter()
        .rposition(|e| matches!(e.activity, Activity::Prompt(_)))
        .map_or(0, |i| i + 1);
    events[start..]
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Harness(Event::MessageDelta { text, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// How to judge.
#[derive(Clone, Default)]
pub struct JudgeOptions {
    /// Run each candidate's check on its exact candidate first. `by judge`
    /// always does.
    pub run_checks: bool,
    /// Ask this judge too; the deterministic score alone when `None`.
    pub judge: Option<Arc<dyn Judge>>,
    /// Added to the judge's prompt under `## Rubric`.
    pub rubric: Option<String>,
    /// Record the scores on the candidates and in the outcome store.
    pub record: bool,
}

/// A judge's answer, parsed strictly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    /// Every candidate once, best first.
    pub ranking: Vec<String>,
    /// Every candidate's score, from 0 to 100.
    pub scores: BTreeMap<String, f64>,
    /// Every candidate's reason.
    pub reasons: BTreeMap<String, String>,
}

/// Parse a verdict: the whole text one JSON object (optionally inside one
/// fenced block and nothing else) with exactly `ranking`, `scores` and
/// `reasons`, naming every candidate exactly once each, scores from 0 to
/// 100.
pub fn parse_verdict(text: &str, candidates: &[String]) -> Result<Verdict, String> {
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
    let verdict: Verdict = serde_json::from_str(body).map_err(|e| format!("not a verdict: {e}"))?;
    let want: BTreeSet<&str> = candidates.iter().map(String::as_str).collect();
    let ranked: BTreeSet<&str> = verdict.ranking.iter().map(String::as_str).collect();
    if ranked != want || verdict.ranking.len() != candidates.len() {
        return Err(format!(
            "ranking must name every candidate once ({}), not {}",
            candidates.join(", "),
            verdict.ranking.join(", ")
        ));
    }
    for (what, keys) in [
        (
            "scores",
            verdict
                .scores
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
        ),
        (
            "reasons",
            verdict.reasons.keys().map(String::as_str).collect(),
        ),
    ] {
        if keys != want {
            return Err(format!("{what} must have exactly the candidates as keys"));
        }
    }
    if let Some((branch, score)) = verdict
        .scores
        .iter()
        .find(|(_, s)| !(s.is_finite() && (0.0..=100.0).contains(*s)))
    {
        return Err(format!("{branch}'s score {score} is not from 0 to 100"));
    }
    Ok(verdict)
}

/// Who decided a judgement.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum JudgedBy {
    Deterministic,
    Judge {
        name: String,
    },
    /// The judge was asked, and its answer was not used.
    Fallback {
        name: String,
        error: String,
    },
}

impl JudgedBy {
    pub fn describe(&self) -> String {
        match self {
            JudgedBy::Deterministic => "deterministic".into(),
            JudgedBy::Judge { name } => name.clone(),
            JudgedBy::Fallback { name, .. } => format!("deterministic (fallback from {name})"),
        }
    }
}

/// One judged candidate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scored {
    /// From 1, best first.
    pub rank: u32,
    pub attempt: Attempt,
    /// The score without a model, from 0 to 100.
    pub deterministic: f64,
    /// The judge's, when its verdict was used.
    pub judge: Option<f64>,
    /// The one ranked on.
    pub score: f64,
    /// Whether it can be picked: it has a candidate, its turn completed and
    /// its check did not fail.
    pub eligible: bool,
    pub reason: String,
}

/// Candidates ranked, and the proposed pick.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Judgement {
    pub candidates: Vec<Scored>,
    /// The best eligible candidate, if any is.
    pub pick: Option<String>,
    pub by: JudgedBy,
}

/// Why an attempt cannot be picked, if it cannot.
fn ineligible(attempt: &Attempt) -> Option<String> {
    if attempt.candidate.is_none() {
        return Some("no candidate".into());
    }
    if !matches!(
        attempt.status,
        BranchStatus::Ready | BranchStatus::Merged { .. }
    ) {
        return Some("its last turn did not complete with changes".into());
    }
    match &attempt.check {
        AttemptCheck::Failed { .. } => Some("its check failed".into()),
        AttemptCheck::TimedOut { .. } => Some("its check timed out".into()),
        AttemptCheck::Error { message } => Some(format!("its check could not run: {message}")),
        _ => None,
    }
}

/// Lower is better, as a share from 0 (worst) to 1 (best) among `values`;
/// one half when unknown, one when all are equal.
fn better_when_lower(value: Option<f64>, values: &[f64]) -> f64 {
    let Some(value) = value else { return 0.5 };
    let (min, max) = values
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(*v), hi.max(*v))
        });
    if max <= min {
        return 1.0;
    }
    1.0 - (value - min) / (max - min)
}

/// The score without a model, from 0 to 100: the check (passed 60, none or
/// not run 40), the diff's size (smaller up to 20), cost (cheaper up to 10)
/// and time (faster up to 10), each relative to the other eligible
/// candidates. An ineligible candidate scores 0.
pub fn deterministic_scores(attempts: &[Attempt]) -> Vec<(f64, String)> {
    let eligible: Vec<&Attempt> = attempts
        .iter()
        .filter(|a| ineligible(a).is_none())
        .collect();
    let lines: Vec<f64> = eligible
        .iter()
        .map(|a| f64::from(a.insertions + a.deletions))
        .collect();
    let costs: Vec<f64> = eligible.iter().filter_map(|a| a.cost_usd).collect();
    let times: Vec<f64> = eligible
        .iter()
        .filter_map(|a| a.duration_ms.map(|d| d as f64))
        .collect();
    attempts
        .iter()
        .map(|a| {
            if let Some(why) = ineligible(a) {
                return (0.0, why);
            }
            let (check, said) = match a.check {
                AttemptCheck::Passed | AttemptCheck::PassedAtMerge => (60.0, "check passed"),
                AttemptCheck::None => (40.0, "no check"),
                _ => (40.0, "check not run"),
            };
            let size =
                20.0 * better_when_lower(Some(f64::from(a.insertions + a.deletions)), &lines);
            let cost = 10.0 * better_when_lower(a.cost_usd, &costs);
            let time = 10.0 * better_when_lower(a.duration_ms.map(|d| d as f64), &times);
            let score = (check + size + cost + time).min(100.0);
            (
                (score * 10.0).round() / 10.0,
                format!(
                    "{said}; +{} -{} in {} file{}",
                    a.insertions,
                    a.deletions,
                    a.files_changed,
                    if a.files_changed == 1 { "" } else { "s" }
                ),
            )
        })
        .collect()
}

const RUBRIC: &str = "\
Judge which attempt best does the task. In order of weight: it does what the task asks and \
nothing else is broken (a passing check counts); it changes only what the task needs; it is \
clear and maintainable; it is small. Do not reward length.";

/// The prompt a judge harness gets.
pub fn prompt(task: &str, attempts: &[Attempt], diffs: &[String], rubric: Option<&str>) -> String {
    let mut text = format!(
        "You are judging {} attempts at one coding task. Read only: do not change, create or \
         run anything.\n\n## Task\n{task}\n\n## Rubric\n{RUBRIC}\n",
        attempts.len()
    );
    if let Some(rubric) = rubric {
        text.push_str(&format!("{rubric}\n"));
    }
    text.push_str("\n## Candidates\n");
    for (attempt, diff) in attempts.iter().zip(diffs) {
        text.push_str(&format!(
            "\n### {}\nharness {}; status {}; check {}; {} file(s), +{} -{}; {} turn(s); cost {}; \
             time {}\n```diff\n{}\n```\n",
            attempt.branch,
            attempt.harness,
            serde_json::to_value(&attempt.status)
                .ok()
                .and_then(|v| v["state"].as_str().map(str::to_owned))
                .unwrap_or_default(),
            attempt.check.word(),
            attempt.files_changed,
            attempt.insertions,
            attempt.deletions,
            attempt.turns,
            attempt
                .cost_usd
                .map_or("unknown".into(), |c| format!("${c:.2}")),
            attempt
                .duration_ms
                .map_or("unknown".into(), |d| format!("{}s", d / 1000)),
            diff.trim_end()
        ));
    }
    let names: Vec<String> = attempts
        .iter()
        .map(|a| format!("\"{}\"", a.branch))
        .collect();
    text.push_str(&format!(
        "\n## Answer\nReply with only one JSON object and nothing else, of this shape, naming \
         every candidate ({}) exactly once in each part:\n\
         {{\"ranking\": [best first], \"scores\": {{\"<branch>\": 0 to 100}}, \"reasons\": \
         {{\"<branch>\": \"one sentence\"}}}}\n",
        names.join(", ")
    ));
    text
}

fn truncated(diff: String) -> String {
    let count = diff.chars().count();
    if count <= DIFF_MAX {
        return diff;
    }
    let mut kept: String = diff.chars().take(DIFF_MAX).collect();
    kept.push_str(&format!("\n... ({} more characters)", count - DIFF_MAX));
    kept
}

/// Judge `names`: gather and optionally check them as `by compare`, score
/// them, ask the judge if there is one, rank, and propose the best
/// eligible candidate. With `options.record`, each candidate gets an
/// [`FleetActivity::Judged`] event and its score in the outcome store.
pub(crate) fn judge(
    yard: &Yard,
    names: &[String],
    options: &JudgeOptions,
) -> Result<Judgement, Error> {
    if names.is_empty() {
        return Err(Error::State("nothing to judge".into()));
    }
    let attempts = crate::compare::compare(yard, names, options.run_checks)?;
    let deterministic = deterministic_scores(&attempts);
    let store = yard.store();
    let mut by = JudgedBy::Deterministic;
    let mut verdict = None;
    if let Some(judge) = &options.judge {
        let task = store.read(&attempts[0].branch)?.info.prompt;
        let mut diffs = Vec::new();
        for attempt in &attempts {
            let record = store.read(&attempt.branch)?;
            diffs.push(match &attempt.candidate {
                Some(commit) => truncated(git::diff(yard.root(), &record.info.base, commit)?),
                None => "(no candidate)".into(),
            });
        }
        let text = prompt(&task, &attempts, &diffs, options.rubric.as_deref());
        let asked = judge
            .verdict(yard, &text, names)
            .map_err(|e| e.to_string())
            .and_then(|answer| parse_verdict(&answer, names));
        match asked {
            Ok(parsed) => {
                by = JudgedBy::Judge { name: judge.name() };
                verdict = Some(parsed);
            }
            Err(error) => {
                by = JudgedBy::Fallback {
                    name: judge.name(),
                    error,
                }
            }
        }
    }
    let mut scored: Vec<Scored> = attempts
        .into_iter()
        .zip(deterministic)
        .map(|(attempt, (det, why))| {
            let eligible = ineligible(&attempt).is_none();
            let judged = verdict.as_ref().map(|v| {
                (
                    v.scores[&attempt.branch],
                    v.reasons[&attempt.branch].clone(),
                )
            });
            let (score, reason) = match (&judged, eligible) {
                (Some((s, r)), true) => (*s, r.clone()),
                (Some((s, r)), false) => (*s, format!("{r} (not pickable: {why})")),
                (None, _) => (det, why),
            };
            Scored {
                rank: 0,
                judge: judged.map(|(s, _)| s),
                attempt,
                deterministic: det,
                score,
                eligible,
                reason,
            }
        })
        .collect();
    match &verdict {
        Some(v) => scored.sort_by_key(|s| {
            v.ranking
                .iter()
                .position(|b| *b == s.attempt.branch)
                .unwrap_or(usize::MAX)
        }),
        None => scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(a.attempt.turns.cmp(&b.attempt.turns))
                .then(a.attempt.branch.cmp(&b.attempt.branch))
        }),
    }
    for (rank, s) in scored.iter_mut().enumerate() {
        s.rank = rank as u32 + 1;
    }
    let pick = scored
        .iter()
        .find(|s| s.eligible)
        .map(|s| s.attempt.branch.clone());
    let judgement = Judgement {
        candidates: scored,
        pick,
        by,
    };
    if options.record {
        record(yard, &judgement)?;
        // The pick may teach the repository something, as proposals.
        if let Some(pick) = &judgement.pick {
            crate::knowledge::on_end(yard, pick, crate::knowledge::DistillTrigger::JudgedBest);
        }
    }
    Ok(judgement)
}

/// Each candidate's score as an event on it and in the outcome store.
fn record(yard: &Yard, judgement: &Judgement) -> Result<(), Error> {
    let store = yard.store();
    let of = judgement.candidates.len() as u32;
    for s in &judgement.candidates {
        let picked = judgement.pick.as_deref() == Some(s.attempt.branch.as_str());
        let mark = JudgeMark {
            score: s.score,
            rank: s.rank,
            of,
            picked,
            by: judgement.by.describe(),
            reason: Some(s.reason.clone()),
            check: s.attempt.check.word().to_owned(),
        };
        crate::record::Recorder::open(&store, &s.attempt.branch, None)?
            .record(Activity::Fleet(Box::new(FleetActivity::Judged(mark))))?;
        fleet::observe(yard, &s.attempt.branch, Some((s.score, picked)))?;
    }
    Ok(())
}

/// The harness judge a fleet entry names, for `options`.
pub fn harness_judge(spec: &JudgeSpec, options: &TaskOptions) -> Arc<dyn Judge> {
    Arc::new(HarnessJudge {
        spec: spec.clone(),
        options: options.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(name: &str, check: AttemptCheck, lines: u32, cost: Option<f64>) -> Attempt {
        Attempt {
            branch: name.into(),
            harness: "h".into(),
            status: BranchStatus::Ready,
            turns: 1,
            cost_usd: cost,
            tokens: None,
            duration_ms: Some(1000),
            check,
            candidate: Some("c".into()),
            files_changed: 1,
            insertions: lines,
            deletions: 0,
            files: vec![],
            unique_files: vec![],
        }
    }

    #[test]
    fn deterministic_scores_prefer_passing_small_cheap_attempts() {
        let attempts = vec![
            attempt("a", AttemptCheck::Passed, 10, Some(1.0)),
            attempt("b", AttemptCheck::Passed, 30, Some(2.0)),
            attempt(
                "c",
                AttemptCheck::Failed {
                    output_tail: "x".into(),
                },
                1,
                Some(0.1),
            ),
            attempt("d", AttemptCheck::None, 10, None),
        ];
        let scores = deterministic_scores(&attempts);
        assert_eq!(scores[0].0, 60.0 + 20.0 + 10.0 + 10.0);
        assert_eq!(scores[1].0, 60.0 + 0.0 + 0.0 + 10.0);
        assert_eq!(scores[2], (0.0, "its check failed".into()));
        assert_eq!(scores[3].0, 40.0 + 20.0 + 5.0 + 10.0);
    }

    #[test]
    fn verdicts_are_parsed_strictly() {
        let names = vec!["a".to_owned(), "b".to_owned()];
        let good = r#"{"ranking": ["b", "a"], "scores": {"a": 40, "b": 90},
            "reasons": {"a": "misses a case", "b": "complete"}}"#;
        let verdict = parse_verdict(good, &names).unwrap();
        assert_eq!(verdict.ranking, ["b", "a"]);
        assert!(parse_verdict(&format!("```json\n{good}\n```"), &names).is_ok());
        for bad in [
            "b is best",
            &format!("Here you go: {good}"),
            &format!("{good} thanks"),
            r#"{"ranking": ["b"], "scores": {"a": 1, "b": 2}, "reasons": {"a": "", "b": ""}}"#,
            r#"{"ranking": ["b", "b"], "scores": {"a": 1, "b": 2}, "reasons": {"a": "", "b": ""}}"#,
            r#"{"ranking": ["b", "a"], "scores": {"a": 1, "b": 200}, "reasons": {"a": "", "b": ""}}"#,
            r#"{"ranking": ["b", "a"], "scores": {"a": 1}, "reasons": {"a": "", "b": ""}}"#,
            r#"{"ranking": ["b", "a"], "scores": {"a": 1, "b": 2}, "reasons": {"a": "", "b": ""}, "x": 1}"#,
            r#"{"ranking": ["b", "c"], "scores": {"a": 1, "b": 2}, "reasons": {"a": "", "b": ""}}"#,
        ] {
            assert!(parse_verdict(bad, &names).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_prompt_lists_every_candidate_and_the_answer_shape() {
        let attempts = vec![
            attempt("a", AttemptCheck::Passed, 1, None),
            attempt("b", AttemptCheck::None, 2, Some(0.5)),
        ];
        let text = prompt(
            "Fix it",
            &attempts,
            &["+x".into(), "+y".into()],
            Some("Prefer b."),
        );
        assert!(text.contains("## Task\nFix it"));
        assert!(text.contains("### a\n") && text.contains("### b\n"));
        assert!(text.contains("Prefer b."));
        assert!(text.contains("\"a\", \"b\""));
        assert!(truncated("x".repeat(DIFF_MAX + 5)).ends_with("(5 more characters)"));
    }
}
