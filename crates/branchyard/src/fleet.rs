//! Routing tasks to harnesses: task kinds and their classifier, the fleet
//! table, the outcome store the router learns from, the router itself, and
//! failover to the next candidate when a harness (not the task) fails. See
//! `docs/fleet.md`.
//!
//! Nothing here calls a model. The classifier is keyword rules, the router
//! is Thompson sampling over recorded outcomes with a seeded generator, and
//! failover reads the status a turn ended with.

use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{
    harness, placement, run, Activity, Branch, BranchStatus, Budget, Effort, Error, Provisioning,
    RecordedEvent, TaskOptions, Yard,
};
use branchyard_support::rng::SplitMix64;
use branchyard_support::time::now_ms;

// ---------------------------------------------------------------------------
// Task kinds

/// What a task is, for choosing who does it. Given with `--kind`, or
/// inferred from the prompt by [`classify`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(strum::Display, strum::EnumIter, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum TaskKind {
    Bugfix,
    Feature,
    Refactor,
    Review,
    Research,
    Docs,
    Migration,
    Tests,
    Other,
}

impl TaskKind {
    /// Every kind, in the order tables list them.
    pub const ALL: [TaskKind; 9] = [
        TaskKind::Bugfix,
        TaskKind::Feature,
        TaskKind::Refactor,
        TaskKind::Review,
        TaskKind::Research,
        TaskKind::Docs,
        TaskKind::Migration,
        TaskKind::Tests,
        TaskKind::Other,
    ];

    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

impl FromStr for TaskKind {
    type Err = Error;
    fn from_str(text: &str) -> Result<TaskKind, Error> {
        TaskKind::ALL
            .into_iter()
            .find(|k| k.as_str() == text)
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "{text:?} is not a task kind; use one of {}",
                    TaskKind::ALL.map(TaskKind::as_str).join(", ")
                ))
            })
    }
}

/// Keyword stems per kind, in tie-breaking order: when two kinds score the
/// same, the earlier one wins. A stem matches a word of the prompt that
/// starts with it (`fix` matches `fixes`, `doc` matches `documentation`).
pub const KEYWORDS: &[(TaskKind, &[&str])] = &[
    (
        TaskKind::Review,
        &["review", "audit", "critique", "proofread"],
    ),
    (
        TaskKind::Migration,
        &["migrat", "upgrad", "bump", "deprecat", "port"],
    ),
    (
        TaskKind::Refactor,
        &[
            "refactor",
            "restructur",
            "reorganiz",
            "rename",
            "extract",
            "simplif",
            "cleanup",
            "clean",
            "dedup",
            "tidy",
            "split",
        ],
    ),
    (TaskKind::Tests, &["test", "coverage", "fixture", "assert"]),
    (
        TaskKind::Docs,
        &[
            "doc",
            "readme",
            "comment",
            "changelog",
            "tutorial",
            "guide",
            "explain",
        ],
    ),
    (
        TaskKind::Bugfix,
        &[
            "fix",
            "bug",
            "crash",
            "broke",
            "regress",
            "fail",
            "panic",
            "error",
            "wrong",
            "incorrect",
            "flaky",
            "leak",
            "hang",
        ],
    ),
    (
        TaskKind::Research,
        &[
            "research",
            "investigat",
            "explor",
            "evaluat",
            "survey",
            "analy",
            "why",
            "compare",
            "benchmark",
            "find",
        ],
    ),
    (
        TaskKind::Feature,
        &[
            "add",
            "implement",
            "support",
            "feature",
            "create",
            "build",
            "introduc",
            "new",
            "enable",
            "allow",
        ],
    ),
];

/// A prompt's kind and the words that decided it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub kind: TaskKind,
    /// The prompt's words that matched a stem of the chosen kind.
    pub matched: Vec<String>,
}

