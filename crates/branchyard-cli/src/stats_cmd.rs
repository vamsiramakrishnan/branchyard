//! `by stats`: a summary of the repository's branches, turns and cost,
//! read from the store locally, or from the server with `--remote`, where
//! it also shows the server's queue by priority. See
//! `docs/observability.md#by-stats`.

use std::collections::BTreeMap;

use branchyard::{Activity, BranchInfo, BranchStatus, Event, FeedEvent};
use branchyard_client::api::{Operation, OperationState};
use serde::Serialize;

use crate::commands::{open, print, Env, Outcome, Target};

/// Feed events read per page.
const PAGE: usize = 1000;

/// What `by stats` reports; `--json` prints it as is.
#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Stats {
    /// Branches by status (`running`, `ready`, ...).
    pub branches: BTreeMap<String, usize>,
    /// Turns the branches' records count, by harness.
    pub turns: BTreeMap<String, u64>,
    /// Harness-reported cost in USD, by harness.
    pub cost_usd: BTreeMap<String, f64>,
    /// From the event store (locally): how turns ended, by outcome.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcomes: Option<BTreeMap<String, u64>>,
    /// From the event store: turn durations in seconds, median and 90th
    /// percentile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_seconds: Option<Percentiles>,
    /// From the event store: tool calls the harnesses reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
    /// From the event store: connector gateway calls by decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connector_calls: Option<BTreeMap<String, u64>>,
    /// With `--remote`: the server's unfinished operations of this
    /// repository visible to the caller.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue: Option<Queue>,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Percentiles {
    pub count: usize,
    pub median: f64,
    pub p90: f64,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Queue {
    /// Queued (unclaimed) operations by priority, highest first.
    pub queued: BTreeMap<i32, usize>,
    pub running: usize,
    /// The longest a queued operation has waited, in seconds.
    pub oldest_seconds: Option<f64>,
}

