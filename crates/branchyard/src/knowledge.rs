//! Repository knowledge: short rules a person adopted for the agents
//! working in a repository, learned from how earlier branches went. See
//! `docs/knowledge.md`.
//!
//! An entry is proposed, by a person or by a **distiller** at a branch's
//! end, and is used only once a person adopts it. A distiller never adopts
//! anything. Two distillers exist:
//!
//! - the deterministic **extractor**, which needs no model: the
//!   corrections a person sent into the branch after its task (`by send`,
//!   `by send --steer`), the comments of `by review` and the pull request
//!   review comments `by pr --watch` delivered, each proposed as written;
//! - a **harness distiller**: any [`Judge`] (usually a [`crate::HarnessJudge`],
//!   which runs read-only on a scratch branch) answering one strict JSON
//!   object of proposed entries. Invalid output falls back to the
//!   extractor's proposals, saying why.
//!
//! Adopted entries that match a branch (its repository, the paths its task
//! touches when they are known, its kind) are given to its harness each
//! turn in the managed instructions block, most specific first, within a
//! token budget; the `provisioned` event lists their ids.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::fleet::{classify, recorded_route, FleetActivity, TaskKind};
use crate::judge::{Judge, JudgedBy};
use crate::record::Recorder;
use crate::state::Record;
use crate::{Activity, Error, RecordedEvent, Yard};
use branchyard_support::time::now_ms;

/// Where an entry applies: the whole repository, files matching a path
/// glob, a kind of task, or both of the last two. Serialized as
/// `{"path": "src/**", "kind": "bugfix"}`, either key omitted when unset.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeScope {
    /// A glob over repository paths (`src/parser/**`, `*.sql`); the entry
    /// applies when the task touches a matching file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The entry applies to tasks of this kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TaskKind>,
}

impl KnowledgeScope {
    /// The whole repository.
    pub fn repo() -> KnowledgeScope {
        KnowledgeScope::default()
    }

    /// How specific the scope is: a path and a kind 3, a path 2, a kind 1,
    /// the repository 0. Provisioning gives the most specific first.
    pub fn specificity(&self) -> u8 {
        match (&self.path, &self.kind) {
            (Some(_), Some(_)) => 3,
            (Some(_), None) => 2,
            (None, Some(_)) => 1,
            (None, None) => 0,
        }
    }

    /// `repo`, `path src/**`, `kind bugfix` or `path src/** kind bugfix`.
    pub fn describe(&self) -> String {
        match (&self.path, &self.kind) {
            (None, None) => "repo".into(),
            (Some(path), None) => format!("path {path}"),
            (None, Some(kind)) => format!("kind {kind}"),
            (Some(path), Some(kind)) => format!("path {path} kind {kind}"),
        }
    }

    /// Refuse a path that is not a usable glob.
    pub fn check(&self) -> Result<(), Error> {
        if let Some(path) = &self.path {
            if path.trim().is_empty() {
                return Err(Error::Unsupported("a knowledge path glob is empty".into()));
            }
            glob::Pattern::new(path).map_err(|e| {
                Error::Unsupported(format!("{path:?} is not a usable path glob: {e}"))
            })?;
        }
        Ok(())
    }

    /// Whether the scope applies to a task of `kind` touching `files`.
    /// A path scope never matches while no file is known.
    pub fn matches(&self, kind: Option<TaskKind>, files: &[String]) -> bool {
        if let Some(want) = self.kind {
            if kind != Some(want) {
                return false;
            }
        }
        match &self.path {
            None => true,
            Some(path) => match glob::Pattern::new(path) {
                Ok(pattern) => {
                    let options = glob::MatchOptions {
                        case_sensitive: true,
                        require_literal_separator: false,
                        require_literal_leading_dot: false,
                    };
                    files.iter().any(|f| pattern.matches_with(f, options))
                }
                Err(_) => false,
            },
        }
    }
}

/// Where an entry came from. Serialized as an object tagged by `from`:
/// `{"from": "person", "name": "ana"}` or `{"from": "branch", "branch":
/// "fix-parser", "turn": 2, "via": "send"}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum KnowledgeSource {
    /// Written by a person (`by knowledge add`).
    Person { name: String },
    /// Proposed by a distiller from a branch: from the correction sent
    /// before turn `turn`, or (`turn` unset) from the branch as a whole.
    /// `via` is what it came from: `send`, `steer`, `review`,
    /// `pull_request`, or `harness <id>` for a harness distiller.
    Branch {
        branch: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u32>,
        via: String,
    },
}

impl KnowledgeSource {
    /// One line for people.
    pub fn describe(&self) -> String {
        match self {
            KnowledgeSource::Person { name } => format!("added by {name}"),
            KnowledgeSource::Branch { branch, turn, via } => match turn {
                Some(turn) => format!("from {branch} turn {turn} ({via})"),
                None => format!("from {branch} ({via})"),
            },
        }
    }
}

/// Whether an entry is used.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeStatus {
    /// Waiting for a person; never given to a harness.
    Proposed,
    /// A person adopted it: matching branches are given it.
    Adopted,
    /// A person rejected it; the same text is not proposed again.
    Rejected,
}

impl KnowledgeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            KnowledgeStatus::Proposed => "proposed",
            KnowledgeStatus::Adopted => "adopted",
            KnowledgeStatus::Rejected => "rejected",
        }
    }
}

