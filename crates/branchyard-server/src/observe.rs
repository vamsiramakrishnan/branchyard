//! What an operation's run adds to metrics and traces.
//!
//! The dispatcher ([`crate::ops`]) counts admissions, claims, lease
//! renewals and expiries, and finished operations as they happen, and
//! opens the admission, claim and operation spans ([`crate::telemetry`]).
//! What happened inside the operation (turns, tool calls, connector calls,
//! tokens, cost) is the engine's and the harness's, recorded as the
//! branch's events; once the operation finishes, `record_events` reads the
//! events it recorded (from its feed position at admission to its end) and
//! turns them into counters, histograms, and spans with the events' own
//! timestamps, children of the operation's span. Each operation is run by
//! one worker, so this is counted once across servers sharing a database.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use branchyard::{Activity, BranchStatus, Event};
use branchyard_client::api::FeedEntry;
use branchyard_telemetry::spans::{self, Finished};
use opentelemetry::KeyValue;

use crate::metrics::{self, Metrics};
use crate::store::StoredOperation;
use crate::telemetry::BranchTraces;

/// The process's metrics, shared by the dispatcher, the executor, webhooks
/// and the `/metrics` route; and the traces its branches last ran in.
/// Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct Observability {
    /// The instruments.
    pub metrics: Arc<Metrics>,
    /// The latest trace of each branch, for webhook deliveries.
    pub branches: Arc<BranchTraces>,
}

impl Observability {
    /// Metrics on the process's meters (the ones `branchyard-telemetry`
    /// installed, when `init` ran), so `GET /metrics` and an OTLP push read
    /// the same instruments.
    pub fn from_env() -> Observability {
        Observability {
            metrics: Arc::new(Metrics::new(
                branchyard_telemetry::Meters::process_or_isolated(),
            )),
            branches: Arc::new(BranchTraces::default()),
        }
    }
}

/// A turn in progress in the events being read.
struct Turn {
    number: Option<u64>,
    start_ms: u64,
    saw_event: bool,
    tool: Option<Tool>,
    /// Spans inside the turn, written with the turn's span as their parent
    /// once it is known.
    children: Vec<Finished>,
}

struct Tool {
    name: String,
    call_id: String,
    start_ms: u64,
}

/// The tools whose names are a label of `branchyard_tool_call_duration_seconds`
/// (lower case); any other tool is `other`, and an MCP tool is `mcp`. The
/// list is closed so a harness cannot grow the series.
const KNOWN_TOOLS: &[&str] = &[
    "bash",
    "shell",
    "exec",
    "read",
    "write",
    "edit",
    "multiedit",
    "glob",
    "grep",
    "ls",
    "webfetch",
    "websearch",
    "task",
    "todowrite",
    "notebookedit",
    "apply_patch",
    "view",
    "search",
    "fetch",
    "browser",
];

/// The label a tool's name takes: itself if it is a known tool (lower
/// case), `mcp` for an MCP server's tool, `other` for anything else.
pub fn tool_label(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("mcp__") || lower.starts_with("mcp:") {
        return "mcp";
    }
    KNOWN_TOOLS
        .iter()
        .find(|known| **known == lower)
        .copied()
        .unwrap_or("other")
}

/// The `kind` (or `state`) tag of a serde-tagged value, such as a turn's
/// outcome.
fn tag(value: &impl serde::Serialize, key: &str) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.get(key).and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn is_error(outcome: &str) -> bool {
    !matches!(outcome, "completed" | "ready" | "no_changes")
}

/// Tokens (and cost) a harness reported, per branch, as the last
/// cumulative observation the scan has seen.
#[derive(Default)]
struct Seen {
    input: u64,
    output: u64,
    cost: f64,
}