fn state(status: &BranchStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

impl Stats {
    /// From the branches' records.
    pub fn from_branches(infos: &[BranchInfo]) -> Stats {
        let mut stats = Stats::default();
        for info in infos {
            *stats.branches.entry(state(&info.status)).or_default() += 1;
            *stats.turns.entry(info.harness.clone()).or_default() += u64::from(info.turns);
            if let Some(cost) = info.cost_usd {
                *stats.cost_usd.entry(info.harness.clone()).or_default() += cost;
            }
        }
        stats
    }

    /// Add what the event store says about turns, tools and connectors.
    pub fn add_events(&mut self, events: &[FeedEvent]) {
        let mut outcomes: BTreeMap<String, u64> = BTreeMap::new();
        let mut connectors: BTreeMap<String, u64> = BTreeMap::new();
        let mut tools = 0;
        let mut started: BTreeMap<&str, u64> = BTreeMap::new();
        let mut durations: Vec<f64> = Vec::new();
        for event in events {
            let at = event.event.at_ms;
            let branch = event.branch.as_str();
            let ended = |outcome: String,
                         started: &mut BTreeMap<&str, u64>,
                         outcomes: &mut BTreeMap<String, u64>,
                         durations: &mut Vec<f64>| {
                if let Some(start) = started.remove(branch) {
                    *outcomes.entry(outcome).or_default() += 1;
                    durations.push(at.saturating_sub(start) as f64 / 1000.0);
                }
            };
            match &event.event.activity {
                Activity::Prompt(_) => {
                    started.insert(branch, at);
                }
                Activity::Harness(Event::ToolStarted { .. }) => tools += 1,
                Activity::Harness(Event::TurnEnded { outcome, .. }) => {
                    let kind = serde_json::to_value(outcome)
                        .ok()
                        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned))
                        .unwrap_or_else(|| "unknown".into());
                    ended(kind, &mut started, &mut outcomes, &mut durations);
                }
                Activity::Harness(Event::OutcomeUnknown { .. }) => ended(
                    "unknown".into(),
                    &mut started,
                    &mut outcomes,
                    &mut durations,
                ),
                Activity::Status(status)
                    if !matches!(status, BranchStatus::Running | BranchStatus::Waiting) =>
                {
                    ended(state(status), &mut started, &mut outcomes, &mut durations)
                }
                Activity::ConnectorCall(call) => {
                    *connectors.entry(call.decision.clone()).or_default() += 1
                }
                _ => {}
            }
        }
        durations.sort_by(|a, b| a.total_cmp(b));
        let at = |q: f64| {
            let i = ((durations.len() as f64 - 1.0) * q).round() as usize;
            durations.get(i).copied().unwrap_or(0.0)
        };
        self.turn_seconds = Some(Percentiles {
            count: durations.len(),
            median: at(0.5),
            p90: at(0.9),
        });
        self.outcomes = Some(outcomes);
        self.tool_calls = Some(tools);
        self.connector_calls = Some(connectors);
    }

    /// Add the server's unfinished operations, as of `now_ms`.
    pub fn add_queue(&mut self, operations: &[Operation], now_ms: u64) {
        let mut queue = Queue::default();
        for op in operations {
            match op.state {
                OperationState::Queued => {
                    *queue.queued.entry(op.priority).or_default() += 1;
                    let waited = now_ms.saturating_sub(op.created_at_ms) as f64 / 1000.0;
                    queue.oldest_seconds = Some(queue.oldest_seconds.unwrap_or(0.0).max(waited));
                }
                OperationState::Running => queue.running += 1,
                _ => {}
            }
        }
        self.queue = Some(queue);
    }

    /// The summary as `by stats` prints it.
    pub fn render(&self) -> String {
        let total: usize = self.branches.values().sum();
        let mut out = format!(
            "branches  {total}{}\n",
            listed(self.branches.iter().map(|(k, v)| format!("{k} {v}")))
        );
        let turns: u64 = self.turns.values().sum();
        out += &format!(
            "turns     {turns}{}\n",
            listed(self.turns.iter().map(|(k, v)| format!("{k} {v}")))
        );
        if let Some(outcomes) = &self.outcomes {
            if !outcomes.is_empty() {
                out += &format!(
                    "outcomes  {}\n",
                    outcomes
                        .iter()
                        .map(|(k, v)| format!("{k} {v}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        if let Some(p) = self.turn_seconds.as_ref().filter(|p| p.count > 0) {
            out += &format!(
                "turn time median {}, p90 {}\n",
                seconds(p.median),
                seconds(p.p90)
            );
        }
        if let Some(tools) = self.tool_calls {
            out += &format!("tools     {tools} calls\n");
        }
        if let Some(calls) = self.connector_calls.as_ref().filter(|c| !c.is_empty()) {
            let n: u64 = calls.values().sum();
            out += &format!(
                "connectors {n} calls{}\n",
                listed(calls.iter().map(|(k, v)| format!("{k} {v}")))
            );
        }
        let cost: f64 = self.cost_usd.values().sum();
        out += &format!(
            "cost      ${cost:.2}{}\n",
            listed(self.cost_usd.iter().map(|(k, v)| format!("{k} ${v:.2}")))
        );
        if let Some(queue) = &self.queue {
            let queued: usize = queue.queued.values().sum();
            out += &format!(
                "queue     {queued} queued{}, {} running",
                listed(
                    queue
                        .queued
                        .iter()
                        .rev()
                        .map(|(p, n)| format!("priority {p}: {n}"))
                ),
                queue.running
            );
            if let Some(oldest) = queue.oldest_seconds {
                out += &format!("; oldest waiting {}", seconds(oldest));
            }
            out.push('\n');
        }
        out
    }
}

/// ` (a, b, c)`, or nothing for none.
fn listed(items: impl Iterator<Item = String>) -> String {
    let items: Vec<String> = items.collect();
    match items.is_empty() {
        true => String::new(),
        false => format!(" ({})", items.join(", ")),
    }
}

fn seconds(s: f64) -> String {
    match s {
        s if s < 60.0 => format!("{s:.1}s"),
        s if s < 3600.0 => format!("{}m{:02}s", (s / 60.0) as u64, (s % 60.0) as u64),
        s => format!(
            "{}h{:02}m",
            (s / 3600.0) as u64,
            ((s % 3600.0) / 60.0) as u64
        ),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `by stats [--json]`.
pub fn main(_env: &Env, target: &Target, as_json: bool) -> Outcome {
    let stats = match target {
        Target::Local => {
            let yard = open()?;
            let mut stats = Stats::from_branches(&yard.branches()?);
            let mut events = Vec::new();
            let mut cursor = 0;
            loop {
                let page = yard.events_since(cursor, PAGE)?;
                if page.events.is_empty() {
                    break;
                }
                cursor = page.next_cursor;
                events.extend(page.events);
            }
            stats.add_events(&events);
            stats
        }
        Target::Remote(remote) => {
            let mut stats = Stats::from_branches(&remote.repo.branches()?);
            stats.add_queue(&remote.repo.operations(None)?, now_ms());
            stats
        }
    };
    if as_json {
        let value = serde_json::to_value(&stats).unwrap_or_default();
        return print(&crate::json::text(&value));
    }
    print(&stats.render())
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::RecordedEvent;

    fn info(name: &str, harness: &str, status: BranchStatus, turns: u32, cost: f64) -> BranchInfo {
        let mut value = serde_json::json!({
            "name": name, "git_branch": name, "worktree": "/w", "prompt": "p",
            "harness": harness, "profile": "p", "base": "main",
            "status": status, "turns": turns, "cost_usd": cost, "created_at": 0
        });
        value["status"] = serde_json::to_value(status).unwrap();
        serde_json::from_value(value).unwrap()
    }

    fn event(position: u64, branch: &str, at_ms: u64, activity: Activity) -> FeedEvent {
        FeedEvent {
            position,
            branch: branch.into(),
            event: serde_json::from_value(serde_json::json!({
                "at_ms": at_ms,
                "activity": activity,
            }))
            .map(|e: RecordedEvent| e)
            .unwrap(),
        }
    }

    #[test]
    fn branches_events_and_the_queue_add_up() {
        let mut stats = Stats::from_branches(&[
            info("a", "codex", BranchStatus::Ready, 2, 1.5),
            info("b", "codex", BranchStatus::Running, 1, 0.25),
            info("c", "claude-code", BranchStatus::Interrupted, 3, 2.0),
        ]);
        assert_eq!(stats.branches["ready"], 1);
        assert_eq!(stats.turns["codex"], 3);
        assert_eq!(stats.cost_usd["codex"], 1.75);
        stats.add_events(&[
            event(1, "a", 0, Activity::Prompt("p".into())),
            event(
                2,
                "a",
                500,
                Activity::Harness(Event::ToolStarted {
                    turn: 1,
                    call_id: "c".into(),
                    name: "Bash".into(),
                }),
            ),
            event(
                3,
                "a",
                10_000,
                Activity::Harness(Event::TurnEnded {
                    turn: 1,
                    outcome: branchyard::TurnOutcome::Completed,
                }),
            ),
            event(4, "c", 0, Activity::Prompt("p".into())),
            event(5, "c", 90_000, Activity::Status(BranchStatus::Interrupted)),
        ]);
        assert_eq!(
            stats.outcomes.as_ref().unwrap(),
            &BTreeMap::from([("completed".into(), 1), ("interrupted".into(), 1)])
        );
        assert_eq!(stats.tool_calls, Some(1));
        let seconds = stats.turn_seconds.as_ref().unwrap();
        assert_eq!(seconds.count, 2);
        assert_eq!(seconds.p90, 90.0);
        let op = |state: OperationState, priority: i32, created: u64| -> Operation {
            serde_json::from_value(serde_json::json!({
                "id": "op", "repo": "r", "kind": "task", "state": state, "branches": [],
                "cursor": 0, "created_at_ms": created, "priority": priority
            }))
            .unwrap()
        };
        stats.add_queue(
            &[
                op(OperationState::Queued, 5, 1_000),
                op(OperationState::Queued, 0, 4_000),
                op(OperationState::Queued, 5, 9_000),
                op(OperationState::Running, 0, 9_000),
            ],
            11_000,
        );
        let queue = stats.queue.as_ref().unwrap();
        assert_eq!(queue.queued[&5], 2);
        assert_eq!(queue.running, 1);
        assert_eq!(queue.oldest_seconds, Some(10.0));
        let text = stats.render();
        for expected in [
            "branches  3 (interrupted 1, ready 1, running 1)",
            "turns     6 (claude-code 3, codex 3)",
            "outcomes  completed 1, interrupted 1",
            "tools     1 calls",
            "cost      $3.75 (claude-code $2.00, codex $1.75)",
            "queue     3 queued (priority 5: 2, priority 0: 1), 1 running; oldest waiting 10.0s",
        ] {
            assert!(text.contains(expected), "{expected:?} missing from\n{text}");
        }
    }
}