impl fmt::Display for KnowledgeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for KnowledgeStatus {
    type Err = Error;
    fn from_str(text: &str) -> Result<KnowledgeStatus, Error> {
        match text {
            "proposed" => Ok(KnowledgeStatus::Proposed),
            "adopted" => Ok(KnowledgeStatus::Adopted),
            "rejected" => Ok(KnowledgeStatus::Rejected),
            other => Err(Error::Unsupported(format!(
                "{other:?} is not a knowledge status; use proposed, adopted or rejected"
            ))),
        }
    }
}

/// One knowledge entry, durable in the store; entries outlive the
/// branches they came from.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    /// Assigned by the store, from 1.
    pub id: u64,
    pub scope: KnowledgeScope,
    pub text: String,
    pub source: KnowledgeSource,
    pub status: KnowledgeStatus,
    /// Milliseconds since the Unix epoch.
    pub created_ms: u64,
    /// Who adopted it, while it is adopted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_by: Option<String>,
    /// When a person last adopted, rejected or edited it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_ms: Option<u64>,
    /// Why it was proposed, or why it was rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The knowledge store: entries per repository. Rows outlive branches.
pub(crate) trait KnowledgeBackend: Send + Sync + fmt::Debug {
    /// Store `entry` as a new entry; its `id` and `created_ms` are
    /// assigned. Returns the stored copy.
    fn add_knowledge(&self, entry: &KnowledgeEntry) -> Result<KnowledgeEntry, Error>;
    fn knowledge(&self, id: u64) -> Result<Option<KnowledgeEntry>, Error>;
    /// Every entry, by id.
    fn knowledge_entries(&self) -> Result<Vec<KnowledgeEntry>, Error>;
    /// Replace the entry with `entry.id`, if its status is still
    /// `expected`; false when it is gone or changed meanwhile.
    fn put_knowledge(
        &self,
        entry: &KnowledgeEntry,
        expected: KnowledgeStatus,
    ) -> Result<bool, Error>;
    /// Delete the entry; false when there was none.
    fn remove_knowledge(&self, id: u64) -> Result<bool, Error>;
}

/// When a branch's end distills it automatically.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistillTrigger {
    /// The branch merged.
    Merged,
    /// A judge proposed it as the pick.
    JudgedBest,
    /// A turn of it ended ready.
    Ready,
}

impl DistillTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            DistillTrigger::Merged => "merged",
            DistillTrigger::JudgedBest => "judged_best",
            DistillTrigger::Ready => "ready",
        }
    }
}

/// How a yard learns and uses knowledge; see [`Yard::use_knowledge`].
#[derive(Clone)]
pub struct KnowledgeSettings {
    /// Give matching adopted entries to each turn's harness. Default on.
    pub provision: bool,
    /// At most about this many tokens of entries per turn (four
    /// characters a token). Default [`DEFAULT_BUDGET_TOKENS`].
    pub budget_tokens: usize,
    /// When a top-level branch is distilled on its own. Default: merged
    /// and judged best. Empty: only `by knowledge distill`.
    pub distill_on: Vec<DistillTrigger>,
    /// A harness distiller, tried before the extractor's proposals; `None`
    /// proposes only the extractor's.
    pub distiller: Option<Arc<dyn Judge>>,
}

/// The default token budget for provisioned knowledge.
pub const DEFAULT_BUDGET_TOKENS: usize = 1500;

impl Default for KnowledgeSettings {
    fn default() -> KnowledgeSettings {
        KnowledgeSettings {
            provision: true,
            budget_tokens: DEFAULT_BUDGET_TOKENS,
            distill_on: vec![DistillTrigger::Merged, DistillTrigger::JudgedBest],
            distiller: None,
        }
    }
}

impl fmt::Debug for KnowledgeSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KnowledgeSettings")
            .field("provision", &self.provision)
            .field("budget_tokens", &self.budget_tokens)
            .field("distill_on", &self.distill_on)
            .field("distiller", &self.distiller.as_ref().map(|d| d.name()))
            .finish()
    }
}

/// What to add with [`Yard::add_knowledge`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewKnowledge {
    pub text: String,
    pub scope: KnowledgeScope,
    /// Add it as proposed instead of adopted.
    pub propose: bool,
    pub note: Option<String>,
}

/// A change to an entry with [`Yard::edit_knowledge`]; `None` keeps it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KnowledgeEdit {
    pub text: Option<String>,
    /// `Some(None)` clears the path.
    pub path: Option<Option<String>>,
    /// `Some(None)` clears the kind.
    pub kind: Option<Option<TaskKind>>,
}

/// What [`Yard::distill`] proposed.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Distilled {
    pub branch: String,
    /// The entries proposed now, new in the store.
    pub proposed: Vec<KnowledgeEntry>,
    /// Proposals left out because an entry with the same text and scope
    /// exists already (in any status, so a rejection sticks).
    pub duplicates: u32,
    /// The extractor, the harness distiller, or the extractor after the
    /// harness distiller's answer was refused (and why).
    pub by: JudgedBy,
    /// What started it: `merged`, `judged_best`, `ready` or `asked`.
    pub trigger: String,
}

/// Knowledge activity on a branch, recorded as [`Activity::Knowledge`].
/// Serialized as an object tagged by `type`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KnowledgeActivity {
    /// The branch was distilled: these entries were proposed.
    Distilled {
        ids: Vec<u64>,
        duplicates: u32,
        /// `deterministic`, a harness, or the fallback from one.
        by: String,
        /// Why a harness distiller's answer was not used.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fallback: Option<String>,
        trigger: String,
    },
}