/// Count and trace what `entries` (the feed between the operation's
/// admission and its end) say happened on `branches`, whose harness
/// `harness_of` names. Spans are children of `parent`, the operation's
/// `traceparent`, when the operation is traced.
pub fn record_events(
    observability: &Observability,
    branches: &BTreeSet<String>,
    harness_of: &dyn Fn(&str) -> String,
    entries: &[FeedEntry],
    parent: Option<&str>,
) {
    let metrics = &observability.metrics;
    let traced = parent.filter(|_| branchyard_telemetry::spans_enabled());
    let mut turns: BTreeMap<&str, Turn> = BTreeMap::new();
    let mut seen: BTreeMap<&str, Seen> = BTreeMap::new();
    let end_tool = |turn: &mut Turn, branch: &str, harness: &str, at: u64| {
        let Some(tool) = turn.tool.take() else { return };
        metrics.observe(
            metrics::TOOL_DURATION,
            &[("harness", harness), ("tool", tool_label(&tool.name))],
            at.saturating_sub(tool.start_ms) as f64 / 1000.0,
        );
        if traced.is_some() {
            turn.children.push(Finished {
                name: format!("tool {}", tool.name),
                parent: None,
                kind: spans::Kind::Internal,
                start_ms: tool.start_ms,
                end_ms: at,
                attributes: vec![
                    KeyValue::new("by.branch", branch.to_owned()),
                    KeyValue::new("by.tool", tool.name),
                    KeyValue::new("by.call_id", tool.call_id),
                ],
                error: None,
            });
        }
    };
    let write_turn = |turn: Turn, branch: &str, harness: &str, at: u64, outcome: &str| {
        let Some(parent) = traced else { return };
        let mut attributes = vec![
            KeyValue::new("by.branch", branch.to_owned()),
            KeyValue::new("by.harness", harness.to_owned()),
            KeyValue::new("by.outcome", outcome.to_owned()),
        ];
        if let Some(n) = turn.number {
            attributes.push(KeyValue::new(
                "by.turn",
                i64::try_from(n).unwrap_or(i64::MAX),
            ));
        }
        let written = spans::record(Finished {
            name: "turn".into(),
            parent: Some(parent.to_owned()),
            kind: spans::Kind::Internal,
            start_ms: turn.start_ms,
            end_ms: at,
            attributes,
            error: is_error(outcome).then(|| outcome.to_owned()),
        });
        for mut child in turn.children {
            child.parent = written.clone().or_else(|| Some(parent.to_owned()));
            spans::record(child);
        }
    };
    let end_turn = |turn: Turn, branch: &str, harness: &str, at: u64, outcome: &str| {
        let mut turn = turn;
        end_tool(&mut turn, branch, harness, at);
        metrics.inc(
            metrics::TURNS_ENDED,
            &[("harness", harness), ("outcome", outcome)],
        );
        let seconds = at.saturating_sub(turn.start_ms) as f64 / 1000.0;
        metrics.observe(metrics::TURN_DURATION, &[("harness", harness)], seconds);
        let mut genai = vec![
            ("gen_ai.operation.name", "invoke_agent"),
            ("gen_ai.agent.name", harness),
            ("by.turn.outcome", outcome),
        ];
        if is_error(outcome) {
            genai.push(("error.type", outcome));
        }
        metrics.observe(metrics::GENAI_OPERATION_DURATION, &genai, seconds);
        write_turn(turn, branch, harness, at, outcome);
    };
    for entry in entries {
        let branch = entry.branch.as_str();
        if !branches.contains(branch) {
            continue;
        }
        let harness = harness_of(branch);
        let at = entry.at_ms;
        // The harness says when a tool starts, not when it ends: a tool
        // call lasts until the branch's next event.
        if let Some(turn) = turns.get_mut(branch) {
            end_tool(turn, branch, &harness, at);
            if !turn.saw_event && matches!(entry.activity, Activity::Harness(_)) {
                turn.saw_event = true;
                metrics.observe(
                    metrics::TURN_FIRST_EVENT,
                    &[("harness", &harness)],
                    at.saturating_sub(turn.start_ms) as f64 / 1000.0,
                );
            }
        }
        match &entry.activity {
            Activity::Prompt(_) => {
                if let Some(open) = turns.remove(branch) {
                    end_turn(open, branch, &harness, at, "unknown");
                }
                metrics.inc(metrics::TURNS_STARTED, &[("harness", &harness)]);
                turns.insert(
                    branch,
                    Turn {
                        number: None,
                        start_ms: at,
                        saw_event: false,
                        tool: None,
                        children: Vec::new(),
                    },
                );
            }
            Activity::Harness(Event::TurnAccepted { turn, .. }) => {
                if let Some(open) = turns.get_mut(branch) {
                    open.number = Some(*turn);
                }
            }
            Activity::Harness(Event::ToolStarted {
                name,
                call_id,
                turn,
                ..
            }) => {
                metrics.inc(metrics::TOOL_CALLS, &[("harness", &harness)]);
                if let Some(open) = turns.get_mut(branch) {
                    open.number.get_or_insert(*turn);
                    open.tool = Some(Tool {
                        name: name.clone(),
                        call_id: call_id.clone(),
                        start_ms: at,
                    });
                }
            }
            Activity::Harness(Event::UsageObserved { usage, .. }) => {
                let previous = seen.entry(branch).or_default();
                let model = metrics.bounded("model", usage.model.as_deref().unwrap_or("unknown"));
                let (input, output, cost) = (
                    usage.input_tokens.unwrap_or(0),
                    usage.output_tokens.unwrap_or(0),
                    usage.cost_usd.unwrap_or(0.0),
                );
                // A running total counts what it grew by since the last
                // observation; the first one counts whole.
                let (input, output, cost) = if usage.cumulative {
                    let delta = (
                        input.saturating_sub(previous.input),
                        output.saturating_sub(previous.output),
                        (cost - previous.cost).max(0.0),
                    );
                    *previous = Seen {
                        input,
                        output,
                        cost,
                    };
                    delta
                } else {
                    (input, output, cost)
                };
                for (kind, count) in [("input", input), ("output", output)] {
                    if count > 0 {
                        metrics.observe(
                            metrics::GENAI_TOKEN_USAGE,
                            &[
                                ("gen_ai.operation.name", "invoke_agent"),
                                ("gen_ai.token.type", kind),
                                ("gen_ai.agent.name", &harness),
                                ("gen_ai.request.model", &model),
                            ],
                            count as f64,
                        );
                    }
                }
                if cost > 0.0 {
                    metrics.add(
                        metrics::USAGE_COST,
                        &[("harness", &harness), ("model", &model)],
                        cost,
                    );
                }
            }
            Activity::Harness(Event::TurnEnded { turn, outcome }) => {
                if let Some(mut open) = turns.remove(branch) {
                    open.number.get_or_insert(*turn);
                    end_turn(open, branch, &harness, at, &tag(outcome, "kind"));
                }
            }
            Activity::Harness(Event::OutcomeUnknown { .. }) => {
                if let Some(open) = turns.remove(branch) {
                    end_turn(open, branch, &harness, at, "unknown");
                }
            }
            // The engine ended the turn itself (a budget, a cancel, a
            // failure before the harness answered).
            Activity::Status(status)
                if !matches!(status, BranchStatus::Running | BranchStatus::Waiting) =>
            {
                if let Some(open) = turns.remove(branch) {
                    end_turn(open, branch, &harness, at, &tag(status, "state"));
                }
            }
            Activity::Model(activity) => {
                if let branchyard::models::ModelActivity::Call(call) = activity.as_ref() {
                    let model = metrics.bounded("model", call.model.as_str());
                    metrics.inc(
                        metrics::MODEL_CALLS,
                        &[("model", &model), ("decision", &call.decision)],
                    );
                    if let Some(tokens) = &call.tokens {
                        for (kind, count) in [
                            ("input", tokens.input),
                            ("output", tokens.output),
                            ("cache_read", tokens.cache_read),
                            ("cache_write", tokens.cache_write + tokens.cache_write_1h),
                        ] {
                            if count > 0 {
                                metrics.add(
                                    metrics::MODEL_TOKENS,
                                    &[("model", &model), ("kind", kind)],
                                    count as f64,
                                );
                            }
                        }
                    }
                    if let Some(cost) = call.cost_usd.filter(|c| *c > 0.0) {
                        metrics.add(metrics::MODEL_COST, &[("model", &model)], cost);
                    }
                }
            }
            Activity::ConnectorCall(call) => {
                let pairs = [
                    ("connector", call.connector.as_str()),
                    ("decision", call.decision.as_str()),
                ];
                metrics.inc(metrics::CONNECTOR_CALLS, &pairs);
                if let Some(latency) = call.latency_ms {
                    metrics.observe(metrics::CONNECTOR_DURATION, &pairs, latency as f64 / 1000.0);
                }
                if traced.is_some() {
                    let start = at.saturating_sub(call.latency_ms.unwrap_or(0));
                    let span = Finished {
                        name: format!("connector {}", call.operation),
                        parent: None,
                        kind: spans::Kind::Client,
                        start_ms: start,
                        end_ms: at,
                        attributes: vec![
                            KeyValue::new("by.branch", branch.to_owned()),
                            KeyValue::new("by.connector", call.connector.clone()),
                            KeyValue::new("by.operation", call.operation.clone()),
                            KeyValue::new("by.decision", call.decision.clone()),
                        ],
                        error: (call.decision != "allowed")
                            .then(|| call.reason.clone().unwrap_or(call.decision.clone())),
                    };
                    match turns.get_mut(branch) {
                        Some(turn) => turn.children.push(span),
                        None => {
                            let mut span = span;
                            span.parent = traced.map(str::to_owned);
                            spans::record(span);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // A turn still open when the operation finished (it should not be):
    // its span ends with the last event read, and it is not counted ended.
    if let Some(last) = entries.last() {
        for (branch, mut turn) in turns {
            let harness = harness_of(branch);
            end_tool(&mut turn, branch, &harness, last.at_ms);
            write_turn(turn, branch, &harness, last.at_ms, "unfinished");
        }
    }
}

/// Count the start of each of a task's new branches (`created`): from the
/// operation's admission (`admitted_ms`) to the branch's first prompt, by
/// whether its worktree came from a warm pool (`hit`), had a pool and
/// found no slot (`miss`), or had none (`none`), and by the `provider` and
/// `path` its turn recorded ([`branchyard::startup::Startup`]; a branch
/// with none is `unknown`); each phase that turn reached, by the time it
/// took; and the pool's hits and misses in `repo`.
pub fn record_starts(
    metrics: &Metrics,
    repo: &str,
    admitted_ms: u64,
    created: &BTreeSet<String>,
    entries: &[FeedEntry],
) {
    // A turn's startup is recorded with the harness's first event, after
    // its prompt: find each branch's first before counting.
    let mut startups: BTreeMap<&str, &branchyard::startup::Startup> = BTreeMap::new();
    for entry in entries {
        if let Activity::Startup(startup) = &entry.activity {
            if created.contains(entry.branch.as_str()) {
                startups.entry(entry.branch.as_str()).or_insert(startup);
            }
        }
    }
    let mut pooled: BTreeMap<&str, &'static str> = BTreeMap::new();
    let mut started: BTreeSet<&str> = BTreeSet::new();
    for entry in entries {
        let branch = entry.branch.as_str();
        if !created.contains(branch) || started.contains(branch) {
            continue;
        }
        match &entry.activity {
            Activity::Workspace(report) => {
                if let Some(used) = &report.pool {
                    let result = match used.slot {
                        Some(_) => "hit",
                        None => "miss",
                    };
                    if pooled.insert(branch, result).is_none() {
                        metrics.inc(metrics::POOL_CLAIMS, &[("repo", repo), ("result", result)]);
                    }
                }
            }
            Activity::Prompt(_) => {
                started.insert(branch);
                let pool = pooled.get(branch).copied().unwrap_or("none");
                let (provider, path) = match startups.get(branch) {
                    Some(startup) => (startup.provider.as_str(), startup.path.to_string()),
                    None => ("unknown", "unknown".to_owned()),
                };
                metrics.observe(
                    metrics::START,
                    &[("pool", pool), ("provider", provider), ("path", &path)],
                    entry.at_ms.saturating_sub(admitted_ms) as f64 / 1000.0,
                );
                if let Some(startup) = startups.get(branch) {
                    for (phase, ms) in startup.phases.durations() {
                        metrics.observe(
                            metrics::START_PHASE,
                            &[
                                ("provider", provider),
                                ("path", &path),
                                ("phase", &phase.to_string()),
                            ],
                            ms as f64 / 1000.0,
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

/// Count `stored`'s harness cost: the growth of each branch's recorded
/// `cost_usd` over the operation (`before` holds what the branches that
/// existed at its start had), and the branches it left stalled.
pub fn record_cost(
    metrics: &Metrics,
    stored: &StoredOperation,
    before: &BTreeMap<String, f64>,
    after: &[branchyard::BranchInfo],
) {
    for info in after {
        if info.stalled {
            metrics.inc(metrics::TURN_STALLS, &[("harness", &info.harness)]);
        }
        let Some(cost) = info.cost_usd else { continue };
        let grew = cost - before.get(&info.name).copied().unwrap_or(0.0);
        if grew > 0.0 {
            metrics.add(
                metrics::COST,
                &[("tenant", stored.tenant()), ("harness", &info.harness)],
                grew,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::TurnOutcome;
    use branchyard_telemetry::prometheus::Sample;
    use branchyard_telemetry::testing::{capture, CapturedSpan};

    fn entry(seq: u64, branch: &str, at_ms: u64, activity: Activity) -> FeedEntry {
        FeedEntry {
            seq,
            branch: branch.into(),
            at_ms,
            activity,
        }
    }

    /// A counter's or gauge's value, or a histogram's count.
    fn counter(metrics: &Metrics, name: &str, labels: &[(&str, &str)]) -> f64 {
        match metrics.sample(name, labels) {
            Some(Sample::Number(n)) => n,
            Some(Sample::Histogram { count, .. }) => count as f64,
            None => 0.0,
        }
    }

    fn startup(
        provider: &str,
        path: branchyard::startup::StartupPath,
        phases: branchyard::startup::Phases,
    ) -> Activity {
        Activity::Startup(Box::new(branchyard::startup::Startup {
            provider: provider.into(),
            path,
            phases,
        }))
    }

    #[test]
    fn a_tasks_new_branches_count_their_start_and_pool_use() {
        use branchyard::startup::{Phases, StartupPath};
        let metrics = Metrics::default();
        let pooled = |slot: Option<&str>| {
            let mut report =
                branchyard::WorkspaceReport::new(branchyard::WorkspacePhase::Setup, None);
            report.pool = Some(Box::new(branchyard::PoolUse {
                slot: slot.map(str::to_owned),
                reason: None,
                requested_ms: 1_050,
                worktree_ms: 1,
            }));
            Activity::Workspace(report)
        };
        let created: BTreeSet<String> = ["hit", "miss", "plain"]
            .into_iter()
            .map(String::from)
            .collect();
        let entries = vec![
            entry(1, "hit", 1_100, pooled(Some("s1"))),
            entry(2, "hit", 1_200, Activity::Prompt("p".into())),
            entry(
                3,
                "hit",
                1_300,
                startup(
                    "agent_sandbox",
                    StartupPath::Pool,
                    Phases {
                        queued: Some(10),
                        sandbox_ready: Some(110),
                        first_event: Some(300),
                        ..Phases::default()
                    },
                ),
            ),
            entry(4, "miss", 1_100, pooled(None)),
            entry(5, "miss", 4_000, Activity::Prompt("p".into())),
            entry(6, "plain", 2_000, Activity::Prompt("p".into())),
            // Not a start: a later prompt, and another operation's branch.
            entry(7, "hit", 9_000, Activity::Prompt("again".into())),
            entry(8, "other", 9_000, Activity::Prompt("p".into())),
            entry(
                9,
                "other",
                9_100,
                startup("local", StartupPath::Local, Phases::default()),
            ),
            // A later turn's startup does not replace the first.
            entry(
                10,
                "hit",
                9_100,
                startup(
                    "agent_sandbox",
                    StartupPath::Resumed,
                    Phases {
                        queued: Some(1),
                        ..Phases::default()
                    },
                ),
            ),
        ];
        record_starts(&metrics, "app", 1_000, &created, &entries);
        let count = |pool: &str, provider: &str, path: &str| {
            counter(
                &metrics,
                metrics::START,
                &[("pool", pool), ("provider", provider), ("path", path)],
            )
        };
        assert_eq!(
            (
                count("hit", "agent_sandbox", "pool"),
                count("miss", "unknown", "unknown"),
                count("none", "unknown", "unknown"),
            ),
            (1.0, 1.0, 1.0)
        );
        let claims = |result: &str| {
            counter(
                &metrics,
                metrics::POOL_CLAIMS,
                &[("repo", "app"), ("result", result)],
            )
        };
        assert_eq!((claims("hit"), claims("miss")), (1.0, 1.0));
        let sum = |pairs: &[(&str, &str)]| metrics.distribution(metrics::START, pairs).1;
        assert_eq!(
            sum(&[
                ("pool", "hit"),
                ("provider", "agent_sandbox"),
                ("path", "pool")
            ]),
            0.2
        );
        assert_eq!(
            sum(&[
                ("pool", "miss"),
                ("provider", "unknown"),
                ("path", "unknown")
            ]),
            3.0
        );
        // Each phase the first startup reached counts the time it took:
        // 10 ms queued, 100 ms until the sandbox, 190 ms after it.
        let phase = |name: &str| {
            let (count, sum) = metrics.distribution(
                metrics::START_PHASE,
                &[
                    ("provider", "agent_sandbox"),
                    ("path", "pool"),
                    ("phase", name),
                ],
            );
            (sum, count)
        };
        assert_eq!(phase("queued"), (0.01, 1));
        assert_eq!(phase("sandbox_ready"), (0.1, 1));
        assert_eq!(phase("first_event"), (0.19, 1));
        // Phases that did not occur are not counted, and a branch outside
        // the operation or a later turn adds nothing.
        let text = metrics.render().unwrap();
        let phases = text
            .lines()
            .filter(|l| l.starts_with("branchyard_start_phase_seconds_count"))
            .count();
        assert_eq!(phases, 3, "{text}");
    }

    #[test]
    fn model_gateway_calls_become_metrics() {
        let observability = Observability::default();
        let call = |decision: &str, tokens: Option<serde_json::Value>, cost: Option<f64>| {
            let call: branchyard::models::ModelCall = serde_json::from_value(serde_json::json!({
                "model": "claude-sonnet-4-6", "api": "anthropic", "decision": decision,
                "status": 200, "latency_ms": 5, "tokens": tokens, "cost_usd": cost
            }))
            .unwrap();
            Activity::Model(Box::new(branchyard::models::ModelActivity::Call(call)))
        };
        let tokens = serde_json::json!({"input": 10, "output": 5, "cache_read": 100,
            "cache_write": 30, "cache_write_1h": 10});
        let entries = vec![
            entry(
                1,
                "a",
                1_000,
                call("allowed", Some(tokens.clone()), Some(0.25)),
            ),
            entry(2, "a", 1_100, call("allowed", Some(tokens), Some(0.5))),
            entry(3, "a", 1_200, call("denied", None, None)),
            entry(4, "other", 1_300, call("allowed", None, Some(9.0))),
        ];
        let branches: BTreeSet<String> = ["a".to_owned()].into();
        record_events(
            &observability,
            &branches,
            &|_| "claude-code".to_owned(),
            &entries,
            None,
        );
        let m = &observability.metrics;
        let model = ("model", "claude-sonnet-4-6");
        assert_eq!(
            counter(m, metrics::MODEL_CALLS, &[model, ("decision", "allowed")]),
            2.0
        );
        assert_eq!(
            counter(m, metrics::MODEL_CALLS, &[model, ("decision", "denied")]),
            1.0
        );
        assert_eq!(
            counter(m, metrics::MODEL_TOKENS, &[model, ("kind", "cache_write")]),
            80.0
        );
        assert_eq!(
            counter(m, metrics::MODEL_TOKENS, &[model, ("kind", "output")]),
            10.0
        );
        assert_eq!(counter(m, metrics::MODEL_COST, &[model]), 0.75);
    }

    #[test]
    fn tool_names_are_a_closed_set_of_labels() {
        assert_eq!(tool_label("Bash"), "bash");
        assert_eq!(tool_label("apply_patch"), "apply_patch");
        assert_eq!(tool_label("mcp__github__create_issue"), "mcp");
        assert_eq!(tool_label("totally-new-tool-7f3a"), "other");
        assert_eq!(tool_label(""), "other");
    }

    fn captured<'a>(spans: &'a [CapturedSpan], name: &str) -> &'a CapturedSpan {
        spans
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no span {name} in {spans:?}"))
    }

    #[test]
    fn turns_tools_and_connector_calls_become_metrics_and_spans() {
        let capture = capture().unwrap();
        let observability = Observability::default();
        let call: branchyard::connectors::ConnectorCall =
            serde_json::from_value(serde_json::json!({
                "connector": "github", "operation": "issues.list", "decision": "allowed",
                "latency_ms": 40
            }))
            .unwrap();
        let denied: branchyard::connectors::ConnectorCall =
            serde_json::from_value(serde_json::json!({
                "connector": "github", "operation": "issues.create", "decision": "denied",
                "reason": "policy_denied"
            }))
            .unwrap();
        let usage = |cumulative, input, output, cost| {
            Activity::Harness(Event::UsageObserved {
                turn: Some(1),
                usage: branchyard_harness::Usage {
                    cumulative,
                    input_tokens: Some(input),
                    output_tokens: Some(output),
                    cost_usd: Some(cost),
                    model: Some("gpt-5".into()),
                    ..branchyard_harness::Usage::default()
                },
            })
        };
        let entries = vec![
            entry(1, "a", 1_000, Activity::Status(BranchStatus::Running)),
            entry(2, "a", 1_000, Activity::Prompt("p".into())),
            entry(3, "other", 1_100, Activity::Prompt("not ours".into())),
            entry(
                4,
                "a",
                1_200,
                Activity::Harness(Event::TurnAccepted {
                    turn: 1,
                    native: None,
                }),
            ),
            entry(
                5,
                "a",
                1_500,
                Activity::Harness(Event::ToolStarted {
                    turn: 1,
                    call_id: "c1".into(),
                    name: "Bash".into(),
                    target: None,
                }),
            ),
            entry(6, "a", 2_000, Activity::ConnectorCall(Box::new(call))),
            entry(7, "a", 2_100, Activity::ConnectorCall(Box::new(denied))),
            // A running total: 100/10 tokens, then 150/30; counted as
            // 100/10 and then the growth, 50/20.
            entry(8, "a", 3_000, usage(true, 100, 10, 0.5)),
            entry(9, "a", 3_500, usage(true, 150, 30, 0.75)),
            entry(
                10,
                "a",
                4_000,
                Activity::Harness(Event::TurnEnded {
                    turn: 1,
                    outcome: TurnOutcome::Completed,
                }),
            ),
            entry(11, "a", 4_000, Activity::Status(BranchStatus::Ready)),
            // A second branch whose turn the engine stopped at a limit.
            entry(12, "b", 5_000, Activity::Prompt("q".into())),
            entry(
                13,
                "b",
                65_000,
                Activity::Status(BranchStatus::BudgetExceeded {
                    limit: "max_usd".into(),
                }),
            ),
        ];
        let branches: BTreeSet<String> = ["a".to_owned(), "b".to_owned()].into();
        let trace = branchyard_client::new_key();
        let parent = format!("00-{trace}-b7ad6b7169203331-01");
        let harness = |b: &str| match b {
            "a" => "codex".to_owned(),
            _ => "claude-code".to_owned(),
        };
        record_events(&observability, &branches, &harness, &entries, Some(&parent));
        let m = &observability.metrics;
        assert_eq!(
            counter(m, metrics::TURNS_STARTED, &[("harness", "codex")]),
            1.0
        );
        assert_eq!(
            counter(
                m,
                metrics::TURNS_ENDED,
                &[("harness", "codex"), ("outcome", "completed")]
            ),
            1.0
        );
        assert_eq!(
            counter(
                m,
                metrics::TURNS_ENDED,
                &[("harness", "claude-code"), ("outcome", "budget_exceeded")]
            ),
            1.0
        );
        assert_eq!(
            m.distribution(metrics::TURN_DURATION, &[("harness", "claude-code")]),
            (1, 60.0)
        );
        // The same turn as a GenAI client operation, by outcome.
        assert_eq!(
            m.distribution(
                metrics::GENAI_OPERATION_DURATION,
                &[
                    ("gen_ai.operation.name", "invoke_agent"),
                    ("gen_ai.agent.name", "claude-code"),
                    ("by.turn.outcome", "budget_exceeded"),
                    ("error.type", "budget_exceeded"),
                ]
            ),
            (1, 60.0)
        );
        // The first harness event after the prompt: the turn was accepted
        // 200 ms in.
        assert_eq!(
            m.distribution(metrics::TURN_FIRST_EVENT, &[("harness", "codex")]),
            (1, 0.2)
        );
        assert_eq!(
            counter(m, metrics::TOOL_CALLS, &[("harness", "codex")]),
            1.0
        );
        // A tool call lasts until the branch's next event: 500 ms.
        assert_eq!(
            m.distribution(
                metrics::TOOL_DURATION,
                &[("harness", "codex"), ("tool", "bash")]
            ),
            (1, 0.5)
        );
        assert_eq!(
            counter(
                m,
                metrics::CONNECTOR_CALLS,
                &[("connector", "github"), ("decision", "denied")]
            ),
            1.0
        );
        assert_eq!(
            m.distribution(
                metrics::CONNECTOR_DURATION,
                &[("connector", "github"), ("decision", "allowed")]
            ),
            (1, 0.04)
        );
        // Tokens count what a running total grew by; cost likewise.
        let tokens = |kind: &str| {
            m.distribution(
                metrics::GENAI_TOKEN_USAGE,
                &[
                    ("gen_ai.operation.name", "invoke_agent"),
                    ("gen_ai.token.type", kind),
                    ("gen_ai.agent.name", "codex"),
                    ("gen_ai.request.model", "gpt-5"),
                ],
            )
        };
        assert_eq!(tokens("input"), (2, 150.0));
        assert_eq!(tokens("output"), (2, 30.0));
        assert_eq!(
            m.value(
                metrics::USAGE_COST,
                &[("harness", "codex"), ("model", "gpt-5")]
            ),
            0.75
        );

        let spans = capture.trace(&trace);
        let turn = spans
            .iter()
            .find(|s| {
                s.name == "turn"
                    && s.attributes.get("by.harness").map(String::as_str) == Some("codex")
            })
            .unwrap();
        assert_eq!(turn.parent_id.as_deref(), Some("b7ad6b7169203331"));
        assert_eq!((turn.start_ms, turn.end_ms), (1_000, 4_000));
        assert_eq!(
            turn.attributes.get("by.turn").map(String::as_str),
            Some("1")
        );
        assert_eq!(turn.error, None);
        let tool = captured(&spans, "tool Bash");
        assert_eq!(tool.parent_id.as_deref(), Some(turn.span_id.as_str()));
        assert_eq!((tool.start_ms, tool.end_ms), (1_500, 2_000));
        let listed = captured(&spans, "connector issues.list");
        assert_eq!(listed.parent_id.as_deref(), Some(turn.span_id.as_str()));
        assert_eq!((listed.start_ms, listed.end_ms), (1_960, 2_000));
        assert_eq!(listed.kind, "client");
        assert!(listed.error.is_none());
        assert_eq!(
            captured(&spans, "connector issues.create").error.as_deref(),
            Some("policy_denied")
        );
        let stopped = spans
            .iter()
            .find(|s| s.attributes.get("by.outcome").map(String::as_str) == Some("budget_exceeded"))
            .unwrap();
        assert_eq!(stopped.error.as_deref(), Some("budget_exceeded"));
        assert!(
            !spans
                .iter()
                .any(|s| s.attributes.get("by.branch").map(String::as_str) == Some("other")),
            "another operation's branch is not this one's"
        );
    }

    #[test]
    fn nothing_is_written_as_a_span_without_a_parent() {
        let capture = capture().unwrap();
        let before = capture.spans().len();
        let observability = Observability::default();
        let entries = vec![
            entry(1, "solo", 1_000, Activity::Prompt("p".into())),
            entry(
                2,
                "solo",
                2_000,
                Activity::Harness(Event::TurnEnded {
                    turn: 1,
                    outcome: TurnOutcome::Completed,
                }),
            ),
        ];
        let branches: BTreeSet<String> = ["solo".to_owned()].into();
        record_events(&observability, &branches, &|_| "pi".into(), &entries, None);
        assert_eq!(
            counter(
                &observability.metrics,
                metrics::TURNS_ENDED,
                &[("harness", "pi"), ("outcome", "completed")]
            ),
            1.0
        );
        assert!(!capture.spans().iter().skip(before).any(|s| s
            .attributes
            .get("by.branch")
            .map(String::as_str)
            == Some("solo")));
    }

    #[test]
    fn cost_counts_what_each_branch_spent_during_the_operation() {
        let metrics = Metrics::default();
        let stored: StoredOperation = serde_json::from_value(serde_json::json!({
            "operation": {"id": "op", "repo": "r", "kind": "send", "state": "succeeded",
                          "branches": ["a"], "cursor": 0, "created_at_ms": 1},
            "tenant": "acme"
        }))
        .unwrap();
        let info = |name: &str,
                    harness: &str,
                    cost: Option<f64>,
                    stalled: bool|
         -> branchyard::BranchInfo {
            serde_json::from_value(serde_json::json!({
                "name": name, "git_branch": name, "worktree": "/w", "prompt": "p",
                "harness": harness, "profile": "p", "base": "main",
                "status": {"state": "ready"}, "turns": 1, "cost_usd": cost, "created_at": 0,
                "stalled": stalled
            }))
            .unwrap()
        };
        let before: BTreeMap<String, f64> = [("a".to_owned(), 1.0)].into();
        record_cost(
            &metrics,
            &stored,
            &before,
            &[
                info("a", "codex", Some(1.5), false),
                info("new", "codex", Some(0.25), true),
                info("free", "pi", None, false),
            ],
        );
        assert_eq!(
            counter(
                &metrics,
                metrics::COST,
                &[("tenant", "acme"), ("harness", "codex")]
            ),
            0.75
        );
        assert_eq!(
            counter(
                &metrics,
                metrics::COST,
                &[("tenant", "acme"), ("harness", "pi")]
            ),
            0.0
        );
        assert_eq!(
            counter(&metrics, metrics::TURN_STALLS, &[("harness", "codex")]),
            1.0
        );
        assert_eq!(
            counter(&metrics, metrics::TURN_STALLS, &[("harness", "pi")]),
            0.0
        );
    }
}
