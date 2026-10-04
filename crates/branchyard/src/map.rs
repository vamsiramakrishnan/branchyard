//! A wide map: one prompt over every item of a list, each item on its own
//! branch, each answer checked against a JSON schema, the answers
//! collected into a table, and optionally one reduce turn over them all.
//! See `docs/map.md`.
//!
//! A map is recorded in `.branchyard/maps/<name>/`: `map.json` (the spec,
//! with its items), `results.jsonl` (one row per finished item, appended
//! and synced as each finishes; the last row for an id wins) and
//! `reduce.json`. A process running the map holds the directory's lock,
//! so two never run one map at once, and a summary can say a map is
//! running. Running a map again skips the items whose last row is `ok`.
//!
//! Each attempt at an item is an ordinary branch, started as `by run`
//! starts one (routed through the fleet with its failover when asked), so
//! everything Branchyard records about branches applies. The answer is
//! read from the attempt's last reply, as a judge's verdict is: the whole
//! reply as JSON, or its last fenced block. An answer that fails the
//! schema gets one follow-up turn on the same branch naming the errors;
//! still invalid, or the branch not ending ready, the attempt failed, and
//! a retry starts a new branch.

use branchyard_support::LockExt as _;
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::json_schema::JsonSchema;
use crate::map_input::{self, MapItem, TemplateContext};
use crate::state::now_ms;
use crate::{Branch, BranchStatus, Error, Fleet, RouteOptions, TaskKind, TaskOptions, Yard};

/// Branches running at once when none is said.
pub const DEFAULT_CONCURRENCY: u32 = 4;
/// Most branches a map runs at once.
pub const MAX_CONCURRENCY: u32 = 64;
/// Retries of a failed item when none is said: one new branch.
pub const DEFAULT_RETRIES: u32 = 1;
/// The first line of a follow-up turn for an invalid answer.
pub const FOLLOW_UP_HEADER: &str = "[branchyard map: answer invalid]";
/// How much of the results a reduce prompt quotes.
const REDUCE_RESULTS_MAX: usize = 100_000;

/// A map: what to run over which items, and how. Stored as `map.json`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapSpec {
    /// The map's name: a branch name, which every branch's name starts
    /// with (`<name>-<item id>`).
    pub name: String,
    /// The prompt template; see [`map_input::render`] for placeholders.
    pub prompt: String,
    /// The JSON Schema (a subset; see `json_schema`) every answer must
    /// match. Without one, an item's result is its last reply's text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    pub items: Vec<MapItem>,
    /// Branches running at once.
    pub concurrency: u32,
    /// New branches an item gets after its first fails.
    pub retries: u32,
    /// Stop starting items once the map's recorded cost, across runs,
    /// reaches this. Items not started stay pending for a later run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_usd: Option<f64>,
    /// A prompt for one final turn given every result, whose reply is the
    /// map's summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduce: Option<String>,
    /// Remove an item's branches once its answer is recorded `ok` (and the
    /// reduce branch once it answered). Failed items' branches are kept.
    #[serde(default)]
    pub remove_done: bool,
    /// What the caller needs to run the map again (`by map resume`): the
    /// command line, or a server's request. Not read here.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub launch: Value,
    #[serde(default)]
    pub created_ms: u64,
}

impl MapSpec {
    pub fn new(name: impl Into<String>, prompt: impl Into<String>, items: Vec<MapItem>) -> MapSpec {
        MapSpec {
            name: name.into(),
            prompt: prompt.into(),
            schema: None,
            items,
            concurrency: DEFAULT_CONCURRENCY,
            retries: DEFAULT_RETRIES,
            total_usd: None,
            reduce: None,
            remove_done: false,
            launch: Value::Null,
            created_ms: 0,
        }
    }
}

/// How an item ended.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MapStatus {
    /// A branch ended ready (or with no changes) with a valid answer.
    Ok,
    /// Every attempt failed.
    Failed,
}

impl MapStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MapStatus::Ok => "ok",
            MapStatus::Failed => "failed",
        }
    }
}

/// One finished item, as `results.jsonl` records it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapRow {
    pub id: String,
    pub status: MapStatus,
    /// The answer: JSON matching the schema, or the reply's text without
    /// one. Only when `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Why the last attempt failed. Only when `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The branch that answered, or the last one tried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Every branch this run of the item used, failovers included, in
    /// order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    /// Branches started for the item (failovers not counted).
    pub attempts: u32,
    /// The branches' reported cost, when any reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub at_ms: u64,
}

/// The reduce turn's outcome, as `reduce.json` records it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapReduce {
    pub status: MapStatus,
    /// The reply: the map's summary. Only when `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// The results it was given, hashed: a rerun with the same results
    /// does not reduce again.
    pub digest: String,
    pub at_ms: u64,
}