impl KnowledgeActivity {
    /// One line for logs.
    pub fn describe(&self) -> String {
        match self {
            KnowledgeActivity::Distilled {
                ids,
                duplicates,
                by,
                fallback,
                trigger,
            } => {
                let proposed = match ids.is_empty() {
                    true => "no new knowledge proposed".to_owned(),
                    false => format!(
                        "proposed knowledge {}",
                        ids.iter()
                            .map(|id| format!("#{id}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                };
                let mut line = format!("distilled ({trigger}, {by}): {proposed}");
                if *duplicates > 0 {
                    line.push_str(&format!("; {duplicates} already known"));
                }
                if let Some(why) = fallback {
                    line.push_str(&format!("; the distiller's answer was not used: {why}"));
                }
                line
            }
        }
    }
}

/// The longest entry text accepted, in characters.
pub const TEXT_MAX: usize = 600;
/// Shorter corrections are not proposed.
const TEXT_MIN: usize = 12;
/// Most entries one harness distiller answer may propose.
pub const DISTILLED_MAX: usize = 5;

/// `text`, trimmed and with its whitespace collapsed to single spaces
/// within lines, refused when empty or over [`TEXT_MAX`].
fn clean(text: &str) -> Result<String, Error> {
    let lines: Vec<String> = text
        .trim()
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    let text = lines.join("\n").trim().to_owned();
    if text.is_empty() {
        return Err(Error::Unsupported("a knowledge entry needs text".into()));
    }
    let count = text.chars().count();
    if count > TEXT_MAX {
        return Err(Error::Unsupported(format!(
            "a knowledge entry is at most {TEXT_MAX} characters, not {count}"
        )));
    }
    Ok(text)
}

/// The key two entries are duplicates by: their text, case and spacing
/// aside, and their scope.
fn same(a: &KnowledgeEntry, text: &str, scope: &KnowledgeScope) -> bool {
    let key = |t: &str| {
        t.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    &a.scope == scope && key(&a.text) == key(text)
}

// ---------------------------------------------------------------------------
// People's operations

pub(crate) fn list(
    yard: &Yard,
    status: Option<KnowledgeStatus>,
) -> Result<Vec<KnowledgeEntry>, Error> {
    Ok(yard
        .store()
        .knowledge()
        .knowledge_entries()?
        .into_iter()
        .filter(|e| status.is_none_or(|s| e.status == s))
        .collect())
}

pub(crate) fn get(yard: &Yard, id: u64) -> Result<KnowledgeEntry, Error> {
    yard.store()
        .knowledge()
        .knowledge(id)?
        .ok_or(Error::UnknownKnowledge(id))
}

pub(crate) fn add(yard: &Yard, new: &NewKnowledge, by: &str) -> Result<KnowledgeEntry, Error> {
    new.scope.check()?;
    let text = clean(&new.text)?;
    let now = now_ms();
    let (status, adopted_by, decided_ms) = match new.propose {
        true => (KnowledgeStatus::Proposed, None, None),
        false => (KnowledgeStatus::Adopted, Some(by.to_owned()), Some(now)),
    };
    yard.store().knowledge().add_knowledge(&KnowledgeEntry {
        id: 0,
        scope: new.scope.clone(),
        text,
        source: KnowledgeSource::Person {
            name: by.to_owned(),
        },
        status,
        created_ms: now,
        adopted_by,
        decided_ms,
        note: new.note.clone(),
    })
}

/// Move an entry to `status` as `by`, from whatever it is now.
pub(crate) fn decide(
    yard: &Yard,
    id: u64,
    status: KnowledgeStatus,
    by: &str,
    note: Option<&str>,
) -> Result<KnowledgeEntry, Error> {
    let store = yard.store();
    for _ in 0..8 {
        let current = get(yard, id)?;
        let mut entry = current.clone();
        entry.status = status;
        entry.decided_ms = Some(now_ms());
        entry.adopted_by = match status {
            KnowledgeStatus::Adopted => Some(by.to_owned()),
            _ => None,
        };
        if let Some(note) = note.filter(|n| !n.trim().is_empty()) {
            entry.note = Some(note.trim().to_owned());
        }
        if store.knowledge().put_knowledge(&entry, current.status)? {
            return Ok(entry);
        }
    }
    Err(Error::State(format!(
        "knowledge #{id} kept changing; try again"
    )))
}

pub(crate) fn edit(
    yard: &Yard,
    id: u64,
    change: &KnowledgeEdit,
    by: &str,
) -> Result<KnowledgeEntry, Error> {
    let store = yard.store();
    let current = get(yard, id)?;
    let mut entry = current.clone();
    if let Some(text) = &change.text {
        entry.text = clean(text)?;
    }
    if let Some(path) = &change.path {
        entry.scope.path = path.clone().filter(|p| !p.trim().is_empty());
    }
    if let Some(kind) = change.kind {
        entry.scope.kind = kind;
    }
    entry.scope.check()?;
    entry.decided_ms = Some(now_ms());
    // Editing an adopted entry keeps it adopted, now by the editor.
    if entry.status == KnowledgeStatus::Adopted {
        entry.adopted_by = Some(by.to_owned());
    }
    match store.knowledge().put_knowledge(&entry, current.status)? {
        true => Ok(entry),
        false => Err(Error::State(format!(
            "knowledge #{id} changed while it was being edited; try again"
        ))),
    }
}

pub(crate) fn remove(yard: &Yard, id: u64) -> Result<KnowledgeEntry, Error> {
    let entry = get(yard, id)?;
    match yard.store().knowledge().remove_knowledge(id)? {
        true => Ok(entry),
        false => Err(Error::UnknownKnowledge(id)),
    }
}

/// Adopted entries as an `AGENTS.md`-style Markdown file: the repository's
/// first, then each path and kind, most specific last, each with its id.
pub fn export(entries: &[KnowledgeEntry]) -> String {
    let mut adopted: Vec<&KnowledgeEntry> = entries
        .iter()
        .filter(|e| e.status == KnowledgeStatus::Adopted)
        .collect();
    adopted.sort_by_key(|e| (e.scope.specificity(), e.scope.describe(), e.id));
    let mut text = String::from(
        "# Repository knowledge\n\n\
         Rules people adopted for agents working in this repository, exported by \
         `by knowledge export`. Each ends with its entry id.\n",
    );
    let mut heading = None;
    for entry in adopted {
        let this = match (&entry.scope.path, entry.scope.kind) {
            (None, None) => "Everywhere".to_owned(),
            (Some(path), None) => format!("Files matching `{path}`"),
            (None, Some(kind)) => format!("{} tasks", capitalized(kind.as_str())),
            (Some(path), Some(kind)) => format!(
                "{} tasks on files matching `{path}`",
                capitalized(kind.as_str())
            ),
        };
        if heading.as_ref() != Some(&this) {
            text.push_str(&format!("\n## {this}\n\n"));
            heading = Some(this);
        }
        let body = entry.text.replace('\n', "\n  ");
        text.push_str(&format!("- {body} (k{})\n", entry.id));
    }
    text
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Use: what a turn is given

/// The adopted entries a turn is given, and the block that carries them.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Briefing {
    pub ids: Vec<u64>,
    /// Matching entries left out because they did not fit the budget.
    pub omitted: Vec<u64>,
    pub text: Option<String>,
}

/// Roughly how many tokens `text` is: four characters a token.
fn tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// The block for `entries`, matching and adopted, most specific first,
/// within `budget_tokens`.
pub(crate) fn brief(
    entries: &[KnowledgeEntry],
    kind: Option<TaskKind>,
    files: &[String],
    budget_tokens: usize,
) -> Briefing {
    let mut matching: Vec<&KnowledgeEntry> = entries
        .iter()
        .filter(|e| e.status == KnowledgeStatus::Adopted && e.scope.matches(kind, files))
        .collect();
    matching.sort_by_key(|e| (std::cmp::Reverse(e.scope.specificity()), e.id));
    let header = "## Repository knowledge\n\
                  Rules people adopted for this repository, most specific first. Follow \
                  them unless the task says otherwise.\n";
    let mut used = tokens(header);
    let mut lines = Vec::new();
    let mut briefing = Briefing::default();
    for entry in matching {
        let line = match entry.scope.specificity() {
            0 => format!("- [k{}] {}", entry.id, entry.text),
            _ => format!(
                "- [k{}] ({}) {}",
                entry.id,
                entry.scope.describe(),
                entry.text
            ),
        };
        let cost = tokens(&line) + 1;
        if used + cost > budget_tokens {
            briefing.omitted.push(entry.id);
            continue;
        }
        used += cost;
        briefing.ids.push(entry.id);
        lines.push(line);
    }
    if !lines.is_empty() {
        briefing.text = Some(format!("{header}{}", lines.join("\n")));
    }
    briefing
}

/// The kind a branch's task has: its route's, else the classifier's.
#[allow(clippy::map_unwrap_or)] // ratchet: branchyard
pub(crate) fn kind_of(record: &Record, events: &[RecordedEvent]) -> TaskKind {
    recorded_route(events)
        .map(|d| d.kind)
        .unwrap_or_else(|| classify(&record.info.prompt).kind)
}

/// The files a branch's task is known to touch: those its candidate
/// changed, and repository paths its prompt names.
pub(crate) fn files_of(yard: &Yard, record: &Record) -> Vec<String> {
    let mut files: BTreeSet<String> = BTreeSet::new();
    files.extend(changed_files(yard, record));
    for word in record
        .info
        .prompt
        .split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | ',' | ';'))
    {
        let word = word.trim_end_matches([':', '.']).trim_start_matches("./");
        if word.is_empty() || word.len() > 200 || word.starts_with('/') || word.contains("..") {
            continue;
        }
        if !(word.contains('/') || word.contains('.')) {
            continue;
        }
        if yard.root().join(word).exists() {
            files.insert(word.to_owned());
        }
    }
    files.into_iter().collect()
}

/// The paths `record`'s candidate changes against its base.
pub(crate) fn changed_files(yard: &Yard, record: &Record) -> Vec<String> {
    match &record.info.candidate {
        Some(candidate) => crate::git::diff(yard.root(), &record.info.base, &candidate.commit)
            .map(|diff| crate::compare::diff_files(&diff))
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

/// What `record`'s next turn is given, under the yard's settings.
pub(crate) fn briefing_for(yard: &Yard, record: &Record) -> Briefing {
    let settings = yard.knowledge_settings();
    if !settings.provision {
        return Briefing::default();
    }
    let Ok(entries) = yard.store().knowledge().knowledge_entries() else {
        return Briefing::default();
    };
    if !entries.iter().any(|e| e.status == KnowledgeStatus::Adopted) {
        return Briefing::default();
    }
    let events = crate::record::read(&yard.store(), &record.info.name).unwrap_or_default();
    // A judge's or distiller's scratch branch gets none: it judges the
    // work, it does not do it.
    if is_scratch(&events) {
        return Briefing::default();
    }
    let kind = kind_of(record, &events);
    let files = files_of(yard, record);
    brief(&entries, Some(kind), &files, settings.budget_tokens)
}

fn is_scratch(events: &[RecordedEvent]) -> bool {
    events.iter().any(|e| {
        matches!(&e.activity, Activity::Fleet(a) if matches!(a.as_ref(), FleetActivity::Judging { .. }))
    })
}

// ---------------------------------------------------------------------------
// Learning: distillers

/// One proposal before it is stored.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Proposal {
    pub text: String,
    pub scope: KnowledgeScope,
    pub turn: Option<u32>,
    pub via: String,
    pub note: Option<String>,
}

/// Prompts that start with this are Branchyard's own (plan mode, plan
/// approval, goal follow-ups), never a person's correction.
pub(crate) const GENERATED: &str = "[branchyard ";

/// Replies that only keep a turn going; never proposed.
const CONTINUATIONS: &[&str] = &[
    "continue",
    "go on",
    "keep going",
    "proceed",
    "yes",
    "ok",
    "okay",
    "lgtm",
    "looks good",
    "thanks",
    "thank you",
    "go ahead",
    "do it",
];

/// A person's text, without an inbox block or a rewind's summary.
fn own_text(prompt: &str) -> &str {
    let mut text = prompt;
    if let Some(at) = text.rfind("\n\n## Your task now\n") {
        text = &text[at + "\n\n## Your task now\n".len()..];
    }
    if let Some(end) = text.find(crate::inbox::CLOSE_TAG) {
        if text.trim_start().starts_with(crate::inbox::OPEN_TAG) {
            text = &text[end + crate::inbox::CLOSE_TAG.len()..];
        }
    }
    text.trim()
}

fn worth_proposing(text: &str) -> bool {
    let plain = text.trim().trim_end_matches(['.', '!']).to_lowercase();
    text.chars().count() >= TEXT_MIN && !CONTINUATIONS.contains(&plain.as_str())
}

/// Shorten `text` to [`TEXT_MAX`] characters at a word.
fn bounded(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= TEXT_MAX {
        return text.to_owned();
    }
    let cut: String = text.chars().take(TEXT_MAX - 3).collect();
    let cut = match cut.rfind(char::is_whitespace) {
        Some(at) if at > TEXT_MAX / 2 => cut[..at].to_owned(),
        _ => cut,
    };
    format!("{}...", cut.trim_end())
}

/// `by review`'s comments in `text`: (file, comment).
fn review_comments(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut file = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("File: ") {
            file = Some(path.trim().to_owned());
        } else if let Some(body) = line.strip_prefix("User comment: ") {
            let body = body.trim();
            let body = body
                .strip_prefix('"')
                .and_then(|b| b.strip_suffix('"'))
                .unwrap_or(body);
            let body = body
                .replace("\\n", "\n")
                .replace("\\r", "")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\");
            if let Some(file) = file.take() {
                found.push((file, body));
            }
        }
    }
    found
}

/// The review feedback in a `by pr --watch` prompt: (path, comment). CI
/// failures are not corrections and are left out.
fn pull_request_feedback(text: &str) -> Vec<(Option<String>, String)> {
    let mut items: Vec<String> = Vec::new();
    for line in text.lines().skip(1) {
        let numbered = line
            .split_once(". ")
            .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        match numbered {
            Some((_, rest)) => items.push(rest.to_owned()),
            None => {
                if let Some(last) = items.last_mut() {
                    if line.starts_with("Address it on this branch") {
                        break;
                    }
                    last.push('\n');
                    last.push_str(line);
                }
            }
        }
    }
    let mut found = Vec::new();
    for item in items {
        let (head, body) = item.split_once('\n').unwrap_or((item.as_str(), ""));
        if head.starts_with("CI check") || head.starts_with("local check") {
            continue;
        }
        let path = head
            .split_once(" on ")
            .map(|(_, place)| place.trim_end_matches(':'))
            .map(|place| match place.rsplit_once(':') {
                Some((path, line)) if line.chars().all(|c| c.is_ascii_digit()) => path,
                _ => place,
            })
            .map(str::to_owned);
        let body: Vec<&str> = body
            .lines()
            .map(|l| l.trim_start_matches('>').trim())
            .filter(|l| !l.is_empty())
            .collect();
        if !body.is_empty() {
            found.push((path, body.join(" ")));
        }
    }
    found
}

/// The deterministic extractor: the person's corrections in `events`
/// after the task, review comments and pull request review feedback.
pub(crate) fn extract(events: &[RecordedEvent]) -> Vec<Proposal> {
    let mut proposals = Vec::new();
    let mut turn = 0u32;
    let mut push = |text: &str, scope: KnowledgeScope, turn: u32, via: &str| {
        if worth_proposing(text) {
            proposals.push(Proposal {
                text: bounded(text),
                scope,
                turn: Some(turn),
                via: via.to_owned(),
                note: None,
            });
        }
    };
    for event in events {
        let (text, via) = match &event.activity {
            Activity::Prompt(prompt) => {
                turn += 1;
                if turn == 1 {
                    continue;
                }
                (own_text(prompt), "send")
            }
            Activity::Steered { text, .. } => (own_text(text), "steer"),
            _ => continue,
        };
        if text.is_empty()
            || text.starts_with(GENERATED)
            || text.starts_with(crate::inbox::OPEN_TAG)
        {
            continue;
        }
        let at = turn.max(1);
        if text.starts_with("Feedback on pull request #") {
            for (path, body) in pull_request_feedback(text) {
                let scope = KnowledgeScope { path, kind: None };
                push(&body, scope, at, "pull_request");
            }
            continue;
        }
        let comments = review_comments(text);
        if !comments.is_empty() {
            for (file, body) in comments {
                let scope = KnowledgeScope {
                    path: Some(file),
                    kind: None,
                };
                push(&body, scope, at, "review");
            }
            continue;
        }
        push(text, KnowledgeScope::repo(), at, via);
    }
    proposals
}

/// A harness distiller's answer, parsed strictly.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    entries: Vec<AnswerEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerEntry {
    text: String,
    path: Option<String>,
    kind: Option<String>,
    why: String,
}

/// Parse a distiller's answer: one JSON object (optionally in one fenced
/// block) with exactly `entries`, at most `DISTILLED_MAX` of them, each
/// with exactly `text`, `path` (a glob or null), `kind` (a task kind or
/// null) and `why`.
pub fn parse_distilled(text: &str) -> Result<Vec<(String, KnowledgeScope, String)>, String> {
    let mut body = text.trim();
    if let Some(rest) = body.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        body = rest
            .strip_suffix("```")
            .ok_or("a fenced answer must end with its fence")?
            .trim();
    }
    if !body.starts_with('{') {
        return Err("the answer is not a JSON object".into());
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("not a distiller's answer: {e}"))?;
    // Every key of every entry is required, `null` included.
    for (index, entry) in value["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let keys: BTreeSet<&str> = entry
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        if keys != BTreeSet::from(["text", "path", "kind", "why"]) {
            return Err(format!(
                "entries[{index}] must have exactly text, path, kind and why"
            ));
        }
    }
    let answer: Answer =
        serde_json::from_value(value).map_err(|e| format!("not a distiller's answer: {e}"))?;
    if answer.entries.len() > DISTILLED_MAX {
        return Err(format!(
            "at most {DISTILLED_MAX} entries, not {}",
            answer.entries.len()
        ));
    }
    let mut out = Vec::new();
    for (index, entry) in answer.entries.into_iter().enumerate() {
        let text = clean(&entry.text).map_err(|e| format!("entries[{index}].text: {e}"))?;
        let kind = entry
            .kind
            .map(|k| k.parse::<TaskKind>())
            .transpose()
            .map_err(|e| format!("entries[{index}].kind: {e}"))?;
        let scope = KnowledgeScope {
            path: entry.path.filter(|p| !p.trim().is_empty()),
            kind,
        };
        scope
            .check()
            .map_err(|e| format!("entries[{index}].path: {e}"))?;
        if entry.why.trim().is_empty() {
            return Err(format!("entries[{index}].why is empty"));
        }
        out.push((text, scope, entry.why.trim().to_owned()));
    }
    Ok(out)
}

/// The prompt a harness distiller gets.
pub(crate) fn distiller_prompt(
    task: &str,
    corrections: &[Proposal],
    diffstat: &str,
    last_message: &str,
    adopted: &[KnowledgeEntry],
) -> String {
    let mut text = String::from(
        "You are distilling durable knowledge about a repository from one finished branch \
         of work, for future agents working in it. Read only: do not change, create or run \
         anything.\n\nPropose at most 5 short, general rules a future agent should follow in \
         this repository: conventions, pitfalls, commands, where things live. Learn mostly \
         from the corrections the person had to make. Do not propose facts about this task \
         alone, and do not repeat the adopted rules below. Proposing nothing is fine.\n",
    );
    text.push_str(&format!("\n## Task\n{task}\n"));
    text.push_str("\n## Corrections and review comments from the person\n");
    if corrections.is_empty() {
        text.push_str("(none)\n");
    }
    for c in corrections {
        let at = match &c.scope.path {
            Some(path) => format!(" (on {path})"),
            None => String::new(),
        };
        text.push_str(&format!("- [{}{at}] {}\n", c.via, c.text));
    }
    text.push_str(&format!("\n## What changed\n{}\n", diffstat.trim_end()));
    let last: String = last_message.chars().take(3000).collect();
    text.push_str(&format!(
        "\n## The agent's last reply\n{}\n",
        last.trim_end()
    ));
    text.push_str("\n## Already adopted\n");
    if adopted.is_empty() {
        text.push_str("(none)\n");
    }
    for entry in adopted {
        text.push_str(&format!("- ({}) {}\n", entry.scope.describe(), entry.text));
    }
    text.push_str(
        "\n## Answer\nReply with only one JSON object and nothing else, of this shape:\n\
         {\"entries\": [{\"text\": \"one rule, at most 600 characters\", \"path\": \"a path \
         glob such as src/parser/** or null\", \"kind\": \"bugfix, feature, refactor, review, \
         research, docs, migration, tests, other or null\", \"why\": \"one sentence\"}]}\n",
    );
    text
}

/// Distill `name` as `trigger`: the extractor's proposals, or the
/// distiller's when it answers validly, stored as proposed entries that do
/// not exist yet, and recorded on the branch.
pub(crate) fn distill(
    yard: &Yard,
    name: &str,
    distiller: Option<&Arc<dyn Judge>>,
    trigger: &str,
) -> Result<Distilled, Error> {
    let store = yard.store();
    let record = store.read(name)?;
    let events = crate::record::read(&store, name)?;
    let extracted = extract(&events);
    let mut by = JudgedBy::Deterministic;
    let mut proposals = extracted.clone();
    if let Some(distiller) = distiller {
        let diffstat = match &record.info.candidate {
            Some(c) => {
                let files = changed_files(yard, &record);
                format!(
                    "{} file(s), +{} -{}: {}",
                    c.files_changed,
                    c.insertions,
                    c.deletions,
                    files.join(", ")
                )
            }
            None => "nothing".into(),
        };
        let adopted = list(yard, Some(KnowledgeStatus::Adopted))?;
        let prompt = distiller_prompt(
            &record.info.prompt,
            &extracted,
            &diffstat,
            &crate::delegation::last_message(&events),
            &adopted,
        );
        let answer = distiller
            .verdict(yard, &prompt, &[format!("distill {name}")])
            .map_err(|e| e.to_string())
            .and_then(|text| parse_distilled(&text));
        let via = format!(
            "harness {}",
            distiller.name().trim_start_matches("harness ")
        );
        match answer {
            Ok(entries) => {
                by = JudgedBy::Judge {
                    name: distiller.name(),
                };
                proposals = entries
                    .into_iter()
                    .map(|(text, scope, why)| Proposal {
                        text,
                        scope,
                        turn: None,
                        via: via.clone(),
                        note: Some(why),
                    })
                    .collect();
            }
            Err(error) => {
                by = JudgedBy::Fallback {
                    name: distiller.name(),
                    error,
                };
            }
        }
    }
    let existing = store.knowledge().knowledge_entries()?;
    let mut stored: Vec<KnowledgeEntry> = Vec::new();
    let mut duplicates = 0u32;
    for proposal in proposals {
        let known = existing
            .iter()
            .chain(stored.iter())
            .any(|e| same(e, &proposal.text, &proposal.scope));
        if known {
            duplicates += 1;
            continue;
        }
        let entry = store.knowledge().add_knowledge(&KnowledgeEntry {
            id: 0,
            scope: proposal.scope,
            text: proposal.text,
            source: KnowledgeSource::Branch {
                branch: name.to_owned(),
                turn: proposal.turn,
                via: proposal.via,
            },
            status: KnowledgeStatus::Proposed,
            created_ms: now_ms(),
            adopted_by: None,
            decided_ms: None,
            note: proposal.note,
        })?;
        stored.push(entry);
    }
    let fallback = match &by {
        JudgedBy::Fallback { error, .. } => Some(error.clone()),
        _ => None,
    };
    let activity = KnowledgeActivity::Distilled {
        ids: stored.iter().map(|e| e.id).collect(),
        duplicates,
        by: by.describe(),
        fallback,
        trigger: trigger.to_owned(),
    };
    // A branch's end that taught nothing leaves its log as it was; one that
    // was asked for, or proposed or recognized something, says so.
    let quiet = trigger != "asked"
        && stored.is_empty()
        && duplicates == 0
        && matches!(by, JudgedBy::Deterministic);
    if !quiet {
        Recorder::open(&store, name, None)?.record(Activity::Knowledge(Box::new(activity)))?;
    }
    Ok(Distilled {
        branch: name.to_owned(),
        proposed: stored,
        duplicates,
        by,
        trigger: trigger.to_owned(),
    })
}

/// Distill `name` when the yard's settings ask for it on `trigger`:
/// top-level branches only, never a judge's scratch branch. Best-effort:
/// it never changes what happened to the branch.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard
pub(crate) fn on_end(yard: &Yard, name: &str, trigger: DistillTrigger) {
    let settings = yard.knowledge_settings();
    if !settings.distill_on.contains(&trigger) {
        return;
    }
    let store = yard.store();
    let Ok(record) = store.read(name) else {
        return;
    };
    if record.info.depth != 0 {
        return;
    }
    let Ok(events) = crate::record::read(&store, name) else {
        return;
    };
    if is_scratch(&events) {
        return;
    }
    let _ = distill(yard, name, settings.distiller.as_ref(), trigger.as_str());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        id: u64,
        text: &str,
        scope: KnowledgeScope,
        status: KnowledgeStatus,
    ) -> KnowledgeEntry {
        KnowledgeEntry {
            id,
            scope,
            text: text.into(),
            source: KnowledgeSource::Person { name: "ana".into() },
            status,
            created_ms: 0,
            adopted_by: None,
            decided_ms: None,
            note: None,
        }
    }

