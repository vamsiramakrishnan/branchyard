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
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{
    harness, placement, run, Activity, Branch, BranchStatus, Budget, Effort, Error, Provisioning,
    RecordedEvent, StallAction, TaskOptions, Yard,
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
/// `KEYWORDS` scores a point for that kind, and the prompt's first word,
/// usually the imperative verb, scores three. The highest score wins, ties
/// going to the kind listed first in `KEYWORDS`; no match is
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
/// the candidates' diffs, answering a JSON verdict. See `crate::judge`.
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

/// The stall window of a routed branch whose command line and entry set
/// none: no harness activity for this long interrupts its turn
/// ([`crate::StallAction::Interrupt`]), and with failover the task moves to
/// the next candidate. A harness that hangs on a search, a prompt nobody
/// answers or a dead connection otherwise holds the attempt until
/// `max_minutes`, or for good. `stall_minutes = 0` on the entry turns it
/// off; `--stall-after` on the command line replaces it.
pub const DEFAULT_STALL: Duration = Duration::from_secs(15 * 60);

/// The fleet table: entries by task kind, and `default`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fleet {
    pub entries: BTreeMap<String, FleetEntry>,
}

impl Fleet {
    /// The entry for `kind`, else `default`, else the [inventory
    /// default](inventory_default)'s, with the key it was found at.
    pub fn entry(&self, kind: TaskKind) -> Option<(&str, &FleetEntry)> {
        self.entries
            .get_key_value(kind.as_str())
            .or_else(|| self.entries.get_key_value("default"))
            .or_else(|| self.entries.get_key_value(INVENTORY_ENTRY))
            .map(|(k, v)| (k.as_str(), v))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// The inventory default

/// The entry a route from the [inventory default](inventory_default)
/// records. No `[fleet.<kind>]` can have this name, so a route, `by log`
/// and `by fleet route` can say where it came from.
pub const INVENTORY_ENTRY: &str = "inventory";

/// How people read a route's entry: `[fleet.bugfix]`, or `the inventory
/// default`.
pub fn entry_label(entry: &str) -> String {
    match entry {
        INVENTORY_ENTRY => "the inventory default".into(),
        entry => format!("[fleet.{entry}]"),
    }
}

/// The fleet `--auto` routes among when the configuration has no `[fleet]`,
/// built from what this machine has at that moment; see
/// [`inventory_default`].
#[derive(Clone, Debug, PartialEq)]
pub struct InventoryDefault {
    /// Its one entry, under [`INVENTORY_ENTRY`]: the candidates and a
    /// judge, with the documented defaults for the rest (one attempt, no
    /// failover of its own, exploration 0.1, no limits).
    pub entry: FleetEntry,
    /// Installed harnesses left out, and why: logged out, a login
    /// Branchyard cannot tell, not on `PATH`, at a usage limit, or no
    /// profile to drive it with.
    pub excluded: Vec<Excluded>,
}

impl InventoryDefault {
    /// As a fleet table: its entry for every kind.
    pub fn fleet(&self) -> Fleet {
        Fleet {
            entries: [(INVENTORY_ENTRY.to_owned(), self.entry.clone())].into(),
        }
    }

    /// The candidates' labels, in order: `codex, gemini-cli`.
    pub fn names(&self) -> Vec<String> {
        self.entry
            .candidates
            .iter()
            .map(FleetCandidate::label)
            .collect()
    }
}

/// The inventory default: every installed harness Branchyard has a profile
/// for whose login is likely or verified and that can run here (on `PATH`,
/// not at a usage limit), in catalog order, with `preferred` (`[defaults]
/// harness`, a harness or profile ID) first when it is among them.
///
/// Each candidate runs with no model of its own, and with effort `medium`
/// where its provisioner takes one for a branch whose home is
/// (`private_home`) or is not its own: Codex, isolated. Elsewhere an effort
/// would be refused, so the harness keeps its own. The judge is the first
/// candidate that routes tool permission requests to Branchyard, since a
/// judge runs with every tool denied; with none, judging is deterministic.
pub fn inventory_default(
    inventory: &crate::inventory::Inventory,
    preferred: Option<&str>,
    private_home: bool,
) -> InventoryDefault {
    use crate::inventory::{harness_of, LoginState};
    let preferred_harness = preferred.and_then(harness_of);
    let mut candidates: Vec<FleetCandidate> = Vec::new();
    let mut excluded = Vec::new();
    for state in &inventory.harnesses {
        let id = state.id.as_str();
        // `[defaults] harness` may name one of this harness's profiles.
        let named = match (preferred_harness == Some(id), preferred) {
            (true, Some(preferred)) => preferred,
            _ => id,
        };
        let why = match harness::select(Some(named)) {
            Err(_) => Err(format!(
                "Branchyard has no profile that drives {id} (by harnesses --profiles)"
            )),
            Ok(profile) => match (state.ready(), state.login.state) {
                (Err(why), _) => Err(why),
                (Ok(()), LoginState::LoggedIn) => Ok(profile),
                (Ok(()), _) => Err(format!(
                    "{id}'s login cannot be told ({}); log in and check with `by harnesses`, \
                     or name it in a [fleet]",
                    state.login.detail
                )),
            },
        };
        match why {
            Ok(profile) => candidates.push(FleetCandidate {
                harness: named.to_owned(),
                model: None,
                effort: branchyard_provision::takes_effort(
                    profile.harness,
                    profile.protocol,
                    private_home,
                )
                .then_some(Effort::Medium),
                command: None,
            }),
            Err(reason) => excluded.push(Excluded {
                candidate: FleetCandidate::new(id),
                reason,
            }),
        }
    }
    if let Some(at) = candidates
        .iter()
        .position(|c| preferred_harness.is_some() && harness_of(&c.harness) == preferred_harness)
    {
        let first = candidates.remove(at);
        candidates.insert(0, first);
    }
    let judge = candidates
        .iter()
        .find(|c| {
            harness::select(Some(&c.harness))
                .is_ok_and(|profile| harness::check_approvals(profile, false).is_ok())
        })
        .map(|c| JudgeSpec {
            harness: c.harness.clone(),
            model: None,
            effort: c.effort,
            command: None,
            rubric: None,
        });
    InventoryDefault {
        entry: FleetEntry {
            candidates,
            judge,
            ..FleetEntry::default()
        },
        excluded,
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
    /// A delegating parent chose this child's harness and model, with the
    /// reason it gave ([`ParentChoice`]).
    Chosen(Box<ParentChoice>),
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
                    "routed {} ({}) by {} to {}{}: {}",
                    d.kind,
                    d.kind_source,
                    entry_label(entry),
                    d.candidate.label(),
                    match d.attempts {
                        1 => String::new(),
                        n => format!(", attempt {} of {n}", d.attempt),
                    },
                    d.reason
                ),
                None => format!("kind {} ({})", d.kind, d.kind_source),
            },
            FleetActivity::Chosen(c) => format!(
                "chosen by {} for {} ({}): {}{}",
                c.by,
                c.kind,
                c.kind_source,
                c.candidate.label(),
                c.why
                    .as_deref()
                    .map(|w| format!(": {w}"))
                    .unwrap_or_default()
            ),
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

/// A delegating parent's choice of a child's harness and model, recorded
/// on the child as [`FleetActivity::Chosen`] when it is spawned: the kind
/// the classifier gave its prompt, the candidate, the parent, and the
/// reason the parent gave (`by spawn --why`), so the outcome store can
/// learn from the choice as it learns from the router's.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParentChoice {
    /// The kind the classifier gave the child's prompt.
    pub kind: TaskKind,
    /// `classifier`, always: a spawn names no kind.
    pub kind_source: String,
    /// The child's harness, model and effort as the parent chose them.
    pub candidate: FleetCandidate,
    /// The parent branch.
    pub by: String,
    /// The parent's reason, from `by spawn --why` or its equivalents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// The parent's choice recorded on a branch, if it was spawned by one.
pub fn recorded_choice(events: &[RecordedEvent]) -> Option<ParentChoice> {
    events.iter().rev().find_map(|e| match &e.activity {
        Activity::Fleet(activity) => match activity.as_ref() {
            FleetActivity::Chosen(choice) => Some(choice.as_ref().clone()),
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
            | BranchStatus::AwaitingPlanApproval
            | BranchStatus::WaitingOnChildren => None,
            BranchStatus::Ready => Some(BranchOutcome::Ready),
            BranchStatus::Merged { .. } => Some(BranchOutcome::Merged),
            BranchStatus::Interrupted => Some(BranchOutcome::Interrupted),
            // A discarded branch was set aside: its work was not used.
            BranchStatus::NoChanges
            | BranchStatus::BudgetExceeded { .. }
            | BranchStatus::Failed { .. }
            | BranchStatus::Discarded { .. } => Some(BranchOutcome::Failed),
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
    /// Who chose its harness: [`DECIDED_BY_ROUTER`] (a fleet entry),
    /// [`DECIDED_BY_PARENT`] (a delegating parent's spawn), or `None` for
    /// one a person named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    pub recorded_ms: u64,
}

/// [`OutcomeRecord::decided_by`] of a branch the router routed.
pub const DECIDED_BY_ROUTER: &str = "router";
/// [`OutcomeRecord::decided_by`] of a delegated child whose parent chose
/// its harness.
pub const DECIDED_BY_PARENT: &str = "parent";

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
/// branches and delegated children alike (a child's row says its parent
/// decided its harness), not a judge's scratch branch. `judged` sets the
/// score and, when picked, `judged_best`. Best-effort for callers: it
/// never changes what happened to the branch.
pub(crate) fn observe(
    yard: &Yard,
    name: &str,
    judged: Option<(f64, bool)>,
) -> Result<Option<OutcomeRecord>, Error> {
    let store = yard.store();
    let record = store.read(name)?;
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
    let choice = recorded_choice(&events);
    let kind = route
        .as_ref()
        .map(|d| d.kind)
        .or_else(|| choice.as_ref().map(|c| c.kind))
        .unwrap_or_else(|| classify(&record.info.prompt).kind);
    let decided_by = match (&route, &choice) {
        (Some(d), _) if d.entry.is_some() => Some(DECIDED_BY_ROUTER.to_owned()),
        (_, Some(_)) => Some(DECIDED_BY_PARENT.to_owned()),
        _ => None,
    };
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
        decided_by,
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
    /// Runs whose harness a delegating parent chose ([`DECIDED_BY_PARENT`]).
    #[serde(default)]
    pub delegated: u32,
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
                delegated: rows
                    .iter()
                    .filter(|r| r.decided_by.as_deref() == Some(DECIDED_BY_PARENT))
                    .count() as u32,
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

/// Why a candidate's recorded cost keeps it out of an entry with a cost
/// limit: its mean recorded cost for the kind is over `max_usd`. `None`
/// without a limit, without cost reports, or within it.
fn over_budget(max_usd: Option<f64>, stats: Option<&CandidateStats>) -> Option<String> {
    match (max_usd, stats.and_then(|s| s.mean_cost_usd)) {
        (Some(max), Some(mean)) if mean > max => Some(format!(
            "its mean recorded cost ${mean:.2} is over the ${max:.2} budget"
        )),
        _ => None,
    }
}

/// Choose `attempts` candidates from `entry`: each available and within
/// budget; with probability `entry.exploration` one at random, otherwise
/// the highest draw from Beta(1 + successes, 1 + failures) of its recorded
/// outcomes for this kind, ties going to the earlier candidate. Each round
/// picks without replacement; more attempts than candidates start another
/// round.
#[allow(clippy::too_many_arguments)] // ratchet: branchyard
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
        let over = over_budget(entry.budget.max_usd, find(candidate));
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
            harness: entry_label(entry_name),
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
            let Some((at, draw)) = best else {
                return Err(Error::State("the candidate pool is empty".into()));
            };
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

/// The task's limits, with the entry's where the task sets none, and a
/// stall window where neither does ([`DEFAULT_STALL`]); an entry's zero
/// window turns stall detection off.
fn budget_for(task: &Budget, entry: &Budget) -> Budget {
    let (stall_after, stall_action) = match (task.stall_after, entry.stall_after) {
        (Some(window), _) => (Some(window), task.stall_action),
        (None, Some(window)) if window.is_zero() => (None, entry.stall_action),
        (None, Some(window)) => (Some(window), entry.stall_action),
        (None, None) => (Some(DEFAULT_STALL), StallAction::Interrupt),
    };
    Budget {
        max_usd: task.max_usd.or(entry.max_usd),
        max_turns: task.max_turns.or(entry.max_turns),
        max_duration: task.max_duration.or(entry.max_duration),
        stall_after,
        stall_action,
        hold_cap: task.hold_cap.or(entry.hold_cap),
        follow_up_grace: None,
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

// ---------------------------------------------------------------------------
// Advice

/// The entry an advice names when it has no fleet table and lists the
/// harnesses the caller's envelope allows instead.
pub const ENVELOPE_ENTRY: &str = "envelope";

/// Stems of words that mark a task as one not to hand to the cheapest
/// tier: production, destructive and compliance-sensitive work. A prompt's
/// lowercase words are matched whole, or by prefix for a stem of six
/// letters or more; [`risk_words`] lists the ones found.
pub const RISK_WORDS: &[&str] = &[
    "production",
    "deploy",
    "release",
    "delete",
    "drop",
    "wipe",
    "purge",
    "truncate",
    "destructive",
    "irreversible",
    "migrat",
    "security",
    "secret",
    "credential",
    "payment",
    "billing",
    "legal",
    "compliance",
    "privacy",
    "encrypt",
];

/// The words of `prompt` that match [`RISK_WORDS`], in order, each once.
pub fn risk_words(prompt: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for word in prompt
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
    {
        let risky = RISK_WORDS
            .iter()
            .any(|stem| word == *stem || (stem.len() >= 6 && word.starts_with(stem)));
        if risky && !found.contains(&word) {
            found.push(word);
        }
    }
    found
}

/// `candidate`'s recorded `kind` outcomes as one arm: its own arm when it
/// names a model and effort, else every arm of its harness pooled, which
/// is what an envelope's bare harness ID has to go on.
pub fn pooled(
    rows: &[OutcomeRecord],
    kind: TaskKind,
    candidate: &FleetCandidate,
) -> Option<CandidateStats> {
    let effort = candidate.effort.map(effort_text);
    let normalized: Vec<OutcomeRecord> = rows
        .iter()
        .filter(|r| r.kind == kind && r.harness == candidate.harness)
        .filter(|r| {
            candidate
                .model
                .as_ref()
                .is_none_or(|m| r.model.as_deref() == Some(m))
        })
        .filter(|r| {
            effort
                .as_ref()
                .is_none_or(|e| r.effort.as_deref() == Some(e))
        })
        .map(|r| OutcomeRecord {
            model: candidate.model.clone(),
            effort: effort.clone(),
            ..r.clone()
        })
        .collect();
    stats(&normalized).into_iter().next()
}

/// One candidate an advice describes.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Advised {
    /// The harness, model and effort, as the fleet entry or the envelope
    /// lists them.
    pub candidate: FleetCandidate,
    /// Whether it can run here now; `reason` says why not.
    pub available: bool,
    /// Why it cannot run: outside the envelope, not installed, logged out,
    /// or over the entry's budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Its recorded outcomes for this kind ([`pooled`]), when it has any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<CandidateStats>,
    /// The posterior mean success rate the router samples around: 0.5 with
    /// no outcomes.
    pub expected: f64,
}

/// What a caller should know before choosing a harness for a task: the
/// candidates with their availability and recorded outcomes, the router's
/// own pick among them, the budget left, and notes. See
/// `docs/fleet.md#advice`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Advice {
    /// The task's kind, from `kind_source`.
    pub kind: TaskKind,
    /// `flag` or `classifier`.
    pub kind_source: String,
    /// The words that decided a classified kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched: Vec<String>,
    /// Where the candidates came from: a fleet entry (`bugfix`, `default`,
    /// [`INVENTORY_ENTRY`]), or [`ENVELOPE_ENTRY`] for the harnesses the
    /// caller may run when no fleet table is known.
    pub entry: String,
    /// Every candidate, in the entry's (or the envelope's) order.
    pub candidates: Vec<Advised>,
    /// The router's pick among the available candidates: the highest
    /// sampled success rate under `seed`, with no exploration; its `index`
    /// counts the available candidates only. `None` when none can run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick: Option<RoutePick>,
    /// The seed the pick was sampled under; `--seed` reproduces it.
    pub seed: u64,
    /// The caller's own harness and model, inside a delegating turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<FleetCandidate>,
    /// What the caller may still spend on children, and what the cost
    /// limits above it leave its tree; inside a delegating turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_usd: Option<f64>,
    /// What the cost limits above the caller leave its tree, when one
    /// bounds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_remaining_usd: Option<f64>,
    /// [`risk_words`] of the task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk_words: Vec<String>,
    /// Deterministic hints: no outcomes yet, risk words, nothing can run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Where an advice's candidates come from, and what the caller has.
#[derive(Clone, Default)]
pub struct AdviceScope<'a> {
    /// The fleet table, when one is known: its entry for the kind gives the
    /// candidates. Without one, `allowed` does.
    pub fleet: Option<&'a Fleet>,
    /// Harness or profile IDs the caller may run (its envelope); a fleet
    /// candidate outside it is listed as unavailable.
    pub allowed: Option<Vec<String>>,
    /// The caller's own harness and model.
    pub own: Option<FleetCandidate>,
    /// What the caller may still give children ([`Advice::remaining_usd`]).
    pub remaining_usd: Option<f64>,
    /// What the cost limits above the caller leave its tree
    /// ([`Advice::tree_remaining_usd`]).
    pub tree_remaining_usd: Option<f64>,
    /// Seed the pick for a reproducible advice; a fresh one otherwise.
    pub seed: Option<u64>,
    /// What this machine has of each harness, as [`RouteOptions::harnesses`].
    pub harnesses: Option<&'a dyn crate::inventory::HarnessGate>,
}

/// The advice for `prompt`: see [`Advice`].
pub(crate) fn advise(
    yard: &Yard,
    options: &TaskOptions,
    prompt: &str,
    kind: Option<TaskKind>,
    scope: AdviceScope<'_>,
) -> Result<Advice, Error> {
    let (kind, source, matched) = kind_of(prompt, kind);
    // The entry's candidates and its cost limit, which keeps a candidate
    // whose recorded cost is over it out, as the router's plan does.
    let (entry, max_usd, mut candidates): (String, Option<f64>, Vec<FleetCandidate>) =
        match scope.fleet.and_then(|f| f.entry(kind)) {
            Some((name, found)) => (
                name.to_owned(),
                found.budget.max_usd,
                found.candidates.clone(),
            ),
            None => (
                ENVELOPE_ENTRY.to_owned(),
                None,
                scope
                    .allowed
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(FleetCandidate::new)
                    .collect(),
            ),
        };
    // The caller's own harness, listed by name alone, carries its model, and
    // its command (a child on it inherits that; `None` for a real harness).
    if let Some(own) = &scope.own {
        // An envelope lists profile IDs, the caller's own by its profile:
        // the same profile is its own harness, however it is named.
        let own_profile = harness::select(Some(&own.harness)).ok().map(|p| p.id);
        for candidate in &mut candidates {
            let profile = harness::select(Some(&candidate.harness)).ok().map(|p| p.id);
            if candidate.harness != own.harness && (profile.is_none() || profile != own_profile) {
                continue;
            }
            if candidate.model.is_none() && candidate.effort.is_none() {
                candidate.model = own.model.clone();
            }
            if candidate.command.is_none() {
                candidate.command = own.command.clone();
            }
        }
    }
    let rows = yard.store().outcomes().outcomes(Some(kind))?;
    let history: Vec<CandidateStats> = candidates
        .iter()
        .filter_map(|c| pooled(&rows, kind, c))
        .collect();
    let allowed = |c: &FleetCandidate| -> Result<(), String> {
        let Some(list) = &scope.allowed else {
            return Ok(());
        };
        let profile = harness::select(Some(&c.harness)).map_err(|e| e.to_string())?;
        match list.iter().any(|h| h == profile.id || h == profile.harness) {
            true => Ok(()),
            false => Err(format!(
                "not in this branch's envelope ({})",
                list.join(", ")
            )),
        }
    };
    let mut advised = Vec::new();
    let mut eligible = Vec::new();
    for candidate in &candidates {
        let stats = history
            .iter()
            .find(|s| candidate.same_arm(&s.harness, s.model.as_deref(), s.effort.as_deref()))
            .cloned();
        let reason = allowed(candidate)
            .and_then(|()| availability(yard, options, candidate, scope.harnesses))
            .and_then(|()| over_budget(max_usd, stats.as_ref()).map_or(Ok(()), Err))
            .err();
        if reason.is_none() {
            eligible.push(candidate.clone());
        }
        advised.push(Advised {
            candidate: candidate.clone(),
            available: reason.is_none(),
            reason,
            expected: stats.as_ref().map_or(0.5, CandidateStats::expected),
            stats,
        });
    }
    let seed = scope.seed.unwrap_or_else(fresh_seed);
    let pick = match eligible.is_empty() {
        true => None,
        false => {
            let among = FleetEntry {
                candidates: eligible,
                exploration: 0.0,
                budget: Budget {
                    max_usd,
                    ..Budget::default()
                },
                ..FleetEntry::default()
            };
            plan(
                kind,
                source,
                matched.clone(),
                &entry,
                &among,
                &history,
                &|_| Ok(()),
                1,
                seed,
            )?
            .picks
            .into_iter()
            .next()
        }
    };
    let risk = risk_words(prompt);
    let mut notes = Vec::new();
    if history.is_empty() {
        notes.push(format!(
            "no recorded {kind} outcomes in this repository: every P(OK) is the 0.50 prior, so \
             prefer the cheapest candidate that can do the work, and say why"
        ));
    }
    if !risk.is_empty() {
        notes.push(format!(
            "the task mentions {}: do not give it to the cheapest tier; prefer a candidate with \
             recorded successes",
            risk.join(", ")
        ));
    }
    if pick.is_none() {
        notes.push("no candidate can run here now".into());
    }
    Ok(Advice {
        kind,
        kind_source: source.to_owned(),
        matched,
        entry,
        candidates: advised,
        pick,
        seed,
        own: scope.own,
        remaining_usd: scope.remaining_usd,
        tree_remaining_usd: scope.tree_remaining_usd,
        risk_words: risk,
        notes,
    })
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

/// A stall the engine interrupted is the harness's fault too: it stopped
/// answering, which is what the stall window is for. Told from a cancel,
/// which also ends `interrupted`, by the warning the engine records
/// (`engine::STALL_WARNING`) in the latest turn: a stall an earlier turn
/// was interrupted for says nothing about why this one ended. `None` for
/// every other status.
pub fn stall_fault(status: &BranchStatus, events: &[RecordedEvent]) -> Option<String> {
    let stalled = matches!(status, BranchStatus::Interrupted)
        && events
            .iter()
            .rev()
            .take_while(|e| !matches!(e.activity, Activity::Prompt(_)))
            .any(|e| {
                matches!(&e.activity, Activity::Warning(text) if text == crate::engine::STALL_WARNING)
            });
    stalled.then(|| "stalled: no harness activity for its stall window".to_owned())
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
    let events = crate::record::read(&store, name)?;
    let Some(fault) =
        harness_fault(&record.info.status).or_else(|| stall_fault(&record.info.status, &events))
    else {
        return Ok(None);
    };
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
            decided_by: Some(DECIDED_BY_ROUTER.to_owned()),
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

    /// A stall the engine interrupted fails over; a cancel, which also
    /// ends interrupted, does not.
    #[test]
    fn a_stall_fails_over_and_a_cancel_does_not() {
        let event = |activity: Activity| RecordedEvent { at_ms: 0, activity };
        let stalled = [event(Activity::Warning(
            crate::engine::STALL_WARNING.into(),
        ))];
        assert_eq!(
            stall_fault(&BranchStatus::Interrupted, &stalled),
            Some("stalled: no harness activity for its stall window".into())
        );
        let cancelled = [event(Activity::Warning("cancelled by alice".into()))];
        assert_eq!(stall_fault(&BranchStatus::Interrupted, &cancelled), None);
        assert_eq!(stall_fault(&BranchStatus::Ready, &stalled), None);
        // Only the latest turn counts: a branch that stalled once, was
        // resumed and then cancelled was not stalled this time.
        let resumed_then_cancelled = [
            event(Activity::Prompt("first".into())),
            event(Activity::Warning(crate::engine::STALL_WARNING.into())),
            event(Activity::Prompt("again".into())),
            event(Activity::Warning("cancelled by alice".into())),
        ];
        assert_eq!(
            stall_fault(&BranchStatus::Interrupted, &resumed_then_cancelled),
            None
        );
        let stalled_again = [
            event(Activity::Prompt("first".into())),
            event(Activity::Warning("cancelled by alice".into())),
            event(Activity::Prompt("again".into())),
            event(Activity::Warning(crate::engine::STALL_WARNING.into())),
        ];
        assert!(stall_fault(&BranchStatus::Interrupted, &stalled_again).is_some());
    }

    /// The advice keeps the entry's cost limit: a candidate whose mean
    /// recorded cost is over it is listed as unavailable with that reason
    /// and never picked, as the router's plan excludes it; without a
    /// limit, or without cost reports, nothing is excluded for cost.
    #[test]
    fn advice_keeps_the_entrys_cost_limit() {
        let dir = tempfile::Builder::new()
            .prefix("by-advice-")
            .tempdir()
            .unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let yard = Yard::open(dir.path()).unwrap();
        for (harness, cost) in [("codex", 3.0), ("claude-code", 0.5)] {
            let mut row = row(harness, BranchOutcome::Merged, Some(cost));
            row.id = format!("{harness}-row");
            yard.store().outcomes().put_outcome(&row).unwrap();
        }
        // Every candidate's executable is this shell, so availability
        // passes and only the cost decides.
        let options = TaskOptions {
            command: Some(vec!["/bin/sh".into()]),
            ..TaskOptions::default()
        };
        let advise_with = |max_usd: Option<f64>| {
            let mut e = entry(&["codex", "claude-code"]);
            e.budget.max_usd = max_usd;
            let table = Fleet {
                entries: [("bugfix".to_owned(), e)].into_iter().collect(),
            };
            let scope = AdviceScope {
                fleet: Some(&table),
                allowed: None,
                own: None,
                remaining_usd: None,
                tree_remaining_usd: None,
                seed: Some(7),
                harnesses: None,
            };
            advise(
                &yard,
                &options,
                "Fix the flaky parser test",
                Some(TaskKind::Bugfix),
                scope,
            )
            .unwrap()
        };
        let limited = advise_with(Some(1.0));
        let codex = &limited.candidates[0];
        assert!(!codex.available, "{codex:?}");
        assert_eq!(
            codex.reason.as_deref(),
            Some("its mean recorded cost $3.00 is over the $1.00 budget")
        );
        assert!(
            limited.candidates[1].available,
            "{:?}",
            limited.candidates[1]
        );
        assert_eq!(
            limited.pick.as_ref().map(|p| p.candidate.harness.as_str()),
            Some("claude-code")
        );
        let unlimited = advise_with(None);
        assert!(
            unlimited.candidates.iter().all(|c| c.available),
            "{unlimited:?}"
        );
    }

    /// A routed branch gets the default stall window unless its task or
    /// entry sets one, and an entry's zero window turns it off.
    #[test]
    fn routed_branches_stall_by_default() {
        let none = Budget::default();
        let routed = budget_for(&none, &none);
        assert_eq!(routed.stall_after, Some(DEFAULT_STALL));
        assert_eq!(routed.stall_action, StallAction::Interrupt);
        let entry = Budget::default()
            .stall_after(Duration::from_secs(60))
            .stall_action(StallAction::Notify);
        let routed = budget_for(&none, &entry);
        assert_eq!(routed.stall_after, Some(Duration::from_secs(60)));
        assert_eq!(routed.stall_action, StallAction::Notify);
        let off = Budget::default().stall_after(Duration::ZERO);
        assert_eq!(budget_for(&none, &off).stall_after, None);
        let task = Budget::default()
            .stall_after(Duration::from_secs(30))
            .stall_action(StallAction::Interrupt);
        let routed = budget_for(&task, &off);
        assert_eq!(routed.stall_after, Some(Duration::from_secs(30)));
        assert_eq!(routed.stall_action, StallAction::Interrupt);
    }

    fn installed(
        id: &str,
        state: crate::inventory::LoginState,
        evidence: crate::inventory::Evidence,
        detail: &str,
    ) -> crate::inventory::HarnessState {
        crate::inventory::HarnessState {
            id: id.into(),
            path: format!("/usr/local/bin/{id}"),
            on_path: true,
            version: Some("1.0.0".into()),
            login: crate::inventory::Login {
                state,
                evidence,
                detail: detail.into(),
            },
            ..Default::default()
        }
    }

    /// Today's machine: Codex verified, Antigravity and Gemini CLI by
    /// GEMINI_API_KEY, Claude Code logged out, Goose unknown, Aider with no
    /// profile, Qwen Code off PATH, OpenCode at its limit.
    fn machine() -> crate::inventory::Inventory {
        use crate::inventory::{Evidence, LoginState, Quota};
        let mut qwen = installed("qwen-code", LoginState::LoggedIn, Evidence::Likely, "");
        qwen.on_path = false;
        let mut opencode = installed("opencode", LoginState::LoggedIn, Evidence::Likely, "");
        opencode.quota = Some(Quota {
            limit_reached: true,
            ..Quota::default()
        });
        crate::inventory::Inventory {
            host: "here".into(),
            harnesses: vec![
                installed(
                    "claude-code",
                    LoginState::LoggedOut,
                    Evidence::Likely,
                    "no ~/.claude/.credentials.json and no ANTHROPIC_API_KEY",
                ),
                installed(
                    "codex",
                    LoginState::LoggedIn,
                    Evidence::Verified,
                    "codex login status: Logged in using ChatGPT",
                ),
                installed(
                    "antigravity",
                    LoginState::LoggedIn,
                    Evidence::Likely,
                    "GEMINI_API_KEY is set (an API key; not checked)",
                ),
                installed(
                    "gemini-cli",
                    LoginState::LoggedIn,
                    Evidence::Likely,
                    "GEMINI_API_KEY is set (an API key; not checked)",
                ),
                opencode,
                installed(
                    "goose",
                    LoginState::Unknown,
                    Evidence::None,
                    "no way to tell",
                ),
                installed("aider", LoginState::LoggedIn, Evidence::Likely, "a key"),
                qwen,
            ],
            ..Default::default()
        }
    }

    #[test]
    fn the_inventory_default_routes_among_what_is_logged_in_here() {
        let built = inventory_default(&machine(), None, false);
        assert_eq!(built.names(), ["codex", "antigravity", "gemini-cli"]);
        // Safe defaults: no model; no effort where it would be refused
        // (Codex writes it into a home only an isolated branch has).
        assert!(built
            .entry
            .candidates
            .iter()
            .all(|c| c.model.is_none() && c.effort.is_none() && c.command.is_none()));
        let e = &built.entry;
        assert_eq!(
            (e.attempts, e.failover, e.exploration, e.budget.max_usd),
            (1, false, DEFAULT_EXPLORATION, None)
        );
        // The judge is the first candidate, and one that can be denied
        // every tool.
        assert_eq!(e.judge.as_ref().map(|j| j.harness.as_str()), Some("codex"));
        let why: BTreeMap<&str, &str> = built
            .excluded
            .iter()
            .map(|x| (x.candidate.harness.as_str(), x.reason.as_str()))
            .collect();
        assert_eq!(
            why.keys().copied().collect::<Vec<_>>(),
            ["aider", "claude-code", "goose", "opencode", "qwen-code"]
        );
        assert!(
            why["claude-code"].contains("logged out (no ~/.claude/.credentials.json"),
            "{why:?}"
        );
        assert!(why["goose"].contains("login cannot be told (no way to tell)"));
        assert!(why["aider"].contains("no profile"));
        assert!(why["qwen-code"].contains("not on PATH"));
        assert!(why["opencode"].contains("usage limit"));

        // Isolated, Codex takes effort medium.
        let isolated = inventory_default(&machine(), None, true);
        assert_eq!(
            isolated.names(),
            ["codex effort=medium", "antigravity", "gemini-cli"]
        );
        assert_eq!(
            isolated.entry.judge.as_ref().and_then(|j| j.effort),
            Some(Effort::Medium)
        );

        // [defaults] harness goes first, by harness or profile ID; a judge
        // that cannot route permission requests is passed over.
        let preferred = inventory_default(&machine(), Some("antigravity"), false);
        assert_eq!(preferred.names(), ["antigravity", "codex", "gemini-cli"]);
        assert_eq!(
            preferred.entry.judge.map(|j| j.harness),
            Some("codex".to_owned())
        );
        let profile = inventory_default(&machine(), Some("gemini-cli-acp"), false);
        assert_eq!(profile.names(), ["gemini-cli-acp", "codex", "antigravity"]);
        // One that is not logged in changes nothing.
        let absent = inventory_default(&machine(), Some("claude-code"), false);
        assert_eq!(absent.names(), built.names());

        // Its fleet answers every kind from the one entry, and says so.
        let fleet = built.fleet();
        for kind in TaskKind::ALL {
            assert_eq!(
                fleet.entry(kind).map(|(name, _)| name),
                Some(INVENTORY_ENTRY)
            );
        }
        assert_eq!(entry_label(INVENTORY_ENTRY), "the inventory default");
        assert_eq!(entry_label("docs"), "[fleet.docs]");
        let routed = FleetActivity::Routed(Box::new(RouteDecision {
            kind: TaskKind::Bugfix,
            kind_source: "classifier".into(),
            entry: Some(INVENTORY_ENTRY.into()),
            candidate: FleetCandidate::new("codex"),
            attempt: 1,
            attempts: 1,
            reason: "sampled".into(),
            failover: true,
            fallbacks: Vec::new(),
            tried: Vec::new(),
            budget_usd: None,
            spent_usd: 0.0,
            from: None,
            environment: None,
            connectors: Vec::new(),
        }));
        assert_eq!(
            routed.describe(),
            "routed bugfix (classifier) by the inventory default to codex: sampled"
        );

        // Nothing logged in: no candidates, and the router says why.
        let none = inventory_default(
            &crate::inventory::Inventory {
                harnesses: machine().harnesses[..1].to_vec(),
                ..Default::default()
            },
            None,
            false,
        );
        assert!(none.entry.candidates.is_empty() && none.entry.judge.is_none());
        let err = plan(
            TaskKind::Other,
            "flag",
            Vec::new(),
            INVENTORY_ENTRY,
            &none.entry,
            &[],
            &all_ok,
            1,
            1,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("the inventory default is unavailable"),
            "{err}"
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

    /// A bare harness pools every arm of its outcomes; a candidate naming
    /// a model gets its own arm; another kind or harness gives nothing.
    #[test]
    fn pooled_arms_and_risk_words() {
        let bugfix = |harness: &str, outcome, cost, model: Option<&str>| OutcomeRecord {
            kind: TaskKind::Bugfix,
            model: model.map(str::to_owned),
            ..row(harness, outcome, cost)
        };
        let rows = vec![
            bugfix("codex", BranchOutcome::Merged, Some(2.0), Some("large")),
            bugfix("codex", BranchOutcome::Failed, Some(0.5), Some("small")),
            bugfix("gemini-cli", BranchOutcome::Ready, Some(1.0), None),
        ];
        let bare = pooled(&rows, TaskKind::Bugfix, &FleetCandidate::new("codex")).unwrap();
        assert_eq!((bare.runs, bare.merged, bare.failed), (2, 1, 1));
        assert_eq!(bare.model, None);
        assert_eq!(bare.mean_cost_usd, Some(1.25));
        let mut small = FleetCandidate::new("codex");
        small.model = Some("small".into());
        let own = pooled(&rows, TaskKind::Bugfix, &small).unwrap();
        assert_eq!((own.runs, own.failed), (1, 1));
        assert_eq!(own.model.as_deref(), Some("small"));
        assert!(pooled(&rows, TaskKind::Docs, &FleetCandidate::new("codex")).is_none());
        assert!(pooled(&rows, TaskKind::Bugfix, &FleetCandidate::new("amp")).is_none());
        assert_eq!(
            risk_words("Delete the production migration before the payment release"),
            ["delete", "production", "migration", "payment", "release"]
        );
        assert!(risk_words("Fix the dropdown on the product page").is_empty());
    }
}