/// A map's state: its items' rows and progress.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapReport {
    pub name: String,
    pub prompt: String,
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    /// Items with no row yet: not started, running, or interrupted.
    pub pending: usize,
    /// A process is running the map now.
    pub running: bool,
    /// Every recorded row's cost, across runs, and the reduce's.
    pub spent_usd: f64,
    /// The result's fields, for a table: the schema's top-level
    /// properties, else `result`.
    pub columns: Vec<String>,
    /// The latest row of each item that has one, in the items' order.
    pub rows: Vec<MapRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduce: Option<MapReduce>,
    /// Why a run left items pending (the total budget), when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
}

/// One line per map, for `by map ls` and `by ls`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapSummary {
    pub name: String,
    pub prompt: String,
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    pub pending: usize,
    pub running: bool,
    pub spent_usd: f64,
    pub created_ms: u64,
    /// When its last row was written.
    pub updated_ms: u64,
}

impl MapSummary {
    /// `3 of 10 done, 1 failed, running`.
    pub fn progress(&self) -> String {
        let mut text = format!("{} of {} done", self.done, self.total);
        if self.failed > 0 {
            text.push_str(&format!(", {} failed", self.failed));
        }
        if self.running {
            text.push_str(", running");
        } else if self.pending > 0 {
            text.push_str(&format!(", {} pending", self.pending));
        }
        text
    }
}

/// What happened, as each item ends.
#[derive(Clone, Debug)]
pub struct MapProgress {
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    pub row: MapRow,
}

/// What [`MapOptions::progress`] calls.
pub type ProgressFn = Arc<dyn Fn(&MapProgress) + Send + Sync>;

/// How a map's branches run; not stored.
#[derive(Clone, Default)]
pub struct MapOptions {
    /// Every attempt's options: harness, limits (per branch), policy,
    /// check, provider, provisioning, observer. Its `name` is ignored;
    /// `plan` is refused.
    pub task: TaskOptions,
    /// Route each attempt through this table, failing over as
    /// [`Yard::run_routed`] does; the seed is varied per item.
    pub fleet: Option<Fleet>,
    pub route: RouteOptions,
    /// Record this kind on every branch when not routed.
    pub kind: Option<TaskKind>,
    /// Run the items whose last row is `failed` again.
    pub retry_failed: bool,
    /// Called as each item ends, from the thread that ran it.
    pub progress: Option<ProgressFn>,
}

/// Where maps are kept.
pub(crate) fn maps_dir(root: &Path) -> PathBuf {
    crate::state::dir(root).join("maps")
}

fn spec_path(dir: &Path) -> PathBuf {
    dir.join("map.json")
}

fn rows_path(dir: &Path) -> PathBuf {
    dir.join("results.jsonl")
}

fn reduce_path(dir: &Path) -> PathBuf {
    dir.join("reduce.json")
}