    fn scope(path: Option<&str>, kind: Option<TaskKind>) -> KnowledgeScope {
        KnowledgeScope {
            path: path.map(str::to_owned),
            kind,
        }
    }

    #[test]
    fn scopes_match_kind_and_paths_and_order_by_specificity() {
        let entries = vec![
            entry(1, "repo wide", scope(None, None), KnowledgeStatus::Adopted),
            entry(
                2,
                "bugfix only",
                scope(None, Some(TaskKind::Bugfix)),
                KnowledgeStatus::Adopted,
            ),
            entry(
                3,
                "parser files",
                scope(Some("src/parser/**"), None),
                KnowledgeStatus::Adopted,
            ),
            entry(
                4,
                "parser bugfix",
                scope(Some("src/parser/*.rs"), Some(TaskKind::Bugfix)),
                KnowledgeStatus::Adopted,
            ),
            entry(
                5,
                "not adopted",
                scope(None, None),
                KnowledgeStatus::Proposed,
            ),
            entry(
                6,
                "docs only",
                scope(None, Some(TaskKind::Docs)),
                KnowledgeStatus::Adopted,
            ),
        ];
        let files = vec!["src/parser/lex.rs".to_owned()];
        let b = brief(&entries, Some(TaskKind::Bugfix), &files, 10_000);
        assert_eq!(b.ids, [4, 3, 2, 1]);
        let text = b.text.unwrap();
        assert!(text.contains("[k4] (path src/parser/*.rs kind bugfix) parser bugfix"));
        assert!(text.contains("- [k1] repo wide"));
        // Path scopes need a known file.
        let b = brief(&entries, Some(TaskKind::Bugfix), &[], 10_000);
        assert_eq!(b.ids, [2, 1]);
        // The budget keeps the most specific that fit and skips the rest.
        let b = brief(&entries, Some(TaskKind::Bugfix), &files, 50);
        assert!(!b.ids.is_empty() && !b.omitted.is_empty());
        assert_eq!(b.ids[0], 4);
        let none = brief(&entries, Some(TaskKind::Feature), &[], 0);
        assert!(none.ids.is_empty() && none.text.is_none());
        assert!(scope(Some("["), None).check().is_err());
    }