/// Infer a task's kind from its prompt, with no model call: each word
/// (lowercase letters and digits) that starts with one of a kind's
/// [`KEYWORDS`] scores a point for that kind, and the prompt's first word,
/// usually the imperative verb, scores three. The highest score wins, ties
/// going to the kind listed first in [`KEYWORDS`]; no match is
/// [`TaskKind::Other`].
pub fn classify(prompt: &str) -> Classification {
    let lower = prompt.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let mut best: Option<(u32, usize, TaskKind, Vec<String>)> = None;
    for (order, (kind, stems)) in KEYWORDS.iter().enumerate() {
        let mut score = 0;
        let mut matched = Vec::new();
        for (index, word) in words.iter().enumerate() {
            if stems.iter().any(|stem| word.starts_with(stem)) {
                score += if index == 0 { 3 } else { 1 };
                if !matched.contains(&word.to_string()) {
                    matched.push(word.to_string());
                }
            }
        }
        let better = match &best {
            None => score > 0,
            Some((top, top_order, _, _)) => score > *top || (score == *top && order < *top_order),
        };
        if better && score > 0 {
            best = Some((score, order, *kind, matched));
        }
    }
    match best {
        Some((_, _, kind, matched)) => Classification { kind, matched },
        None => Classification {
            kind: TaskKind::Other,
            matched: Vec::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// The fleet table

/// One candidate of a fleet entry: a harness, and optionally the model and
/// reasoning effort it runs with.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetCandidate {
    /// Harness or profile ID.
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Executable and fixed arguments replacing the profile's, as
    /// [`TaskOptions::command`]; for development and testing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
}

impl FleetCandidate {
    pub fn new(harness: impl Into<String>) -> FleetCandidate {
        FleetCandidate {
            harness: harness.into(),
            model: None,
            effort: None,
            command: None,
        }
    }

    /// `harness`, then ` model=M` and ` effort=E` when set: how tables and
    /// statistics name it.
    pub fn label(&self) -> String {
        let mut label = self.harness.clone();
        if let Some(model) = &self.model {
            label.push_str(&format!(" model={model}"));
        }
        if let Some(effort) = &self.effort {
            label.push_str(&format!(" effort={}", effort_text(*effort)));
        }
        label
    }

    fn same_arm(&self, harness: &str, model: Option<&str>, effort: Option<&str>) -> bool {
        self.harness == harness
            && self.model.as_deref() == model
            && self.effort.map(effort_text) == effort.map(str::to_owned)
    }
}

pub(crate) fn effort_text(effort: Effort) -> String {
    serde_json::to_value(effort)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// A judge harness: run read-only on a scratch branch with a rubric and
/// the candidates' diffs, answering a JSON verdict. See [`crate::judge`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgeSpec {
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Added to the judge's prompt under `## Rubric`, after the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric: Option<String>,
}

/// One row of the fleet table: `[fleet.<kind>]` or `[fleet.default]`.
#[derive(Clone, Debug, PartialEq)]
pub struct FleetEntry {
    /// In order of preference; the router starts from this order and
    /// learns from outcomes.
    pub candidates: Vec<FleetCandidate>,
    /// Branches a routed fan starts (best of N). A run starts one.
    pub attempts: u32,
    /// Limits for each attempt, filling what the task's own budget leaves
    /// unset. A failover chain shares the cost limit.
    pub budget: Budget,
    pub judge: Option<JudgeSpec>,
    /// Fail over to the next candidate when a harness fails.
    pub failover: bool,
    /// The chance, from 0 to 1, that the router picks a candidate at random
    /// instead of by its sampled success rate.
    pub exploration: f64,
    /// Passed through for other tracks; recorded on the branch, not acted on.
    pub environment: Option<String>,
    /// Passed through for other tracks; recorded on the branch, not acted on.
    pub connectors: Vec<String>,
    /// Plan first: routed branches of this kind start with a read-only
    /// planning turn and wait for approval; see `docs/plans-and-goals.md`.
    pub plan: bool,
    /// The judge of a goal (`--goal`) given to a branch of this kind when
    /// the task names none.
    pub goal_judge: Option<JudgeSpec>,
}

impl Default for FleetEntry {
    fn default() -> FleetEntry {
        FleetEntry {
            candidates: Vec::new(),
            attempts: 1,
            budget: Budget::default(),
            judge: None,
            failover: false,
            exploration: DEFAULT_EXPLORATION,
            environment: None,
            connectors: Vec::new(),
            plan: false,
            goal_judge: None,
        }
    }
}

/// The exploration floor when an entry sets none.
pub const DEFAULT_EXPLORATION: f64 = 0.1;

/// The fleet table: entries by task kind, and `default`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fleet {
    pub entries: BTreeMap<String, FleetEntry>,
}

impl Fleet {
    /// The entry for `kind`, else `default`, with the key it was found at.
    pub fn entry(&self, kind: TaskKind) -> Option<(&str, &FleetEntry)> {
        self.entries
            .get_key_value(kind.as_str())
            .or_else(|| self.entries.get_key_value("default"))
            .map(|(k, v)| (k.as_str(), v))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Fleet events

/// What the router decided for a branch, recorded on it before its first
/// turn as [`FleetActivity::Routed`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteDecision {
    pub kind: TaskKind,
    /// `flag` (given with `--kind`) or `classifier`.
    pub kind_source: String,
    /// The fleet entry used (`bugfix`, `default`), or `None` when the
    /// harness was named and only the kind is recorded.
    pub entry: Option<String>,
    pub candidate: FleetCandidate,
    /// This branch's attempt, from 1, of `attempts`.
    pub attempt: u32,
    pub attempts: u32,
    /// Why this candidate, in words.
    pub reason: String,
    /// Whether this branch fails over to `fallbacks` when its harness
    /// fails.
    pub failover: bool,
    /// Candidates a failover may move to, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<FleetCandidate>,
    /// Candidates this failover chain has already tried.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tried: Vec<FleetCandidate>,
    /// The chain's cost limit, and what branches before this one spent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub spent_usd: f64,
    /// The branch this one failed over from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connectors: Vec<String>,
}

/// A judge's score for one candidate, recorded on it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JudgeMark {
    /// From 0 to 100.
    pub score: f64,
    /// From 1.
    pub rank: u32,
    pub of: u32,
    /// Whether the judge proposed this candidate.
    pub picked: bool,
    /// `deterministic`, or the judge harness and whether its verdict was
    /// used.
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The candidate's check, as `by compare` words it.
    pub check: String,
}

/// Routing, failover and judging, recorded as [`Activity::Fleet`].
///
/// Serialized as an object tagged by `type` in snake case.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FleetActivity {
    /// The router chose this branch's harness, or the kind was recorded.
    Routed(Box<RouteDecision>),
    /// This branch's harness failed, and the task moves to `next`.
    FailedOver {
        reason: String,
        next: FleetCandidate,
    },
    /// This branch is a judge's scratch branch for these candidates.
    Judging { candidates: Vec<String> },
    /// A judge scored this branch.
    Judged(JudgeMark),
}

impl FleetActivity {
    /// One line for logs.
    pub fn describe(&self) -> String {
        match self {
            FleetActivity::Routed(d) => match &d.entry {
                Some(entry) => format!(
                    "routed {} ({}) by [fleet.{entry}] to {}{}: {}",
                    d.kind,
                    d.kind_source,
                    d.candidate.label(),
                    match d.attempts {
                        1 => String::new(),
                        n => format!(", attempt {} of {n}", d.attempt),
                    },
                    d.reason
                ),
                None => format!("kind {} ({})", d.kind, d.kind_source),
            },
            FleetActivity::FailedOver { reason, next } => {
                format!("failing over to {}: {reason}", next.label())
            }
            FleetActivity::Judging { candidates } => {
                format!("judging {}", candidates.join(", "))
            }
            FleetActivity::Judged(mark) => format!(
                "judged {:.0}/100, rank {} of {}{} by {}{}",
                mark.score,
                mark.rank,
                mark.of,
                if mark.picked { ", proposed pick" } else { "" },
                mark.by,
                mark.reason
                    .as_deref()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default()
            ),
        }
    }
}