/// Write `text` to `path` through a temporary file and a rename.
fn write_atomic(path: &Path, text: &str) -> Result<(), Error> {
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = fs::File::create(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_data()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn read_spec(dir: &Path) -> Result<Option<MapSpec>, Error> {
    match fs::read_to_string(spec_path(dir)) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| Error::State(format!("{} is not a map: {e}", spec_path(dir).display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Every row recorded, in order. A last line cut short by a crash is
/// skipped.
fn read_rows(dir: &Path) -> Result<Vec<MapRow>, Error> {
    let text = match fs::read_to_string(rows_path(dir)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

fn read_reduce(dir: &Path) -> Option<MapReduce> {
    serde_json::from_str(&fs::read_to_string(reduce_path(dir)).ok()?).ok()
}

/// The result's fields for a table.
fn columns(spec: &MapSpec) -> Vec<String> {
    let properties = spec
        .schema
        .as_ref()
        .filter(|s| s.get("type").and_then(Value::as_str) == Some("object"))
        .and_then(|s| JsonSchema::new(s.clone()).ok())
        .map(|s| s.properties())
        .unwrap_or_default();
    match properties.is_empty() {
        true => vec!["result".to_owned()],
        false => properties,
    }
}

fn report_of(
    spec: &MapSpec,
    all_rows: &[MapRow],
    reduce: Option<MapReduce>,
    running: bool,
) -> MapReport {
    // A reduce recorded before the map stopped asking for one is stale,
    // though what it cost was spent.
    let reduce_cost = reduce.as_ref().and_then(|r| r.cost_usd).unwrap_or(0.0);
    let reduce = reduce.filter(|_| spec.reduce.is_some());
    let mut latest: BTreeMap<&str, &MapRow> = BTreeMap::new();
    for row in all_rows {
        latest.insert(row.id.as_str(), row);
    }
    let rows: Vec<MapRow> = spec
        .items
        .iter()
        .filter_map(|item| latest.get(item.id.as_str()).map(|r| (*r).clone()))
        .collect();
    let done = rows.iter().filter(|r| r.status == MapStatus::Ok).count();
    let failed = rows.len() - done;
    let spent_usd = all_rows.iter().filter_map(|r| r.cost_usd).sum::<f64>() + reduce_cost;
    MapReport {
        name: spec.name.clone(),
        prompt: spec.prompt.clone(),
        total: spec.items.len(),
        done,
        failed,
        pending: spec.items.len() - rows.len(),
        running,
        spent_usd,
        columns: columns(spec),
        rows,
        reduce,
        stopped: None,
    }
}

fn unknown_map(name: &str) -> Error {
    Error::Unsupported(format!(
        "there is no map named {name:?}; `by map ls` lists them"
    ))
}

/// `name`'s spec.
pub(crate) fn spec(yard: &Yard, name: &str) -> Result<MapSpec, Error> {
    let dir = maps_dir(yard.root()).join(name);
    read_spec(&dir)?.ok_or_else(|| unknown_map(name))
}

/// `name`'s report.
pub(crate) fn report(yard: &Yard, name: &str) -> Result<MapReport, Error> {
    let dir = maps_dir(yard.root()).join(name);
    let spec = read_spec(&dir)?.ok_or_else(|| unknown_map(name))?;
    let rows = read_rows(&dir)?;
    Ok(report_of(
        &spec,
        &rows,
        read_reduce(&dir),
        crate::lock::is_held(&dir),
    ))
}

/// Every map, oldest first.
pub(crate) fn list(yard: &Yard) -> Result<Vec<MapSummary>, Error> {
    let root = maps_dir(yard.root());
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(Some(spec)) = read_spec(&dir) else {
            continue;
        };
        let rows = read_rows(&dir)?;
        let report = report_of(&spec, &rows, read_reduce(&dir), crate::lock::is_held(&dir));
        let updated_ms = fs::metadata(rows_path(&dir))
            .or_else(|_| fs::metadata(spec_path(&dir)))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(spec.created_ms, |d| d.as_millis() as u64);
        out.push(MapSummary {
            name: spec.name,
            prompt: spec.prompt,
            total: report.total,
            done: report.done,
            failed: report.failed,
            pending: report.pending,
            running: report.running,
            spent_usd: report.spent_usd,
            created_ms: spec.created_ms,
            updated_ms,
        });
    }
    out.sort_by(|a, b| (a.created_ms, &a.name).cmp(&(b.created_ms, &b.name)));
    Ok(out)
}

/// Forget `name`'s record. Its branches stay.
pub(crate) fn remove(yard: &Yard, name: &str) -> Result<(), Error> {
    let dir = maps_dir(yard.root()).join(name);
    if read_spec(&dir)?.is_none() {
        return Err(unknown_map(name));
    }
    let _lock = crate::DirLock::acquire(&dir, &format!("by map rm {name}"))?;
    fs::remove_dir_all(&dir)?;
    Ok(())
}

/// The prompt an item's branch gets: the template rendered, then how to
/// answer.
pub fn item_prompt(rendered: &str, schema: Option<&Value>) -> String {
    let Some(schema) = schema else {
        return rendered.to_owned();
    };
    format!(
        "{}\n\n## Answer\nWhen you are done, give your answer as one JSON value matching this \
         JSON Schema, alone in a ```json fenced block at the end of your reply:\n```json\n{}\n```\n",
        rendered.trim_end(),
        serde_json::to_string_pretty(schema).unwrap_or_default()
    )
}

/// The follow-up turn for an invalid answer: the errors, then the task
/// again, so a harness that lost its context still has it.
pub fn follow_up_prompt(errors: &[String], prompt: &str) -> String {
    let list: Vec<String> = errors.iter().map(|e| format!("- {e}")).collect();
    format!(
        "{FOLLOW_UP_HEADER}\nYour last reply is not a valid answer:\n{}\n\nAnswer again, with \
         the JSON alone in a ```json fenced block at the end of your reply. The task, \
         again:\n\n{prompt}",
        list.join("\n")
    )
}

/// The reduce turn's prompt: the reduce text, then the results, bounded.
pub fn reduce_prompt(reduce: &str, map: &str, rows: &[MapRow]) -> String {
    let mut lines = String::new();
    let mut shown = 0;
    for row in rows {
        let mut line = serde_json::Map::new();
        line.insert("id".into(), Value::String(row.id.clone()));
        line.insert("status".into(), Value::String(row.status.as_str().into()));
        if let Some(result) = &row.result {
            line.insert("result".into(), result.clone());
        }
        if let Some(error) = &row.error {
            line.insert("error".into(), Value::String(error.clone()));
        }
        let line = Value::Object(line).to_string();
        if lines.len() + line.len() > REDUCE_RESULTS_MAX {
            break;
        }
        lines.push_str(&line);
        lines.push('\n');
        shown += 1;
    }
    let mut text = format!(
        "{}\n\n## Results\nThe results of the map {map}, one JSON object per item (id, status, \
         and result or error):\n```jsonl\n{lines}```\n",
        reduce.trim_end()
    );
    if shown < rows.len() {
        text.push_str(&format!(
            "({} more result(s) are not shown, to keep this prompt short.)\n",
            rows.len() - shown
        ));
    }
    text
}

/// A branch's status in words, for an error.
fn status_text(status: &BranchStatus) -> String {
    match status {
        BranchStatus::Failed { reason } => format!("the branch failed: {reason}"),
        BranchStatus::BudgetExceeded { limit } => {
            format!("the branch stopped at its {limit} limit")
        }
        BranchStatus::Blocked { reason } => format!("the branch is blocked: {reason}"),
        other => format!(
            "the branch ended {}",
            serde_json::to_value(other)
                .ok()
                .and_then(|v| v["state"].as_str().map(str::to_owned))
                .unwrap_or_default()
        ),
    }
}

fn settled(info: &crate::BranchInfo) -> Result<(), String> {
    match info.status {
        BranchStatus::Ready | BranchStatus::NoChanges => Ok(()),
        ref other => Err(status_text(other)),
    }
}

/// What a running map shares between its workers.
struct Run<'a> {
    yard: &'a Yard,
    spec: &'a MapSpec,
    schema: Option<JsonSchema>,
    options: &'a MapOptions,
    rows: Mutex<fs::File>,
    /// Cost recorded so far, across runs.
    spent: Mutex<f64>,
    counts: Mutex<(usize, usize)>,
    stopped: AtomicBool,
    /// The task whose attempts the items are.
    task_id: String,
}

/// One attempt's outcome: the branches it used, their cost, and the
/// answer or why there is none.
struct Attempt {
    branches: Vec<String>,
    cost: Option<f64>,
    answer: Result<Value, String>,
}

fn add_cost(total: &mut Option<f64>, cost: Option<f64>) {
    if let Some(cost) = cost {
        *total = Some(total.unwrap_or(0.0) + cost);
    }
}

impl Run<'_> {
    /// What is left of the total budget; `None` without one.
    fn remaining(&self) -> Option<f64> {
        let spent = *self.spent.lock_recovering("spent");
        self.spec.total_usd.map(|total| total - spent)
    }

    /// Start one branch for `prompt` under `base`'s first free name.
    fn start(
        &self,
        base: &str,
        prompt: &str,
        seed_offset: u64,
    ) -> Result<(Branch, Vec<Branch>), Error> {
        let mut task = self.options.task.clone();
        task.plan = false;
        task.join_task = Some(self.task_id.clone());
        if let Some(left) = self.remaining() {
            task.budget.max_usd = Some(task.budget.max_usd.map_or(left, |m| m.min(left)));
        }
        for n in 1..=1000 {
            task.name = Some(match n {
                1 => base.to_owned(),
                n => format!("{base}-{n}"),
            });
            let started = match &self.options.fleet {
                Some(fleet) => {
                    let mut how = self.options.route.clone();
                    how.seed = how.seed.map(|s| s.wrapping_add(seed_offset));
                    self.yard
                        .run_routed(prompt, &task, fleet, &how)
                        .map(|mut routed| {
                            let last = routed.branches.remove(0);
                            let earlier: Vec<Branch> = routed
                                .failovers
                                .iter()
                                .filter_map(|(from, _)| self.yard.branch(from).ok())
                                .collect();
                            (last, earlier)
                        })
                }
                None => match self.options.kind {
                    Some(kind) => self.yard.run_with_kind(prompt, &task, kind),
                    None => self.yard.task(prompt).options(task.clone()).run(),
                }
                .map(|b| (b, Vec::new())),
            };
            match started {
                Err(Error::BranchExists(_)) => continue,
                other => return other,
            }
        }
        Err(Error::State(format!("no free branch name for {base}")))
    }

    /// The answer in `branch`'s last reply, checked.
    fn answer(&self, branch: &Branch) -> Result<Value, Vec<String>> {
        settled(branch.info()).map_err(|e| vec![e])?;
        let reply = crate::judge::message(&branch.events().map_err(|e| vec![e.to_string()])?);
        let Some(schema) = &self.schema else {
            return Ok(Value::String(reply.trim().to_owned()));
        };
        let value = map_input::parse_answer(&reply).map_err(|e| vec![e])?;
        let errors = schema.validate(&value);
        match errors.is_empty() {
            true => Ok(value),
            false => Err(errors),
        }
    }

    fn attempt(&self, base: &str, prompt: &str, seed_offset: u64) -> Attempt {
        let (branch, earlier) = match self.start(base, prompt, seed_offset) {
            Ok(started) => started,
            Err(error) => {
                return Attempt {
                    branches: Vec::new(),
                    cost: None,
                    answer: Err(format!("the branch could not start: {error}")),
                }
            }
        };
        let mut cost = None;
        let mut branches = Vec::new();
        for b in &earlier {
            add_cost(&mut cost, b.info().cost_usd);
            branches.push(b.info().name.clone());
        }
        branches.push(branch.info().name.clone());
        // Children it delegated to end before its answer is read.
        let _ = branch.wait_subtree();
        let mut current = branch;
        let first = self.answer(&current);
        let mut answer = first.clone().map_err(|errors| errors.join("; "));
        // One follow-up for an answer that is there but invalid: a branch
        // that did not end ready has nothing to correct.
        if let Err(errors) = first {
            if self.schema.is_some() && settled(current.info()).is_ok() {
                let follow_up = follow_up_prompt(&errors, prompt);
                let options = crate::goal::send_options(&self.options.task);
                answer =
                    match crate::run::send(self.yard, &current.info().name, &follow_up, &options) {
                        Ok(next) => {
                            let _ = next.wait_subtree();
                            current = next;
                            self.answer(&current).map_err(|errors| {
                                format!("after one follow-up turn: {}", errors.join("; "))
                            })
                        }
                        Err(error) => Err(format!("the follow-up turn could not run: {error}")),
                    };
            }
        }
        add_cost(&mut cost, current.info().cost_usd);
        Attempt {
            branches,
            cost,
            answer,
        }
    }

    /// Run `item` to a row; `None` when the total budget stopped it before
    /// its first attempt.
    fn item(&self, index: usize, item: &MapItem) -> Result<Option<MapRow>, Error> {
        let rendered = map_input::render(
            &self.spec.prompt,
            item,
            &TemplateContext {
                map: &self.spec.name,
                index: index + 1,
            },
        )
        .map_err(Error::Unsupported)?;
        let prompt = item_prompt(&rendered, self.spec.schema.as_ref());
        let base = format!("{}-{}", self.spec.name, crate::names::slug(&item.id));
        let mut row = MapRow {
            id: item.id.clone(),
            status: MapStatus::Failed,
            result: None,
            error: None,
            branch: None,
            branches: Vec::new(),
            attempts: 0,
            cost_usd: None,
            at_ms: 0,
        };
        for attempt in 0..=self.spec.retries {
            if self.remaining().is_some_and(|left| left <= 0.0) {
                self.stopped.store(true, Ordering::SeqCst);
                if attempt == 0 {
                    return Ok(None);
                }
                row.error = Some(format!(
                    "{}; then the map's total budget ran out",
                    row.error.unwrap_or_default()
                ));
                break;
            }
            row.attempts += 1;
            let outcome = self.attempt(&base, &prompt, (index as u64) << 8 | attempt as u64);
            add_cost(&mut row.cost_usd, outcome.cost);
            if let Some(cost) = outcome.cost {
                *self.spent.lock_recovering("spent") += cost;
            }
            row.branch = outcome.branches.last().cloned().or(row.branch);
            row.branches.extend(outcome.branches);
            match outcome.answer {
                Ok(value) => {
                    row.status = MapStatus::Ok;
                    row.result = Some(value);
                    row.error = None;
                    break;
                }
                Err(error) => row.error = Some(error),
            }
        }
        row.at_ms = now_ms();
        Ok(Some(row))
    }

    /// Append `row` and tell the caller.
    fn record(&self, row: MapRow) -> Result<(), Error> {
        {
            let mut file = self.rows.lock_recovering("rows");
            let line = serde_json::to_string(&row).map_err(|e| Error::State(e.to_string()))?;
            file.write_all(format!("{line}\n").as_bytes())?;
            file.sync_data()?;
        }
        let (done, failed) = {
            let mut counts = self.counts.lock_recovering("counts");
            match row.status {
                MapStatus::Ok => counts.0 += 1,
                MapStatus::Failed => counts.1 += 1,
            }
            *counts
        };
        if row.status == MapStatus::Ok && self.spec.remove_done {
            for name in &row.branches {
                let _ = self.yard.remove(name);
            }
        }
        if let Some(progress) = &self.options.progress {
            progress(&MapProgress {
                total: self.spec.items.len(),
                done,
                failed,
                row,
            });
        }
        Ok(())
    }

    fn reduce(&self, reduce: &str, dir: &Path, rows: &[MapRow]) -> Result<MapReduce, Error> {
        let prompt = reduce_prompt(reduce, &self.spec.name, rows);
        let digest = blake3::hash(prompt.as_bytes()).to_hex()[..16].to_owned();
        if let Some(done) =
            read_reduce(dir).filter(|r| r.digest == digest && r.status == MapStatus::Ok)
        {
            return Ok(done);
        }
        let base = format!("{}-reduce", self.spec.name);
        let mut outcome = MapReduce {
            status: MapStatus::Failed,
            text: None,
            error: None,
            branch: None,
            cost_usd: None,
            digest,
            at_ms: 0,
        };
        match self.start(&base, &prompt, u64::MAX >> 1) {
            Ok((branch, _)) => {
                let _ = branch.wait_subtree();
                outcome.branch = Some(branch.info().name.clone());
                outcome.cost_usd = branch.info().cost_usd;
                match settled(branch.info()) {
                    Ok(()) => {
                        let reply = crate::judge::message(&branch.events()?);
                        outcome.status = MapStatus::Ok;
                        outcome.text = Some(reply.trim().to_owned());
                        if self.spec.remove_done {
                            let _ = self.yard.remove(&branch.info().name);
                        }
                    }
                    Err(error) => outcome.error = Some(error),
                }
            }
            Err(error) => outcome.error = Some(format!("the branch could not start: {error}")),
        }
        outcome.at_ms = now_ms();
        let text =
            serde_json::to_string_pretty(&outcome).map_err(|e| Error::State(e.to_string()))?;
        write_atomic(&reduce_path(dir), &format!("{text}\n"))?;
        Ok(outcome)
    }
}

/// A map's name when none is given: a slug of its prompt, as a branch's.
pub fn default_name(prompt: &str) -> String {
    crate::names::slug(prompt)
}

/// Check `spec` as running it would before any branch runs: its name, its
/// prompt against every item, its schema, its limits.
pub fn check_spec(spec: &MapSpec) -> Result<(), Error> {
    check(spec, &MapOptions::default()).map(|_| ())
}

/// Check what can be checked before any branch runs.
fn check(spec: &MapSpec, options: &MapOptions) -> Result<Option<JsonSchema>, Error> {
    let bad = |what: String| Error::Unsupported(format!("map {}: {what}", spec.name));
    crate::names::validate(&spec.name)?;
    if spec.name.ends_with("-reduce") {
        return Err(bad("a map's name may not end in -reduce".into()));
    }
    map_input::check_template(&spec.prompt).map_err(bad)?;
    if !(1..=MAX_CONCURRENCY).contains(&spec.concurrency) {
        return Err(bad(format!(
            "concurrency must be 1 to {MAX_CONCURRENCY}, not {}",
            spec.concurrency
        )));
    }
    // Not more than 0, NaN included.
    if spec.total_usd.is_some_and(|t| t.is_nan() || t <= 0.0) {
        return Err(bad("the total budget must be more than 0".into()));
    }
    if options.task.plan {
        return Err(bad(
            "a map's branches cannot wait for plan approval; run it without --plan".into(),
        ));
    }
    if spec.reduce.as_ref().is_some_and(|r| r.trim().is_empty()) {
        return Err(bad("the reduce prompt is empty".into()));
    }
    let mut ids = std::collections::BTreeSet::new();
    for (index, item) in spec.items.iter().enumerate() {
        if !ids.insert(&item.id) {
            return Err(bad(format!("two items have the id {:?}", item.id)));
        }
        map_input::render(
            &spec.prompt,
            item,
            &TemplateContext {
                map: &spec.name,
                index: index + 1,
            },
        )
        .map_err(bad)?;
    }
    spec.schema
        .clone()
        .map(|s| JsonSchema::new(s).map_err(|e| bad(format!("the schema: {e}"))))
        .transpose()
}

/// Run `spec`: record it, run every item not yet `ok` (nor `failed`,
/// unless `retry_failed`), at most `concurrency` at once, then reduce.
pub(crate) fn run(
    yard: &Yard,
    mut spec: MapSpec,
    options: &MapOptions,
) -> Result<MapReport, Error> {
    let schema = check(&spec, options)?;
    let dir = maps_dir(yard.root()).join(&spec.name);
    let _lock =
        crate::DirLock::acquire(&dir, &format!("by map {}", spec.name)).map_err(|e| {
            match e.to_string().contains("already in use") {
                true => Error::State(format!("map {} is running in another process", spec.name)),
                false => e,
            }
        })?;
    match read_spec(&dir)? {
        Some(old) if old.prompt != spec.prompt || old.schema != spec.schema => {
            return Err(Error::Unsupported(format!(
                "a map named {} exists with a different prompt or schema; choose another name, \
                 or forget it with `by map rm {}`",
                spec.name, spec.name
            )))
        }
        Some(old) => spec.created_ms = old.created_ms,
        None => spec.created_ms = now_ms(),
    }
    let text = serde_json::to_string_pretty(&spec).map_err(|e| Error::State(e.to_string()))?;
    write_atomic(&spec_path(&dir), &format!("{text}\n"))?;

    let recorded = read_rows(&dir)?;
    let mut latest: BTreeMap<&str, MapStatus> = BTreeMap::new();
    for row in &recorded {
        latest.insert(row.id.as_str(), row.status);
    }
    let mut done = 0;
    let mut failed = 0;
    let mut queue = VecDeque::new();
    for (index, item) in spec.items.iter().enumerate() {
        match latest.get(item.id.as_str()) {
            Some(MapStatus::Ok) => done += 1,
            Some(MapStatus::Failed) if !options.retry_failed => failed += 1,
            _ => queue.push_back((index, item)),
        }
    }
    // One task for the map, its items the attempts; kept across resumes.
    let task_id = match fs::read_to_string(dir.join("task")) {
        Ok(id) if !id.trim().is_empty() => id.trim().to_owned(),
        _ => {
            let joining = crate::tasks::joining(yard, &spec.prompt, &options.task, "map")?;
            crate::tasks::save(&yard.root, &joining.task)?;
            write_atomic(&dir.join("task"), &format!("{}\n", joining.task.id))?;
            joining.task.id
        }
    };
    let rows_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(rows_path(&dir))?;
    let run = Run {
        yard,
        spec: &spec,
        schema,
        options,
        rows: Mutex::new(rows_file),
        spent: Mutex::new(
            recorded.iter().filter_map(|r| r.cost_usd).sum::<f64>()
                + read_reduce(&dir).and_then(|r| r.cost_usd).unwrap_or(0.0),
        ),
        counts: Mutex::new((done, failed)),
        stopped: AtomicBool::new(false),
        task_id,
    };
    let workers = (spec.concurrency as usize).min(queue.len());
    let queue = Mutex::new(queue);
    let results: Vec<Result<(), Error>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| -> Result<(), Error> {
                    loop {
                        if run.stopped.load(Ordering::SeqCst) {
                            return Ok(());
                        }
                        let next = queue.lock_recovering("queue").pop_front();
                        let Some((index, item)) = next else {
                            return Ok(());
                        };
                        if let Some(row) = run.item(index, item)? {
                            run.record(row)?;
                        }
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    });
    for result in results {
        result?;
    }
    let stopped = run.stopped.load(Ordering::SeqCst).then(|| {
        format!(
            "the map's total budget of ${:.2} was reached; run it again with a larger \
             --total-usd to start the rest",
            spec.total_usd.unwrap_or(0.0)
        )
    });
    let all_rows = read_rows(&dir)?;
    let mut report = report_of(&spec, &all_rows, read_reduce(&dir), false);
    if let (Some(reduce), None) = (&spec.reduce, &stopped) {
        if !report.rows.is_empty() {
            report.reduce = Some(run.reduce(reduce, &dir, &report.rows)?);
            report.spent_usd = all_rows.iter().filter_map(|r| r.cost_usd).sum::<f64>()
                + report
                    .reduce
                    .as_ref()
                    .and_then(|r| r.cost_usd)
                    .unwrap_or(0.0);
        }
    }
    report.stopped = stopped;
    Ok(report)
}

/// The result table as JSON lines: per item `id`, `status`, `result` or
/// `error`, `branch`, `attempts` and `cost_usd`.
pub fn rows_jsonl(rows: &[MapRow]) -> String {
    let mut out = String::new();
    for row in rows {
        let mut line = serde_json::Map::new();
        line.insert("id".into(), Value::String(row.id.clone()));
        line.insert("status".into(), Value::String(row.status.as_str().into()));
        if let Some(result) = &row.result {
            line.insert("result".into(), result.clone());
        }
        if let Some(error) = &row.error {
            line.insert("error".into(), Value::String(error.clone()));
        }
        line.insert(
            "branch".into(),
            row.branch.clone().map_or(Value::Null, Value::String),
        );
        line.insert("attempts".into(), Value::from(row.attempts));
        line.insert(
            "cost_usd".into(),
            row.cost_usd.map_or(Value::Null, Value::from),
        );
        out.push_str(&Value::Object(line).to_string());
        out.push('\n');
    }
    out
}

/// The result table as CSV: `id`, `status`, one column per result field
/// (`columns`; a single `result` column when the answer is not an object),
/// `error`, `branch`, `attempts`, `cost_usd`. A field that is not a
/// string is written as JSON.
pub fn rows_csv(rows: &[MapRow], columns: &[String]) -> String {
    let cell = |value: Option<&Value>| match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => map_input::csv_field(text),
        Some(other) => map_input::csv_field(&other.to_string()),
    };
    let mut header = vec!["id".to_owned(), "status".to_owned()];
    header.extend(columns.iter().cloned());
    header.extend(["error", "branch", "attempts", "cost_usd"].map(String::from));
    let mut out = header
        .iter()
        .map(|h| map_input::csv_field(h))
        .collect::<Vec<_>>()
        .join(",");
    out.push('\n');
    let whole = columns.len() == 1 && columns[0] == "result";
    for row in rows {
        let mut cells = vec![
            map_input::csv_field(&row.id),
            row.status.as_str().to_owned(),
        ];
        for column in columns {
            let value = match (&row.result, whole) {
                (Some(result), true) => Some(result),
                (Some(Value::Object(fields)), false) => fields.get(column),
                _ => None,
            };
            cells.push(cell(value));
        }
        cells.push(cell(
            row.error
                .as_ref()
                .map(|e| Value::String(e.clone()))
                .as_ref(),
        ));
        cells.push(
            row.branch
                .as_deref()
                .map(map_input::csv_field)
                .unwrap_or_default(),
        );
        cells.push(row.attempts.to_string());
        cells.push(row.cost_usd.map(|c| format!("{c:.4}")).unwrap_or_default());
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str, result: Option<Value>, error: Option<&str>) -> MapRow {
        MapRow {
            id: id.into(),
            status: match result {
                Some(_) => MapStatus::Ok,
                None => MapStatus::Failed,
            },
            result,
            error: error.map(str::to_owned),
            branch: Some(format!("m-{id}")),
            branches: vec![format!("m-{id}")],
            attempts: 1,
            cost_usd: Some(0.5),
            at_ms: 1,
        }
    }

    #[test]
    fn tables_hold_ids_statuses_fields_and_errors() {
        let rows = [
            row("a", Some(json!({"name": "x, y", "n": 2})), None),
            row("b", None, Some("bad \"json\"")),
        ];
        let csv = rows_csv(&rows, &["n".into(), "name".into()]);
        assert_eq!(
            csv,
            "id,status,n,name,error,branch,attempts,cost_usd\n\
             a,ok,2,\"x, y\",,m-a,1,0.5000\n\
             b,failed,,,\"bad \"\"json\"\"\",m-b,1,0.5000\n"
        );
        let jsonl = rows_jsonl(&rows);
        let lines: Vec<Value> = jsonl
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["result"]["n"], 2);
        assert_eq!(lines[1]["error"], "bad \"json\"");
        assert!(lines[1].get("result").is_none());
        let whole = rows_csv(
            &[row("t", Some(json!("plain text")), None)],
            &["result".into()],
        );
        assert!(whole.contains("t,ok,plain text,,m-t"), "{whole}");
    }

    #[test]
    fn prompts_say_how_to_answer_and_follow_ups_repeat_the_task() {
        let schema = json!({"type": "object"});
        let prompt = item_prompt("Look at x", Some(&schema));
        assert!(prompt.starts_with("Look at x\n\n## Answer\n"), "{prompt}");
        assert!(
            prompt.contains("```json\n{\n  \"type\": \"object\"\n}\n```"),
            "{prompt}"
        );
        assert_eq!(item_prompt("Look at x", None), "Look at x");
        let follow = follow_up_prompt(&["$.n: expected integer".into()], &prompt);
        assert!(follow.starts_with(FOLLOW_UP_HEADER));
        assert!(follow.contains("- $.n: expected integer"));
        assert!(follow.ends_with(&prompt));
        let reduce = reduce_prompt("Summarize", "m", &[row("a", Some(json!(1)), None)]);
        assert!(
            reduce.contains("{\"id\":\"a\",\"status\":\"ok\",\"result\":1}"),
            "{reduce}"
        );
    }

    #[test]
    fn progress_reads_plainly() {
        let summary = MapSummary {
            name: "m".into(),
            prompt: "p".into(),
            total: 10,
            done: 3,
            failed: 1,
            pending: 6,
            running: false,
            spent_usd: 0.0,
            created_ms: 0,
            updated_ms: 0,
        };
        assert_eq!(summary.progress(), "3 of 10 done, 1 failed, 6 pending");
        let running = MapSummary {
            running: true,
            ..summary
        };
        assert_eq!(running.progress(), "3 of 10 done, 1 failed, running");
    }
}
