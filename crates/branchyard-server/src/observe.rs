//! What an operation's run adds to metrics and traces.
//!
//! The dispatcher ([`crate::ops`]) counts admissions, claims, lease
//! renewals and expiries, and finished operations as they happen, and
//! records the admission, claim and operation spans. What happened inside
//! the operation (turns, tool calls, connector calls, cost) is the
//! engine's and the harness's, recorded as the branch's events; once the
//! operation finishes, `record_run` reads the events it recorded (from
//! its feed position at admission to its end) and turns them into
//! counters, a turn-duration histogram, and spans with the events' own
//! timestamps, children of the operation's span. Each operation is run by
//! one worker, so this is counted once across servers sharing a database.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use branchyard::{Activity, BranchStatus, Event};
use branchyard_client::api::FeedEntry;

use crate::metrics::{self, Metrics};
use crate::store::StoredOperation;
use crate::telemetry::{SpanContext, SpanData, Tracer};

/// The process's metrics and tracer, shared by the dispatcher, the
/// executor, webhooks and the `/metrics` route. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct Observability {
    pub metrics: Arc<Metrics>,
    pub tracer: Tracer,
}

impl Observability {
    /// Metrics, and traces as the OpenTelemetry variables configure them.
    pub fn from_env() -> Observability {
        Observability {
            metrics: Arc::new(Metrics::default()),
            tracer: Tracer::from_env(),
        }
    }
}

/// A turn in progress in the events being read.
struct Turn {
    context: SpanContext,
    number: Option<u64>,
    start_ms: u64,
    tool: Option<Tool>,
}

struct Tool {
    context: SpanContext,
    name: String,
    call_id: String,
    start_ms: u64,
}

/// The `kind` (or `state`) tag of a serde-tagged value, such as a turn's
/// outcome.
fn tag(value: &impl serde::Serialize, key: &str) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.get(key).and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

/// Count and trace what `entries` (the feed between the operation's
/// admission and its end) say happened on `branches`, whose harness
/// `harness_of` names. Spans are children of `parent`, the operation's
/// span, when the operation is traced.
pub fn record_events(
    observability: &Observability,
    branches: &BTreeSet<String>,
    harness_of: &dyn Fn(&str) -> String,
    entries: &[FeedEntry],
    parent: Option<&SpanContext>,
) {
    let metrics = &observability.metrics;
    let tracer = &observability.tracer;
    let traced = parent.filter(|_| tracer.enabled());
    let mut turns: BTreeMap<&str, Turn> = BTreeMap::new();
    let end_tool = |turn: &mut Turn, branch: &str, at: u64| {
        if let (Some(tool), Some(_)) = (turn.tool.take(), traced) {
            tracer.record(
                SpanData::new(
                    format!("tool {}", tool.name),
                    tool.context,
                    Some(&turn.context),
                    tool.start_ms,
                    at,
                )
                .attr("by.branch", branch)
                .attr("by.tool", tool.name)
                .attr("by.call_id", tool.call_id),
            );
        }
    };
    let end_turn = |turn: Turn, branch: &str, harness: &str, at: u64, outcome: &str| {
        let mut turn = turn;
        end_tool(&mut turn, branch, at);
        metrics.inc(
            metrics::TURNS_ENDED,
            &[("harness", harness), ("outcome", outcome)],
        );
        metrics.observe(
            metrics::TURN_DURATION,
            &[("harness", harness)],
            at.saturating_sub(turn.start_ms) as f64 / 1000.0,
        );
        if let Some(parent) = traced {
            let mut span = SpanData::new("turn", turn.context, Some(parent), turn.start_ms, at)
                .attr("by.branch", branch)
                .attr("by.harness", harness)
                .attr("by.outcome", outcome);
            if let Some(n) = turn.number {
                span = span.attr("by.turn", n as i64);
            }
            if !matches!(outcome, "completed" | "ready" | "no_changes") {
                span.error = Some(outcome.to_owned());
            }
            tracer.record(span);
        }
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
            end_tool(turn, branch, at);
        }
        match &entry.activity {
            Activity::Prompt(_) => {
                if let Some(open) = turns.remove(branch) {
                    end_turn(open, branch, &harness, at, "unknown");
                }
                metrics.inc(metrics::TURNS_STARTED, &[("harness", &harness)]);
                let context = parent
                    .map(SpanContext::child)
                    .unwrap_or_else(SpanContext::root);
                turns.insert(
                    branch,
                    Turn {
                        context,
                        number: None,
                        start_ms: at,
                        tool: None,
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
            }) => {
                metrics.inc(metrics::TOOL_CALLS, &[("harness", &harness)]);
                if let Some(open) = turns.get_mut(branch) {
                    open.number.get_or_insert(*turn);
                    open.tool = Some(Tool {
                        context: open.context.child(),
                        name: name.clone(),
                        call_id: call_id.clone(),
                        start_ms: at,
                    });
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
                    let model = call.model.as_str();
                    metrics.inc(
                        metrics::MODEL_CALLS,
                        &[("model", model), ("decision", &call.decision)],
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
                                    &[("model", model), ("kind", kind)],
                                    count as f64,
                                );
                            }
                        }
                    }
                    if let Some(cost) = call.cost_usd.filter(|c| *c > 0.0) {
                        metrics.add(metrics::MODEL_COST, &[("model", model)], cost);
                    }
                }
            }
            Activity::ConnectorCall(call) => {
                metrics.inc(
                    metrics::CONNECTOR_CALLS,
                    &[("connector", &call.connector), ("decision", &call.decision)],
                );
                if let Some(parent) = traced {
                    let within = turns.get(branch).map(|t| t.context).unwrap_or(*parent);
                    let start = at.saturating_sub(call.latency_ms.unwrap_or(0));
                    let mut span = SpanData::new(
                        format!("connector {}", call.operation),
                        within.child(),
                        Some(&within),
                        start,
                        at,
                    )
                    .attr("by.branch", branch)
                    .attr("by.connector", call.connector.as_str())
                    .attr("by.operation", call.operation.as_str())
                    .attr("by.decision", call.decision.as_str());
                    span.kind = crate::telemetry::SpanKind::Client;
                    if call.decision != "allowed" {
                        span.error = Some(call.reason.clone().unwrap_or(call.decision.clone()));
                    }
                    tracer.record(span);
                }
            }
            _ => {}
        }
    }
    // A turn still open when the operation finished (it should not be):
    // its span ends with the last event read, and it is not counted ended.
    if let (Some(parent), Some(last)) = (traced, entries.last()) {
        for (branch, mut turn) in turns {
            end_tool(&mut turn, branch, last.at_ms);
            tracer.record(
                SpanData::new(
                    "turn",
                    turn.context,
                    Some(parent),
                    turn.start_ms,
                    last.at_ms,
                )
                .attr("by.branch", branch)
                .attr("by.outcome", "unfinished"),
            );
        }
    }
}