/// The last routing decision recorded on a branch.
pub fn recorded_route(events: &[RecordedEvent]) -> Option<RouteDecision> {
    events.iter().rev().find_map(|e| match &e.activity {
        Activity::Fleet(activity) => match activity.as_ref() {
            FleetActivity::Routed(decision) => Some(decision.as_ref().clone()),
            _ => None,
        },
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Outcomes

/// How a finished branch turned out, for the router.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(strum::Display, strum::EnumString, strum::EnumIter, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum BranchOutcome {
    Merged,
    JudgedBest,
    /// Ended with a candidate, not (yet) merged or judged best.
    Ready,
    /// Failed, stopped at a limit, or changed nothing.
    Failed,
    Interrupted,
}

impl BranchOutcome {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(text: &str) -> Result<BranchOutcome, Error> {
        Ok(crate::store_codec::parse_text("outcome", text)?)
    }

    /// The outcome a branch's status says, or `None` while it has not
    /// finished a turn.
    pub fn of(status: &BranchStatus) -> Option<BranchOutcome> {
        match status {
            BranchStatus::Running
            | BranchStatus::Waiting
            | BranchStatus::Blocked { .. }
            | BranchStatus::AwaitingPlanApproval => None,
            BranchStatus::Ready => Some(BranchOutcome::Ready),
            BranchStatus::Merged { .. } => Some(BranchOutcome::Merged),
            BranchStatus::Interrupted => Some(BranchOutcome::Interrupted),
            BranchStatus::NoChanges
            | BranchStatus::BudgetExceeded { .. }
            | BranchStatus::Failed { .. } => Some(BranchOutcome::Failed),
        }
    }
}

/// One finished branch's outcome, kept after the branch is removed.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// `<branch>#<created ms>`: one per created branch, even when a name
    /// is reused.
    pub id: String,
    /// The repository: its root, locally.
    pub repo: String,
    pub branch: String,
    pub kind: TaskKind,
    pub harness: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub outcome: BranchOutcome,
    /// The latest judge score, from 0 to 100.
    pub score: Option<f64>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub turns: u32,
    /// Whether the router chose its harness.
    pub routed: bool,
    pub recorded_ms: u64,
}

/// The outcome store: one row per created top-level branch, replaced as
/// it goes on. Rows outlive their branches.
pub(crate) trait OutcomeBackend: Send + Sync + fmt::Debug {
    /// Insert the row, or replace the one with its `id`.
    fn put_outcome(&self, row: &OutcomeRecord) -> Result<(), Error>;
    fn outcome(&self, id: &str) -> Result<Option<OutcomeRecord>, Error>;
    /// Every row, or those of `kind`, oldest first.
    fn outcomes(&self, kind: Option<TaskKind>) -> Result<Vec<OutcomeRecord>, Error>;
}

fn outcome_id(branch: &str, created_ms: u64) -> String {
    format!("{branch}#{created_ms}")
}

/// Record or update `name`'s outcome after a turn or a merge: top-level
/// branches only, not a judge's scratch branch. `judged` sets the score
/// and, when picked, `judged_best`. Best-effort for callers: it never
/// changes what happened to the branch.
pub(crate) fn observe(
    yard: &Yard,
    name: &str,
    judged: Option<(f64, bool)>,
) -> Result<Option<OutcomeRecord>, Error> {
    let store = yard.store();
    let record = store.read(name)?;
    if record.info.depth != 0 {
        return Ok(None);
    }
    let events = crate::record::read(&store, name)?;
    if events.iter().any(|e| {
        matches!(&e.activity, Activity::Fleet(a) if matches!(a.as_ref(), FleetActivity::Judging { .. }))
    }) {
        return Ok(None);
    }
    let Some(mut outcome) = BranchOutcome::of(&record.info.status) else {
        return Ok(None);
    };
    let id = outcome_id(name, record.created_ms);
    let previous = store.outcomes().outcome(&id)?;
    let route = recorded_route(&events);
    let kind = route
        .as_ref()
        .map(|d| d.kind)
        .unwrap_or_else(|| classify(&record.info.prompt).kind);
    let mut score = previous.as_ref().and_then(|p| p.score);
    if let Some((judge_score, picked)) = judged {
        score = Some(judge_score);
        if picked && outcome == BranchOutcome::Ready {
            outcome = BranchOutcome::JudgedBest;
        }
    }
    // Judged best stays until the branch merges or a later turn fails.
    if previous.as_ref().map(|p| p.outcome) == Some(BranchOutcome::JudgedBest)
        && outcome == BranchOutcome::Ready
    {
        outcome = BranchOutcome::JudgedBest;
    }
    let provision = record.provision.as_ref();
    let row = OutcomeRecord {
        id,
        repo: yard.root().display().to_string(),
        branch: name.to_owned(),
        kind,
        harness: record.info.harness.clone(),
        model: provision.and_then(|p| p.model.clone()),
        effort: provision.and_then(|p| p.effort).map(effort_text),
        outcome,
        score,
        cost_usd: record.info.cost_usd,
        duration_ms: crate::compare::attempt(&record.info, &events, Vec::new()).duration_ms,
        turns: record.info.turns,
        routed: route.as_ref().is_some_and(|d| d.entry.is_some()),
        recorded_ms: now_ms(),
    };
    store.outcomes().put_outcome(&row)?;
    Ok(Some(row))
}

/// How much one outcome counts as a success, from 0 to 1, or `None` when
/// it says nothing about the candidate (interrupted). Merged and judged
/// best are successes; a ready branch counts its judge score, or one half
/// unjudged; a failure counts nothing.
pub fn credit(row: &OutcomeRecord) -> Option<f64> {
    match row.outcome {
        BranchOutcome::Merged | BranchOutcome::JudgedBest => Some(1.0),
        BranchOutcome::Ready => Some(row.score.map_or(0.5, |s| (s / 100.0).clamp(0.0, 1.0))),
        BranchOutcome::Failed => Some(0.0),
        BranchOutcome::Interrupted => None,
    }
}

/// What the outcome store says of one (kind, harness, model, effort).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateStats {
    pub kind: TaskKind,
    pub harness: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub runs: u32,
    pub merged: u32,
    pub judged_best: u32,
    pub ready: u32,
    pub failed: u32,
    pub interrupted: u32,
    /// Summed [`credit`]: the Beta posterior is (1 + successes, 1 + failures).
    pub successes: f64,
    pub failures: f64,
    pub mean_cost_usd: Option<f64>,
    pub mean_duration_ms: Option<u64>,
    pub mean_turns: Option<f64>,
    pub mean_score: Option<f64>,
}

impl CandidateStats {
    /// The posterior mean success rate.
    pub fn expected(&self) -> f64 {
        (1.0 + self.successes) / (2.0 + self.successes + self.failures)
    }

    pub fn label(&self) -> String {
        let mut candidate = FleetCandidate::new(&self.harness);
        candidate.model = self.model.clone();
        let mut label = candidate.label();
        if let Some(effort) = &self.effort {
            label.push_str(&format!(" effort={effort}"));
        }
        label
    }
}

