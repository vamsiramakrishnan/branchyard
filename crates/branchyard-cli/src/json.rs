//! Machine-readable output for `--json`. Branches, statuses, candidates
//! and harness profiles use the SDK's serde forms, the same values the
//! server returns. A recorded event's shape is spelled out here: it
//! flattens the activity, unlike the SDK's form, and is the CLI's contract.

use branchyard::{
    Activity, BranchInfo, BranchStatus, CandidateInfo, DecisionSource, Event, HarnessInfo,
    RecordedEvent, TurnOutcome,
};
use serde_json::{json, Value};

/// The SDK's own serde form, which is the CLI's contract for these types:
/// a branch, its status and candidate, and a harness profile print the same
/// JSON from `by`, `by --remote` and the server.
#[allow(clippy::expect_used)] // ratchet: branchyard-cli
fn serde(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("SDK values serialize")
}

pub fn status(status: &BranchStatus) -> Value {
    serde(status)
}

fn candidate(c: &CandidateInfo) -> Value {
    serde(c)
}

pub fn branch(info: &BranchInfo) -> Value {
    serde(info)
}

pub fn harness(info: &HarnessInfo) -> Value {
    serde(info)
}

fn outcome(outcome: &TurnOutcome) -> Value {
    match outcome {
        TurnOutcome::Completed => json!({ "kind": "completed" }),
        TurnOutcome::Interrupted => json!({ "kind": "interrupted" }),
        TurnOutcome::Failed { message } => json!({ "kind": "failed", "message": message }),
        TurnOutcome::LimitReached { limit } => json!({ "kind": "limit_reached", "limit": limit }),
        TurnOutcome::Refused => json!({ "kind": "refused" }),
    }
}

/// An event as `{"type": ..., fields...}`.
pub fn event(event: &Event) -> Value {
    match event {
        Event::Ready => json!({ "type": "ready" }),
        Event::SessionStarted {
            session,
            forked_from,
        } => json!({
            "type": "session_started",
            "session": session.as_str(),
            "forked_from": forked_from.as_ref().map(|s| s.as_str()),
        }),
        Event::OpenFailed { reason } => json!({ "type": "open_failed", "reason": reason }),
        Event::TurnAccepted { turn, native } => {
            json!({ "type": "turn_accepted", "turn": turn, "native": native })
        }
        Event::MessageDelta { turn, text } => {
            json!({ "type": "message_delta", "turn": turn, "text": text })
        }
        Event::ToolStarted {
            turn,
            call_id,
            name,
        } => json!({ "type": "tool_started", "turn": turn, "call_id": call_id, "name": name }),
        Event::PermissionRequested { turn, request } => json!({
            "type": "permission_requested",
            "turn": turn,
            "key": request.key.0,
            "tool": request.tool,
            "input": request.input,
        }),
        Event::PermissionWithdrawn { key } => {
            json!({ "type": "permission_withdrawn", "key": key.0 })
        }
        Event::UsageObserved { turn, usage } => json!({
            "type": "usage_observed",
            "turn": turn,
            "cumulative": usage.cumulative,
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cached_input_tokens": usage.cached_input_tokens,
            "cost_usd": usage.cost_usd,
            "cache_write_tokens": usage.cache_write_tokens,
            "cache_write_1h_tokens": usage.cache_write_1h_tokens,
            "model": usage.model,
        }),
        Event::InterruptAcknowledged { turn } => {
            json!({ "type": "interrupt_acknowledged", "turn": turn })
        }
        Event::SteerAccepted { turn, steer } => {
            json!({ "type": "steer_accepted", "turn": turn, "steer": steer })
        }
        Event::SteerRejected {
            turn,
            steer,
            reason,
        } => json!({ "type": "steer_rejected", "turn": turn, "steer": steer, "reason": reason }),
        Event::TurnEnded { turn, outcome: o } => {
            json!({ "type": "turn_ended", "turn": turn, "outcome": outcome(o) })
        }
        Event::OutcomeUnknown { turn, reason } => {
            json!({ "type": "outcome_unknown", "turn": turn, "reason": reason })
        }
        Event::UnsupportedRequest { method } => {
            json!({ "type": "unsupported_request", "method": method })
        }
        Event::Warning { message } => json!({ "type": "warning", "message": message }),
        Event::ProtocolViolation { detail } => {
            json!({ "type": "protocol_violation", "detail": detail })
        }
        Event::Progress { turn } => json!({ "type": "progress", "turn": turn }),
        Event::HarnessTaskStarted { task, background } => json!({
            "type": "harness_task_started",
            "task": task_json(task),
            "background": background,
        }),
        Event::HarnessTaskEnded {
            task_id,
            status,
            summary,
        } => json!({
            "type": "harness_task_ended",
            "task_id": task_id,
            "status": status,
            "summary": summary,
        }),
        Event::BackgroundTasks { running } => json!({
            "type": "background_tasks",
            "running": running.iter().map(task_json).collect::<Vec<_>>(),
        }),
        Event::Unrecognized { kind } => json!({ "type": "unrecognized", "kind": kind }),
        Event::SessionClosed => json!({ "type": "session_closed" }),
    }
}