/// Count the start of each of a task's new branches (`created`): from the
/// operation's admission (`admitted_ms`) to the branch's first prompt, by
/// whether its worktree came from a warm pool (`hit`), had a pool and
/// found no slot (`miss`), or had none (`none`); and the pool's hits and
/// misses in `repo`.
pub fn record_starts(
    metrics: &Metrics,
    repo: &str,
    admitted_ms: u64,
    created: &BTreeSet<String>,
    entries: &[FeedEntry],
) {
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
                metrics.observe(
                    metrics::START,
                    &[("pool", pool)],
                    entry.at_ms.saturating_sub(admitted_ms) as f64 / 1000.0,
                );
            }
            _ => {}
        }
    }
}

/// Count `stored`'s harness cost: the growth of each branch's recorded
/// `cost_usd` over the operation (`before` holds what the branches that
/// existed at its start had).
pub fn record_cost(
    metrics: &Metrics,
    stored: &StoredOperation,
    before: &BTreeMap<String, f64>,
    after: &[branchyard::BranchInfo],
) {
    for info in after {
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
    use crate::metrics::Value;
    use crate::telemetry::{Attr, MemoryExporter};
    use branchyard::TurnOutcome;
    use std::time::Duration;

    fn entry(seq: u64, branch: &str, at_ms: u64, activity: Activity) -> FeedEntry {
        FeedEntry {
            seq,
            branch: branch.into(),
            at_ms,
            activity,
        }
    }

    fn counter(metrics: &Metrics, name: &str, labels: &[(&str, &str)]) -> f64 {
        let snapshot = metrics.snapshot();
        let want: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        match snapshot.get(name).and_then(|s| s.get(&want)) {
            Some(Value::Number(n)) => *n,
            Some(Value::Histogram { count, .. }) => *count as f64,
            None => 0.0,
        }
    }

    #[test]
    fn a_tasks_new_branches_count_their_start_and_pool_use() {
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
            entry(3, "miss", 1_100, pooled(None)),
            entry(4, "miss", 4_000, Activity::Prompt("p".into())),
            entry(5, "plain", 2_000, Activity::Prompt("p".into())),
            // Not a start: a later prompt, and another operation's branch.
            entry(6, "hit", 9_000, Activity::Prompt("again".into())),
            entry(7, "other", 9_000, Activity::Prompt("p".into())),
        ];
        record_starts(&metrics, "app", 1_000, &created, &entries);
        let count = |pool: &str| counter(&metrics, metrics::START, &[("pool", pool)]);
        assert_eq!(
            (count("hit"), count("miss"), count("none")),
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
        let snapshot = metrics.snapshot();
        let sum = |pool: &str| match snapshot[metrics::START]
            .get(&vec![("pool".to_string(), pool.to_string())])
        {
            Some(Value::Histogram { sum, .. }) => *sum,
            other => panic!("{other:?}"),
        };
        assert_eq!(sum("hit"), 0.2);
        assert_eq!(sum("miss"), 3.0);
    }

    #[test]
    fn model_gateway_calls_become_metrics() {
        let observability = Observability {
            metrics: Arc::new(Metrics::default()),
            tracer: Tracer::new(Arc::new(MemoryExporter::default())),
        };
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
    fn turns_tools_and_connector_calls_become_metrics_and_spans() {
        let memory = Arc::new(MemoryExporter::default());
        let observability = Observability {
            metrics: Arc::new(Metrics::default()),
            tracer: Tracer::new(memory.clone()),
        };
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
                }),
            ),
            entry(6, "a", 2_000, Activity::ConnectorCall(Box::new(call))),
            entry(7, "a", 2_100, Activity::ConnectorCall(Box::new(denied))),
            entry(
                8,
                "a",
                4_000,
                Activity::Harness(Event::TurnEnded {
                    turn: 1,
                    outcome: TurnOutcome::Completed,
                }),
            ),
            entry(9, "a", 4_000, Activity::Status(BranchStatus::Ready)),
            // A second branch whose turn the engine stopped at a limit.
            entry(10, "b", 5_000, Activity::Prompt("q".into())),
            entry(
                11,
                "b",
                65_000,
                Activity::Status(BranchStatus::BudgetExceeded {
                    limit: "max_usd".into(),
                }),
            ),
        ];
        let branches: BTreeSet<String> = ["a".to_owned(), "b".to_owned()].into();
        let parent = SpanContext::root();
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
            counter(m, metrics::TURN_DURATION, &[("harness", "claude-code")]),
            1.0
        );
        match &m.snapshot()[metrics::TURN_DURATION]
            [&vec![("harness".to_owned(), "claude-code".to_owned())]]
        {
            Value::Histogram { sum, .. } => assert_eq!(*sum, 60.0),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            counter(m, metrics::TOOL_CALLS, &[("harness", "codex")]),
            1.0
        );
        assert_eq!(
            counter(
                m,
                metrics::CONNECTOR_CALLS,
                &[("connector", "github"), ("decision", "denied")]
            ),
            1.0
        );
        observability.tracer.flush(Duration::from_secs(10));
        let spans = memory.spans();
        let named = |name: &str| spans.iter().find(|s| s.name == name).unwrap();
        let turn = spans
            .iter()
            .find(|s| {
                s.name == "turn" && s.attribute("by.harness") == Some(&Attr::Str("codex".into()))
            })
            .unwrap();
        assert_eq!(turn.parent, Some(parent.span_id));
        assert_eq!(turn.context.trace_id, parent.trace_id);
        assert_eq!((turn.start_ns, turn.end_ns), (1_000_000_000, 4_000_000_000));
        assert_eq!(turn.attribute("by.turn"), Some(&Attr::Int(1)));
        let tool = named("tool Bash");
        assert_eq!(tool.parent, Some(turn.context.span_id));
        // A tool call lasts until the branch's next event.
        assert_eq!((tool.start_ns, tool.end_ns), (1_500_000_000, 2_000_000_000));
        let listed = named("connector issues.list");
        assert_eq!(listed.parent, Some(turn.context.span_id));
        assert_eq!(
            (listed.start_ns, listed.end_ns),
            (1_960_000_000, 2_000_000_000)
        );
        assert!(listed.error.is_none());
        assert_eq!(
            named("connector issues.create").error.as_deref(),
            Some("policy_denied")
        );
        let stopped = spans
            .iter()
            .find(|s| s.attribute("by.outcome") == Some(&Attr::Str("budget_exceeded".into())))
            .unwrap();
        assert_eq!(stopped.error.as_deref(), Some("budget_exceeded"));
        assert!(
            !spans
                .iter()
                .any(|s| s.attribute("by.branch") == Some(&Attr::Str("other".into()))),
            "another operation's branch is not this one's"
        );
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
        let info = |name: &str, harness: &str, cost: Option<f64>| -> branchyard::BranchInfo {
            serde_json::from_value(serde_json::json!({
                "name": name, "git_branch": name, "worktree": "/w", "prompt": "p",
                "harness": harness, "profile": "p", "base": "main",
                "status": {"state": "ready"}, "turns": 1, "cost_usd": cost, "created_at": 0
            }))
            .unwrap()
        };
        let before: BTreeMap<String, f64> = [("a".to_owned(), 1.0)].into();
        record_cost(
            &metrics,
            &stored,
            &before,
            &[
                info("a", "codex", Some(1.5)),
                info("new", "codex", Some(0.25)),
                info("free", "pi", None),
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
    }
}