/// Group outcomes by kind and candidate, sorted by kind then label.
pub fn stats(rows: &[OutcomeRecord]) -> Vec<CandidateStats> {
    type Key = (TaskKind, String, Option<String>, Option<String>);
    let mut groups: BTreeMap<Key, Vec<&OutcomeRecord>> = BTreeMap::new();
    for row in rows {
        groups
            .entry((
                row.kind,
                row.harness.clone(),
                row.model.clone(),
                row.effort.clone(),
            ))
            .or_default()
            .push(row);
    }
    let mean = |values: Vec<f64>| -> Option<f64> {
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    groups
        .into_iter()
        .map(|((kind, harness, model, effort), rows)| {
            let count = |o: BranchOutcome| rows.iter().filter(|r| r.outcome == o).count() as u32;
            let credits: Vec<f64> = rows.iter().filter_map(|r| credit(r)).collect();
            CandidateStats {
                kind,
                harness,
                model,
                effort,
                runs: rows.len() as u32,
                merged: count(BranchOutcome::Merged),
                judged_best: count(BranchOutcome::JudgedBest),
                ready: count(BranchOutcome::Ready),
                failed: count(BranchOutcome::Failed),
                interrupted: count(BranchOutcome::Interrupted),
                successes: credits.iter().sum(),
                failures: credits.iter().map(|c| 1.0 - c).sum(),
                mean_cost_usd: mean(rows.iter().filter_map(|r| r.cost_usd).collect()),
                mean_duration_ms: mean(
                    rows.iter()
                        .filter_map(|r| r.duration_ms.map(|d| d as f64))
                        .collect(),
                )
                .map(|d| d as u64),
                mean_turns: mean(rows.iter().map(|r| f64::from(r.turns)).collect()),
                mean_score: mean(rows.iter().filter_map(|r| r.score).collect()),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The router

/// A seeded generator for the router, [`SplitMix64`]: the same everywhere,
/// so a seeded route is reproducible.
#[derive(Clone, Debug)]
pub struct Rng(SplitMix64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(SplitMix64::new(seed))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    /// Uniform in the open interval (0, 1).
    pub fn uniform(&mut self) -> f64 {
        self.0.uniform()
    }

    fn normal(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }

    /// Gamma(shape, 1), Marsaglia and Tsang; boosted for shape < 1.
    fn gamma(&mut self, shape: f64) -> f64 {
        if shape < 1.0 {
            return self.gamma(shape + 1.0) * self.uniform().powf(1.0 / shape);
        }
        let d = shape - 1.0 / 3.0;
        let c = 1.0 / (9.0 * d).sqrt();
        loop {
            let x = self.normal();
            let v = (1.0 + c * x).powi(3);
            if v <= 0.0 {
                continue;
            }
            let u = self.uniform();
            if u.ln() < 0.5 * x * x + d - d * v + d * v.ln() {
                return d * v;
            }
        }
    }

    /// Beta(a, b).
    pub fn beta(&mut self, a: f64, b: f64) -> f64 {
        let x = self.gamma(a);
        let y = self.gamma(b);
        x / (x + y)
    }
}

/// A seed from the clock and the process, for unseeded routes.
pub fn fresh_seed() -> u64 {
    branchyard_support::rng::fresh_seed()
}

/// One candidate the router chose.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutePick {
    pub candidate: FleetCandidate,
    /// Its position in the entry's list.
    pub index: usize,
    /// The sampled success rate, when picked by sampling.
    pub sample: Option<f64>,
    /// Picked at random under the exploration floor.
    pub explored: bool,
    pub reason: String,
}

/// A candidate the router would not pick, and why.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Excluded {
    pub candidate: FleetCandidate,
    pub reason: String,
}

/// What the router decided for a task.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub kind: TaskKind,
    pub kind_source: String,
    /// The words that decided a classified kind.
    pub matched: Vec<String>,
    pub entry: String,
    pub picks: Vec<RoutePick>,
    pub excluded: Vec<Excluded>,
    pub seed: u64,
}

/// Why `candidate` cannot be picked now, if it cannot: its harness is
/// unknown or unavailable (the check `by harnesses` makes), or refused for
/// its approvals, or its recorded mean cost is over `max_usd`.
pub(crate) type Availability<'a> = &'a dyn Fn(&FleetCandidate) -> Result<(), String>;

/// Choose `attempts` candidates from `entry`: each available and within
/// budget; with probability `entry.exploration` one at random, otherwise
/// the highest draw from Beta(1 + successes, 1 + failures) of its recorded
/// outcomes for this kind, ties going to the earlier candidate. Each round
/// picks without replacement; more attempts than candidates start another
/// round.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan(
    kind: TaskKind,
    kind_source: &str,
    matched: Vec<String>,
    entry_name: &str,
    entry: &FleetEntry,
    history: &[CandidateStats],
    available: Availability<'_>,
    attempts: u32,
    seed: u64,
) -> Result<Route, Error> {
    let mut rng = Rng::new(seed);
    let find = |c: &FleetCandidate| {
        history.iter().find(|s| {
            s.kind == kind && c.same_arm(&s.harness, s.model.as_deref(), s.effort.as_deref())
        })
    };
    let mut eligible = Vec::new();
    let mut excluded = Vec::new();
    for (index, candidate) in entry.candidates.iter().enumerate() {
        let over = match (
            entry.budget.max_usd,
            find(candidate).and_then(|s| s.mean_cost_usd),
        ) {
            (Some(max), Some(mean)) if mean > max => Some(format!(
                "its mean recorded cost ${mean:.2} is over the ${max:.2} budget"
            )),
            _ => None,
        };
        match available(candidate).err().or(over) {
            Some(reason) => excluded.push(Excluded {
                candidate: candidate.clone(),
                reason,
            }),
            None => eligible.push(index),
        }
    }
    if eligible.is_empty() {
        let why: Vec<String> = excluded
            .iter()
            .map(|e| format!("{}: {}", e.candidate.label(), e.reason))
            .collect();
        return Err(Error::HarnessUnavailable {
            harness: format!("[fleet.{entry_name}]"),
            reason: match why.is_empty() {
                true => "it lists no candidates".into(),
                false => format!("no candidate can run: {}", why.join("; ")),
            },
        });
    }
    let exploration = entry.exploration.clamp(0.0, 1.0);
    let mut picks = Vec::new();
    let mut pool: Vec<usize> = Vec::new();
    for _ in 0..attempts.max(1) {
        if pool.is_empty() {
            pool = eligible.clone();
        }
        let explore = pool.len() > 1 && rng.uniform() < exploration;
        let (at, sample, reason) = if explore {
            let at = (rng.next_u64() % pool.len() as u64) as usize;
            (
                at,
                None,
                format!("explored: picked at random (exploration floor {exploration:.2})"),
            )
        } else {
            let mut best: Option<(usize, f64)> = None;
            for (at, index) in pool.iter().enumerate() {
                let stats = find(&entry.candidates[*index]);
                let (s, f) = stats.map_or((0.0, 0.0), |s| (s.successes, s.failures));
                let draw = rng.beta(1.0 + s, 1.0 + f);
                if best.is_none_or(|(_, top)| draw > top) {
                    best = Some((at, draw));
                }
            }
            let (at, draw) = best.expect("the pool is not empty");
            let stats = find(&entry.candidates[pool[at]]);
            let history = match stats {
                Some(s) if s.runs > 0 => format!(
                    "{} recorded {kind} outcome{}, {:.1} successes",
                    s.runs,
                    if s.runs == 1 { "" } else { "s" },
                    s.successes
                ),
                _ => format!("no recorded {kind} outcomes"),
            };
            (
                at,
                Some(draw),
                format!("sampled success rate {draw:.2} ({history})"),
            )
        };
        let index = pool.remove(at);
        picks.push(RoutePick {
            candidate: entry.candidates[index].clone(),
            index,
            sample,
            explored: explore,
            reason,
        });
    }
    Ok(Route {
        kind,
        kind_source: kind_source.to_owned(),
        matched,
        entry: entry_name.to_owned(),
        picks,
        excluded,
        seed,
    })
}

/// Whether `candidate` can run now under `options`: its profile exists, is
/// allowed, and (run locally) its executable is found and, with
/// `harnesses`, this machine's inventory says it can run (installed, not
/// logged out, not at a usage limit), perhaps after installing it.
pub(crate) fn availability(
    yard: &Yard,
    options: &TaskOptions,
    candidate: &FleetCandidate,
    harnesses: Option<&dyn crate::inventory::HarnessGate>,
) -> Result<(), String> {
    let profile = harness::select(Some(&candidate.harness)).map_err(|e| e.to_string())?;
    harness::check_approvals(profile, options.unapproved_tools).map_err(|e| e.to_string())?;
    placement::check(yard, options.provider.as_ref()).map_err(|e| e.to_string())?;
    if !placement::sandboxed(options.provider.as_ref()) {
        let replaced = candidate.command.as_deref().or(options.command.as_deref());
        // The harness by its own name: what this machine has of it, which
        // may install it first. A replaced command is checked as a path.
        if let (Some(gate), None) = (harnesses, replaced) {
            gate.check(profile.harness)?;
        }
        let command = harness::command(profile, replaced);
        harness::check_available(&candidate.harness, &command).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// How to route a task.
#[derive(Clone, Debug, Default)]
pub struct RouteOptions {
    /// The task's kind; inferred from the prompt when `None`.
    pub kind: Option<TaskKind>,
    /// Seed the router for a reproducible choice; a fresh seed otherwise.
    pub seed: Option<u64>,
    /// Attempts for a fan, instead of the entry's.
    pub attempts: Option<u32>,
    /// Fail over when a harness fails, whatever the entry says.
    pub failover: Option<bool>,
    /// Harnesses not to pick, with why: a login near its usage limit
    /// (`by usage`, `[usage] skip_over`). Such a candidate is excluded
    /// like an unavailable one.
    pub excluded: std::collections::BTreeMap<String, String>,
    /// What the machine that runs the task has of each harness
    /// (docs/harness-lifecycle.md): a candidate it says cannot run there
    /// is excluded, with its reason; it may install one first.
    pub harnesses: Option<std::sync::Arc<dyn crate::inventory::HarnessGate>>,
}

/// A routed run or fan: what the router chose, and the branches as they
/// ended, each the last of its failover chain.
#[derive(Debug)]
pub struct Routed {
    pub route: Route,
    pub branches: Vec<Branch>,
    /// (failed branch, the branch it failed over to), in order.
    pub failovers: Vec<(String, String)>,
}

fn kind_of(prompt: &str, given: Option<TaskKind>) -> (TaskKind, &'static str, Vec<String>) {
    match given {
        Some(kind) => (kind, "flag", Vec::new()),
        None => {
            let c = classify(prompt);
            (c.kind, "classifier", c.matched)
        }
    }
}

/// The task's provisioning with the candidate's model and effort in place
/// of the task's own, when it names them.
fn provision_for(base: Option<&Provisioning>, candidate: &FleetCandidate) -> Option<Provisioning> {
    let mut spec = base.cloned().unwrap_or_default();
    if candidate.model.is_some() {
        spec.model = candidate.model.clone();
    }
    if candidate.effort.is_some() {
        spec.effort = candidate.effort;
    }
    (!spec.is_empty()).then_some(spec)
}

/// The task's limits, with the entry's where the task sets none.
fn budget_for(task: &Budget, entry: &Budget) -> Budget {
    Budget {
        max_usd: task.max_usd.or(entry.max_usd),
        max_turns: task.max_turns.or(entry.max_turns),
        max_duration: task.max_duration.or(entry.max_duration),
        stall_after: task.stall_after.or(entry.stall_after),
        stall_action: task.stall_action,
    }
}

/// The route for `prompt` without running anything.
pub(crate) fn route(
    yard: &Yard,
    prompt: &str,
    options: &TaskOptions,
    fleet: &Fleet,
    how: &RouteOptions,
    attempts: Option<u32>,
) -> Result<(Route, FleetEntry), Error> {
    let (kind, source, matched) = kind_of(prompt, how.kind);
    let (entry_name, entry) = fleet.entry(kind).ok_or_else(|| {
        Error::Unsupported(format!(
            "the fleet table has no [fleet.{kind}] and no [fleet.default]; add one, or name \
             a harness"
        ))
    })?;
    let history = stats(&yard.store().outcomes().outcomes(Some(kind))?);
    let available = |c: &FleetCandidate| match how.excluded.get(&c.harness) {
        Some(why) => Err(why.clone()),
        None => availability(yard, options, c, how.harnesses.as_deref()),
    };
    let route = plan(
        kind,
        source,
        matched,
        entry_name,
        entry,
        &history,
        &available,
        attempts.or(how.attempts).unwrap_or(entry.attempts).max(1),
        how.seed.unwrap_or_else(fresh_seed),
    )?;
    Ok((route, entry.clone()))
}

/// Start `prompt` on the candidates the router picks: one branch, or with
/// `fan` the entry's attempts, then fail each over while its harness fails.
pub(crate) fn run_routed(
    yard: &Yard,
    prompt: &str,
    options: &TaskOptions,
    fleet: &Fleet,
    how: &RouteOptions,
    fan: bool,
) -> Result<Routed, Error> {
    let (route, entry) = route(yard, prompt, options, fleet, how, (!fan).then_some(1))?;
    let options = &entry_options(options, &entry);
    let failover = how.failover.unwrap_or(entry.failover);
    let attempts = route.picks.len() as u32;
    let labels = labels(&route.picks);
    let budget = budget_for(&options.budget, &entry.budget);
    let specs = route
        .picks
        .iter()
        .zip(&labels)
        .enumerate()
        .map(|(n, (pick, label))| {
            // Only what the router found eligible: a candidate it excluded
            // (over budget, near its usage limit) stays out of the chain.
            let fallbacks: Vec<FleetCandidate> = entry
                .candidates
                .iter()
                .filter(|c| **c != pick.candidate)
                .filter(|c| !route.excluded.iter().any(|e| e.candidate == **c))
                .cloned()
                .collect();
            let decision = RouteDecision {
                kind: route.kind,
                kind_source: route.kind_source.clone(),
                entry: Some(route.entry.clone()),
                candidate: pick.candidate.clone(),
                attempt: n as u32 + 1,
                attempts,
                reason: pick.reason.clone(),
                failover,
                fallbacks,
                tried: Vec::new(),
                budget_usd: budget.max_usd,
                spent_usd: 0.0,
                from: None,
                environment: entry.environment.clone(),
                connectors: entry.connectors.clone(),
            };
            run::AttemptSpec {
                label: fan.then(|| label.clone()),
                harness: Some(pick.candidate.harness.clone()),
                command: pick.candidate.command.clone(),
                provision: provision_for(options.provision.as_ref(), &pick.candidate),
                budget: Some(budget.clone()),
                events: vec![Activity::Fleet(Box::new(FleetActivity::Routed(Box::new(
                    decision,
                ))))],
            }
        })
        .collect();
    let started = run::run_attempts(yard, prompt, options, specs)?;
    // A branch failed over to runs under the limits of the one it replaces,
    // with the cost limit shared along the chain (see `failover`).
    let chained = &TaskOptions {
        budget,
        ..options.clone()
    };
    let mut failovers = Vec::new();
    let chains: Vec<Result<Chain, Error>> = std::thread::scope(|scope| {
        let handles: Vec<_> = started
            .into_iter()
            .map(|branch| scope.spawn(move || chain(yard, branch, chained)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    });
    let mut branches = Vec::new();
    for result in chains {
        let (branch, moved) = result?;
        failovers.extend(moved);
        branches.push(branch);
    }
    let branches = crate::goal::pursue_all(yard, branches, options)?;
    Ok(Routed {
        route,
        branches,
        failovers,
    })
}

/// `options` with what the fleet entry adds: plan first when it says so,
/// and its goal judge for a goal that names none.
pub(crate) fn entry_options(options: &TaskOptions, entry: &FleetEntry) -> TaskOptions {
    let mut options = options.clone();
    options.plan |= entry.plan;
    if let (Some(goal), Some(judge)) = (options.goal.as_mut(), &entry.goal_judge) {
        if goal.judge.is_none() && goal.custom.is_none() {
            goal.judge = Some(judge.clone());
        }
    }
    options
}

/// `run`, recording `kind` for the outcome store: the harness is the
/// task's, not the router's.
pub(crate) fn run_with_kind(
    yard: &Yard,
    prompt: &str,
    options: &TaskOptions,
    kind: TaskKind,
) -> Result<Branch, Error> {
    let profile = harness::select(options.harness.as_deref())?;
    let mut candidate = FleetCandidate::new(options.harness.as_deref().unwrap_or(profile.harness));
    candidate.model = options.provision.as_ref().and_then(|p| p.model.clone());
    candidate.effort = options.provision.as_ref().and_then(|p| p.effort);
    let decision = RouteDecision {
        kind,
        kind_source: "flag".into(),
        entry: None,
        candidate,
        attempt: 1,
        attempts: 1,
        reason: "the harness was named".into(),
        failover: false,
        fallbacks: Vec::new(),
        tried: Vec::new(),
        budget_usd: options.budget.max_usd,
        spent_usd: 0.0,
        from: None,
        environment: None,
        connectors: Vec::new(),
    };
    let spec = run::AttemptSpec {
        label: None,
        harness: options.harness.clone(),
        command: None,
        provision: options.provision.clone(),
        budget: None,
        events: vec![Activity::Fleet(Box::new(FleetActivity::Routed(Box::new(
            decision,
        ))))],
    };
    Ok(run::run_attempts(yard, prompt, options, vec![spec])?.remove(0))
}

/// Branch name suffixes for a fan's picks: the harness, then `-2`, `-3`
/// for a harness picked again.
fn labels(picks: &[RoutePick]) -> Vec<String> {
    let mut seen: BTreeMap<&str, u32> = BTreeMap::new();
    picks
        .iter()
        .map(|p| {
            let n = seen.entry(p.candidate.harness.as_str()).or_insert(0);
            *n += 1;
            match *n {
                1 => p.candidate.harness.clone(),
                n => format!("{}-{n}", p.candidate.harness),
            }
        })
        .collect()
}

/// The branch a failover chain ended on, and each (failed, next) pair.
type Chain = (Branch, Vec<(String, String)>);

/// Fail `branch` over while its harness keeps failing.
fn chain(yard: &Yard, mut branch: Branch, options: &TaskOptions) -> Result<Chain, Error> {
    let mut moved = Vec::new();
    while let Some(next) = failover(yard, &branch.info().name, options)? {
        moved.push((branch.info().name.clone(), next.info().name.clone()));
        branch = next;
    }
    Ok((branch, moved))
}

/// Why a branch's status is its harness's fault rather than the task's:
/// the harness could not start, exited, never completed its handshake,
/// was rate-limited, could not authenticate, or was unavailable. `None`
/// for everything else, including a failed check, a limit, a cancel, a
/// refusal by the model, provisioning and workspace setup.
pub fn harness_fault(status: &BranchStatus) -> Option<String> {
    let BranchStatus::Failed { reason } = status else {
        return None;
    };
    let text = reason.to_lowercase();
    // The task's configuration, the engine or the repository, never the
    // harness: checked first, since their messages may quote a harness's.
    const NOT_THE_HARNESS: &[&str] = &[
        "could not provision",
        "could not offer delegation",
        "the driver refused",
        "workspace setup",
        "setup failed",
        "setup could not",
        "the model refused",
        "lease to another",
        "denied:",
        "cancelled by",
        "its turn did not start",
        "snapshot failed",
        "could not create the worktree",
        "could not create the branch",
    ];
    if NOT_THE_HARNESS.iter().any(|p| text.contains(p)) {
        return None;
    }
    const RULES: &[(&[&str], &str)] = &[
        (
            &[
                "rate limit",
                "rate_limit",
                "ratelimit",
                "too many requests",
                "429",
                "overloaded",
                "quota",
            ],
            "rate-limited",
        ),
        (
            &[
                "unauthorized",
                "unauthenticated",
                "authentication",
                "not logged in",
                "please log in",
                "login required",
                "invalid api key",
                "invalid_api_key",
                "401",
                "403",
            ],
            "could not authenticate",
        ),
        (&["could not start"], "could not start"),
        (&["unavailable", "not found on path"], "unavailable"),
        (
            &[
                "harness exited",
                "exited",
                "outcome is unknown",
                "closed its output",
                "broken pipe",
                "connection reset",
            ],
            "exited",
        ),
        (
            &["open failed", "handshake", "timed out", "initialize"],
            "did not complete its handshake",
        ),
    ];
    RULES
        .iter()
        .find(|(needles, _)| needles.iter().any(|n| text.contains(n)))
        .map(|(_, why)| format!("the harness {why}: {reason}"))
}

/// When `name` ended its turn because its harness failed and its routing
/// allows it, start the task again on the next candidate: a new branch
/// from its candidate (or its base, when it has none), with a handoff
/// brief, recording why on both. The new branch runs under `options`'s
/// limits with the cost limit cut to what the chain has left: a routed run
/// passes the effective budget its first attempt ran under, `by send` its
/// own. `None` when there is nothing to do: not routed with failover, a
/// task failure, no candidate left, or no budget left.
pub(crate) fn failover(
    yard: &Yard,
    name: &str,
    options: &TaskOptions,
) -> Result<Option<Branch>, Error> {
    let store = yard.store();
    let record = store.read(name)?;
    let Some(fault) = harness_fault(&record.info.status) else {
        return Ok(None);
    };
    let events = crate::record::read(&store, name)?;
    let Some(decision) = recorded_route(&events).filter(|d| d.failover) else {
        return Ok(None);
    };
    let mut tried = decision.tried.clone();
    tried.push(decision.candidate.clone());
    let spent = decision.spent_usd + record.info.cost_usd.unwrap_or(0.0);
    let warn = |text: String| -> Result<(), Error> {
        crate::record::Recorder::open(&store, name, None)?.record(Activity::Warning(text))
    };
    if let Some(max) = decision.budget_usd {
        if spent >= max {
            warn(format!(
                "not failing over: the chain spent ${spent:.2} of its ${max:.2} budget"
            ))?;
            return Ok(None);
        }
    }
    let remaining: Vec<&FleetCandidate> = decision
        .fallbacks
        .iter()
        .filter(|c| !tried.contains(c))
        .collect();
    let mut skipped = Vec::new();
    let Some(next) = remaining
        .iter()
        .find(|c| match availability(yard, options, c, None) {
            Ok(()) => true,
            Err(why) => {
                skipped.push(format!("{}: {why}", c.label()));
                false
            }
        })
    else {
        warn(format!(
            "not failing over: no candidate left{}",
            match skipped.is_empty() {
                true => String::new(),
                false => format!(" ({})", skipped.join("; ")),
            }
        ))?;
        return Ok(None);
    };
    let next = (*next).clone();
    crate::record::Recorder::open(&store, name, None)?.record(Activity::Fleet(Box::new(
        FleetActivity::FailedOver {
            reason: fault.clone(),
            next: next.clone(),
        },
    )))?;
    let fallbacks = decision
        .fallbacks
        .iter()
        .filter(|c| **c != next && !tried.contains(c))
        .cloned()
        .collect();
    let routed = RouteDecision {
        candidate: next.clone(),
        reason: format!("failed over from {name}: {fault}"),
        fallbacks,
        tried,
        spent_usd: spent,
        from: Some(name.to_owned()),
        ..decision.clone()
    };
    let mut provision = record.provision.clone().unwrap_or_default();
    provision.model = next.model.clone();
    provision.effort = next.effort;
    if let Some(task) = &options.provision {
        if next.model.is_none() {
            provision.model = task.model.clone();
        }
        if next.effort.is_none() {
            provision.effort = task.effort;
        }
    }
    let budget = Budget {
        max_usd: decision.budget_usd.map(|max| (max - spent).max(0.0)),
        ..options.budget.clone()
    };
    let next_options = TaskOptions {
        harness: Some(next.harness.clone()),
        command: next.command.clone().or(options.command.clone()),
        provision: Some(provision),
        budget,
        name: None,
        ..options.clone()
    };
    let branch = run::reincarnate_with(
        yard,
        name,
        &next_options,
        run::Reincarnation {
            from_base: true,
            stem: Some(format!("{name} {}", next.harness)),
            events: vec![Activity::Fleet(Box::new(FleetActivity::Routed(Box::new(
                routed,
            ))))],
            why: Some(fault),
        },
    )?;
    Ok(Some(branch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_are_classified_by_keywords() {
        let kind = |p: &str| classify(p).kind;
        assert_eq!(
            kind("Fix the crash when parsing empty input"),
            TaskKind::Bugfix
        );
        assert_eq!(kind("Add a --json flag to by ls"), TaskKind::Feature);
        assert_eq!(
            kind("Refactor engine.rs into smaller modules"),
            TaskKind::Refactor
        );
        assert_eq!(kind("Review by/feature-implementer"), TaskKind::Review);
        assert_eq!(
            kind("Investigate why the build is slow"),
            TaskKind::Research
        );
        assert_eq!(kind("Update the README with install steps"), TaskKind::Docs);
        assert_eq!(
            kind("Migrate from rusqlite 0.30 to 0.39"),
            TaskKind::Migration
        );
        assert_eq!(kind("Write tests for the parser"), TaskKind::Tests);
        assert_eq!(kind("Make it so"), TaskKind::Other);
        // The first word counts three: "fix" beats "test".
        assert_eq!(kind("Fix the flaky parser test"), TaskKind::Bugfix);
        assert_eq!(
            classify("Fix the crash, then fix the error").matched,
            ["fix", "crash", "error"]
        );
        for kind in TaskKind::ALL {
            assert_eq!(kind.as_str().parse::<TaskKind>().unwrap(), kind);
        }
        assert!("nope".parse::<TaskKind>().is_err());
    }

    fn entry(names: &[&str]) -> FleetEntry {
        FleetEntry {
            candidates: names.iter().map(|n| FleetCandidate::new(*n)).collect(),
            attempts: 1,
            exploration: 0.0,
            ..FleetEntry::default()
        }
    }

    fn row(harness: &str, outcome: BranchOutcome, cost: Option<f64>) -> OutcomeRecord {
        OutcomeRecord {
            id: format!("{harness}-{}", now_ms()),
            repo: "/r".into(),
            branch: "b".into(),
            kind: TaskKind::Bugfix,
            harness: harness.into(),
            model: None,
            effort: None,
            outcome,
            score: None,
            cost_usd: cost,
            duration_ms: Some(10),
            turns: 1,
            routed: true,
            recorded_ms: 0,
        }
    }

    fn all_ok(_: &FleetCandidate) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn the_router_is_deterministic_under_a_seed_and_learns_from_outcomes() {
        let e = entry(&["a", "b", "c"]);
        let pick = |history: &[CandidateStats], seed| {
            plan(
                TaskKind::Bugfix,
                "flag",
                vec![],
                "bugfix",
                &e,
                history,
                &all_ok,
                1,
                seed,
            )
            .unwrap()
            .picks[0]
                .candidate
                .harness
                .clone()
        };
        for seed in 0..20 {
            assert_eq!(pick(&[], seed), pick(&[], seed), "seed {seed}");
        }
        // With b merging every time and the others failing, b wins nearly
        // always.
        let mut rows = Vec::new();
        for _ in 0..30 {
            rows.push(row("a", BranchOutcome::Failed, None));
            rows.push(row("b", BranchOutcome::Merged, None));
            rows.push(row("c", BranchOutcome::Failed, None));
        }
        let history = stats(&rows);
        let wins = (0..200).filter(|seed| pick(&history, *seed) == "b").count();
        assert!(wins >= 195, "{wins}");
        // Without history, every candidate gets picked sometimes.
        let picked: BTreeSet<String> = (0..200).map(|seed| pick(&[], seed)).collect();
        assert_eq!(picked.len(), 3);
    }

    #[test]
    fn the_exploration_floor_picks_at_random() {
        let mut e = entry(&["a", "b"]);
        e.exploration = 1.0;
        let mut rows = Vec::new();
        for _ in 0..50 {
            rows.push(row("a", BranchOutcome::Merged, None));
            rows.push(row("b", BranchOutcome::Failed, None));
        }
        let history = stats(&rows);
        let bs = (0..200)
            .filter(|seed| {
                let route = plan(
                    TaskKind::Bugfix,
                    "flag",
                    vec![],
                    "bugfix",
                    &e,
                    &history,
                    &all_ok,
                    1,
                    *seed,
                )
                .unwrap();
                assert!(route.picks[0].explored);
                route.picks[0].candidate.harness == "b"
            })
            .count();
        assert!((60..140).contains(&bs), "{bs}");
    }

    #[test]
    fn unavailable_and_over_budget_candidates_are_never_picked() {
        let mut e = entry(&["a", "b", "c"]);
        e.budget.max_usd = Some(1.0);
        e.exploration = 0.5;
        let rows = vec![row("c", BranchOutcome::Merged, Some(3.0))];
        let history = stats(&rows);
        let no_a = |c: &FleetCandidate| match c.harness.as_str() {
            "a" => Err("a was not found on PATH".to_owned()),
            _ => Ok(()),
        };
        for seed in 0..100 {
            let route = plan(
                TaskKind::Bugfix,
                "flag",
                vec![],
                "bugfix",
                &e,
                &history,
                &no_a,
                3,
                seed,
            )
            .unwrap();
            assert!(route.picks.iter().all(|p| p.candidate.harness == "b"));
            assert_eq!(route.picks.len(), 3, "rounds repeat the only candidate");
            assert_eq!(route.excluded.len(), 2);
        }
        let none = |_: &FleetCandidate| Err("gone".to_owned());
        let refused = plan(
            TaskKind::Bugfix,
            "flag",
            vec![],
            "bugfix",
            &e,
            &history,
            &none,
            1,
            0,
        );
        assert!(
            matches!(&refused, Err(Error::HarnessUnavailable { reason, .. }) if reason.contains("a: gone")),
            "{refused:?}"
        );
    }

    #[test]
    fn a_fan_picks_each_candidate_once_per_round() {
        let e = entry(&["a", "b", "c"]);
        let route = plan(
            TaskKind::Bugfix,
            "flag",
            vec![],
            "bugfix",
            &e,
            &[],
            &all_ok,
            4,
            7,
        )
        .unwrap();
        let first: BTreeSet<&str> = route.picks[..3]
            .iter()
            .map(|p| p.candidate.harness.as_str())
            .collect();
        assert_eq!(first.len(), 3);
        let names = labels(&route.picks);
        assert_eq!(names.iter().filter(|n| n.ends_with("-2")).count(), 1);
    }

    #[test]
    fn beta_samples_have_the_right_mean() {
        let mut rng = Rng::new(42);
        let n = 20_000;
        let mean: f64 = (0..n).map(|_| rng.beta(3.0, 7.0)).sum::<f64>() / n as f64;
        assert!((mean - 0.3).abs() < 0.01, "{mean}");
        let mean: f64 = (0..n).map(|_| rng.beta(0.5, 0.5)).sum::<f64>() / n as f64;
        assert!((mean - 0.5).abs() < 0.02, "{mean}");
    }

    #[test]
    fn only_harness_failures_fail_over() {
        let failed = |reason: &str| BranchStatus::Failed {
            reason: reason.into(),
        };
        for reason in [
            "could not start /x: No such file or directory",
            "the turn's outcome is unknown: harness exited: boom",
            "the harness exited",
            "write: Broken pipe (os error 32)",
            "the turn's outcome is unknown: the harness connection closed before the turn ended",
            "open failed: timed out after 30s",
            "API error 429: rate limit exceeded",
            "Invalid API key · Please run /login",
            "gemini is unavailable: gemini was not found on PATH",
        ] {
            assert!(harness_fault(&failed(reason)).is_some(), "{reason}");
        }
        for reason in [
            "could not provision gemini-cli: a file in HOME",
            "workspace setup failed: exit status 1",
            "workspace setup `npm ci` exited with status 1",
            "the driver refused: unsupported: model selection over ACP",
            "could not create the worktree: 403 in a path",
            "the model refused to continue",
            "this engine lost x's lease to another; it stopped the turn",
        ] {
            assert_eq!(harness_fault(&failed(reason)), None, "{reason}");
        }
        assert_eq!(harness_fault(&BranchStatus::Interrupted), None);
        assert_eq!(
            harness_fault(&BranchStatus::BudgetExceeded {
                limit: "max_usd".into()
            }),
            None
        );
    }

    #[test]
    fn outcomes_credit_and_group() {
        let mut ready = row("a", BranchOutcome::Ready, Some(1.0));
        assert_eq!(credit(&ready), Some(0.5));
        ready.score = Some(80.0);
        assert_eq!(credit(&ready), Some(0.8));
        let rows = vec![
            ready,
            row("a", BranchOutcome::Merged, Some(3.0)),
            row("a", BranchOutcome::Interrupted, None),
        ];
        let s = &stats(&rows)[0];
        assert_eq!((s.runs, s.merged, s.ready, s.interrupted), (3, 1, 1, 1));
        assert!((s.successes - 1.8).abs() < 1e-9);
        assert!((s.failures - 0.2).abs() < 1e-9);
        assert_eq!(s.mean_cost_usd, Some(2.0));
        assert_eq!(
            BranchOutcome::of(&BranchStatus::NoChanges),
            Some(BranchOutcome::Failed)
        );
        assert_eq!(BranchOutcome::of(&BranchStatus::Running), None);
    }
}