fn task_json(task: &branchyard::HarnessTask) -> Value {
    json!({ "task_id": task.task_id, "kind": task.kind, "description": task.description })
}

fn source(source: &DecisionSource) -> Value {
    match source {
        DecisionSource::Rule { pattern } => json!({ "kind": "rule", "pattern": pattern }),
        DecisionSource::Default => json!({ "kind": "default" }),
        DecisionSource::Asked => json!({ "kind": "asked" }),
        DecisionSource::Engine => json!({ "kind": "engine" }),
        DecisionSource::Approval { resolved, ask, by } => json!({
            "kind": "approval",
            "resolved": resolved,
            "ask": ask,
            "by": by,
        }),
    }
}

/// Recorded activity as `{"at_ms": ..., "activity": <kind>, fields...}`;
/// a harness event keeps its own object under `event`.
pub fn recorded(recorded: &RecordedEvent) -> Value {
    let mut value = match &recorded.activity {
        Activity::Harness(e) => json!({ "activity": "harness", "event": event(e) }),
        Activity::Prompt(text) => json!({ "activity": "prompt", "text": text }),
        Activity::Decision {
            tool,
            allowed,
            message,
            source: s,
        } => json!({
            "activity": "decision",
            "tool": tool,
            "allowed": allowed,
            "message": message,
            "source": source(s),
        }),
        Activity::Snapshot(c) => json!({ "activity": "snapshot", "candidate": candidate(c) }),
        Activity::Status(s) => json!({ "activity": "status", "status": status(s) }),
        Activity::Warning(message) => json!({ "activity": "warning", "message": message }),
        Activity::Delegation {
            tool,
            branch,
            outcome,
            refused,
        } => json!({
            "activity": "delegation",
            "tool": tool,
            "branch": branch,
            "outcome": outcome,
            "refused": refused,
        }),
        Activity::Provisioned {
            auth,
            files,
            env,
            secrets,
            unused_secrets,
            connectors,
            knowledge,
        } => json!({
            "activity": "provisioned",
            "auth": auth,
            "files": files,
            "env": env,
            "secrets": secrets,
            "unused_secrets": unused_secrets,
            "connectors": connectors,
            "knowledge": knowledge,
        }),
        Activity::Steered { id, by, text } => json!({
            "activity": "steered",
            "id": id,
            "by": by,
            "text": text,
        }),
        Activity::Recovered { reason, killed } => json!({
            "activity": "recovered",
            "reason": reason,
            "killed": killed,
        }),
        Activity::Stalled { since_ms } => json!({
            "activity": "stalled",
            "since_ms": since_ms,
        }),
        Activity::Resumed => json!({ "activity": "resumed" }),
        Activity::Message(m) => json!({ "activity": "message", "message": m }),
        Activity::MessagesDelivered { ids, via } => json!({
            "activity": "messages_delivered",
            "ids": ids,
            "via": via,
        }),
        Activity::Checkpoint(c) => json!({ "activity": "checkpoint", "checkpoint": c }),
        Activity::Rewound {
            from,
            to,
            commit,
            session,
        } => json!({
            "activity": "rewound",
            "from": from,
            "to": to,
            "commit": commit,
            "session": session,
        }),
        Activity::ForkedAt {
            branch,
            turn,
            commit,
            session,
        } => json!({
            "activity": "forked_at",
            "branch": branch,
            "turn": turn,
            "commit": commit,
            "session": session,
        }),
        Activity::PullRequest(activity) => json!({
            "activity": "pull_request",
            "pull_request": serde(activity),
        }),
        Activity::Workspace(report) => {
            let mut value = serde_json::to_value(report).unwrap_or_default();
            value["activity"] = json!("workspace");
            value
        }
        Activity::ConnectorCall(call) => json!({
            "activity": "connector_call",
            "connector_call": call,
        }),
        Activity::Egress(egress) => json!({
            "activity": "egress",
            "egress": egress,
            "text": egress.describe(),
        }),
        Activity::Adopted(adoption) => json!({
            "activity": "adopted",
            "adopted": adoption,
        }),
        Activity::Sandbox(event) => json!({
            "activity": "sandbox",
            "sandbox": event,
            "text": event.describe(),
        }),
        Activity::Fleet(activity) => json!({
            "activity": "fleet",
            "fleet": activity,
            "text": activity.describe(),
        }),
        Activity::Knowledge(activity) => json!({
            "activity": "knowledge",
            "knowledge": activity,
            "text": activity.describe(),
        }),
        Activity::Plan(activity) => json!({
            "activity": "plan",
            "plan": activity,
            "text": activity.describe(),
        }),
        Activity::Goal(activity) => json!({
            "activity": "goal",
            "goal": activity,
            "text": activity.describe(),
        }),
        Activity::Model(activity) => json!({
            "activity": "model",
            "model": activity,
            "text": activity.describe(),
        }),
        Activity::Access(activity) => json!({
            "activity": "access",
            "access": activity,
            "text": activity.describe(),
        }),
        Activity::Effect(activity) => json!({
            "activity": "effect",
            "effect": activity,
            "text": activity.describe(),
        }),
    };
    value["at_ms"] = json!(recorded.at_ms);
    value
}