    fn prompt(text: &str) -> RecordedEvent {
        RecordedEvent {
            at_ms: 0,
            activity: Activity::Prompt(text.into()),
        }
    }

    #[test]
    fn the_extractor_finds_corrections_reviews_and_pull_request_feedback() {
        let review = "File: src/a.rs\nLine: 3\nUser comment: \"Use the \\\"Error\\\" type \
                      from errors.rs here\"\n\nFile: b.rs\nScope: file\nUser comment: \"ok\"";
        let feedback = "Feedback on pull request #4 (https://x/4) for this branch:\n\n\
                        1. CI check \"test\" failed on abc: https://ci\n\n\
                        2. Review comment by @ana on src/lib.rs:10:\n> Never unwrap in library code\n\n\
                        Address it on this branch. When this turn ends, the branch is pushed \
                        to the pull request again.\n";
        let events = vec![
            prompt("Fix the parser"),
            prompt("Run cargo fmt before you finish, always"),
            prompt("continue"),
            prompt("[branchyard plan approved] carry it out"),
            prompt(review),
            prompt(feedback),
            prompt("<branchyard-inbox>\n[#1] answer from p: x\n</branchyard-inbox>\n\nKeep tests next to the code they test"),
            RecordedEvent {
                at_ms: 0,
                activity: Activity::Steered {
                    id: 1,
                    by: "by send".into(),
                    text: "Prefer small commits over one big one".into(),
                },
            },
        ];
        let found = extract(&events);
        let texts: Vec<(&str, Option<&str>, &str, Option<u32>)> = found
            .iter()
            .map(|p| {
                (
                    p.text.as_str(),
                    p.scope.path.as_deref(),
                    p.via.as_str(),
                    p.turn,
                )
            })
            .collect();
        assert_eq!(
            texts,
            [
                (
                    "Run cargo fmt before you finish, always",
                    None,
                    "send",
                    Some(2)
                ),
                (
                    "Use the \"Error\" type from errors.rs here",
                    Some("src/a.rs"),
                    "review",
                    Some(5)
                ),
                (
                    "Never unwrap in library code",
                    Some("src/lib.rs"),
                    "pull_request",
                    Some(6)
                ),
                (
                    "Keep tests next to the code they test",
                    None,
                    "send",
                    Some(7)
                ),
                (
                    "Prefer small commits over one big one",
                    None,
                    "steer",
                    Some(7)
                ),
            ]
        );
    }