/// Pretty JSON with a trailing newline.
#[allow(clippy::expect_used)] // ratchet: branchyard-cli
pub fn text(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("a JSON value always serializes");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::{NativeSession, PermissionKey, PermissionRequest, Usage};
    use std::path::PathBuf;

    #[test]
    fn branch_json_has_every_field() {
        let info = BranchInfo {
            name: "b".into(),
            git_branch: "by/b".into(),
            worktree: PathBuf::from("/w"),
            prompt: "p".into(),
            harness: "codex".into(),
            profile: "codex-app-server".into(),
            session: Some("s".into()),
            parent: None,
            children: vec!["kid".into()],
            depth: 0,
            base: "base".into(),
            candidate: Some(CandidateInfo {
                commit: "c".into(),
                files_changed: 1,
                insertions: 2,
                deletions: 3,
            }),
            status: BranchStatus::Failed { reason: "r".into() },
            turns: 2,
            cost_usd: None,
            created_at: 7,
            stalled: false,
            superseded_by: None,
        };
        let value = branch(&info);
        assert_eq!(value["status"], json!({ "state": "failed", "reason": "r" }));
        assert_eq!(value["candidate"]["deletions"], 3);
        assert_eq!(value["parent"], Value::Null);
        assert_eq!(value["cost_usd"], Value::Null);
        assert_eq!(value["worktree"], "/w");
        assert_eq!(value["children"], json!(["kid"]));
        assert_eq!(value["depth"], 0);
        // The order `by --json` has always printed.
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "name",
                "git_branch",
                "worktree",
                "prompt",
                "harness",
                "profile",
                "session",
                "parent",
                "children",
                "depth",
                "base",
                "candidate",
                "status",
                "turns",
                "cost_usd",
                "created_at",
                "stalled",
                "superseded_by"
            ]
        );
        let harness = harness(&HarnessInfo {
            harness: "codex".into(),
            profile: "codex-app-server".into(),
            default: true,
            available: false,
            qualification: None,
        });
        assert_eq!(
            harness,
            json!({"harness": "codex", "profile": "codex-app-server", "default": true,
                   "available": false, "qualification": null})
        );
        let keys: Vec<&str> = harness
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "harness",
                "profile",
                "default",
                "available",
                "qualification"
            ]
        );
    }

    #[test]
    fn events_are_tagged_by_type() {
        let started = Event::SessionStarted {
            session: NativeSession::new("s2").unwrap(),
            forked_from: NativeSession::new("s1"),
        };
        assert_eq!(
            event(&started),
            json!({ "type": "session_started", "session": "s2", "forked_from": "s1" })
        );
        let asked = Event::PermissionRequested {
            turn: Some(1),
            request: PermissionRequest {
                key: PermissionKey("k".into()),
                tool: "Bash".into(),
                input: json!({ "command": "ls" }),
            },
        };
        assert_eq!(event(&asked)["input"]["command"], "ls");
        let ended = Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::LimitReached { limit: "l".into() },
        };
        assert_eq!(
            event(&ended)["outcome"],
            json!({ "kind": "limit_reached", "limit": "l" })
        );
        let usage = Event::UsageObserved {
            turn: None,
            usage: Usage {
                cost_usd: Some(0.5),
                ..Usage::default()
            },
        };
        assert_eq!(event(&usage)["input_tokens"], Value::Null);
        assert_eq!(event(&usage)["cost_usd"], 0.5);
        assert_eq!(
            recorded(&RecordedEvent {
                at_ms: 5,
                activity: Activity::Harness(Event::SessionClosed)
            }),
            json!({ "at_ms": 5, "activity": "harness", "event": { "type": "session_closed" } })
        );
        assert_eq!(
            recorded(&RecordedEvent {
                at_ms: 6,
                activity: Activity::Decision {
                    tool: "Bash".into(),
                    allowed: false,
                    message: Some("no".into()),
                    source: DecisionSource::Rule {
                        pattern: "Bash".into()
                    },
                }
            }),
            json!({
                "at_ms": 6,
                "activity": "decision",
                "tool": "Bash",
                "allowed": false,
                "message": "no",
                "source": { "kind": "rule", "pattern": "Bash" },
            })
        );
        assert_eq!(
            recorded(&RecordedEvent {
                at_ms: 7,
                activity: Activity::Status(BranchStatus::Ready)
            }),
            json!({ "at_ms": 7, "activity": "status", "status": { "state": "ready" } })
        );
    }
}