    #[test]
    fn distiller_answers_are_parsed_strictly() {
        let good = r#"{"entries": [{"text": "Run cargo fmt", "path": null, "kind": "bugfix", "why": "asked twice"}]}"#;
        let parsed = parse_distilled(good).unwrap();
        assert_eq!(parsed[0].1.kind, Some(TaskKind::Bugfix));
        assert!(parse_distilled(&format!("```json\n{good}\n```")).is_ok());
        assert!(parse_distilled(r#"{"entries": []}"#).unwrap().is_empty());
        for bad in [
            "Sure! Here you go",
            r#"{"entries": [], "extra": 1}"#,
            r#"{"entries": [{"text": "x", "path": null, "kind": "chore", "why": "y"}]}"#,
            r#"{"entries": [{"text": "", "path": null, "kind": null, "why": "y"}]}"#,
            r#"{"entries": [{"text": "x", "path": "[", "kind": null, "why": "y"}]}"#,
            r#"{"entries": [{"text": "x", "kind": null, "why": "y"}]}"#,
        ] {
            assert!(parse_distilled(bad).is_err(), "{bad}");
        }
        let many = format!(
            r#"{{"entries": [{}]}}"#,
            [r#"{"text": "x", "path": null, "kind": null, "why": "y"}"#; 6].join(",")
        );
        assert!(parse_distilled(&many).is_err());
    }

    #[test]
    fn export_groups_adopted_entries_by_scope() {
        let entries = vec![
            entry(
                2,
                "parser rule",
                scope(Some("src/**"), None),
                KnowledgeStatus::Adopted,
            ),
            entry(
                1,
                "general rule",
                scope(None, None),
                KnowledgeStatus::Adopted,
            ),
            entry(3, "proposed", scope(None, None), KnowledgeStatus::Proposed),
        ];
        let text = export(&entries);
        let general = text.find("general rule (k1)").unwrap();
        let parser = text.find("parser rule (k2)").unwrap();
        assert!(general < parser);
        assert!(text.contains("## Files matching `src/**`"));
        assert!(!text.contains("proposed"));
    }
}
