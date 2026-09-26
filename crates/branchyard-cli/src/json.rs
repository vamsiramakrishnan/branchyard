//! Machine-readable output for `--json`. The SDK types carry no serde
//! derives, so the shapes are spelled out here; they are the CLI's contract.

use branchyard::{BranchInfo, BranchStatus, Event, HarnessInfo, RecordedEvent, TurnOutcome};
use serde_json::{json, Value};

pub fn status(status: &BranchStatus) -> Value {
    match status {
        BranchStatus::Running => json!({ "state": "running" }),
        BranchStatus::Ready => json!({ "state": "ready" }),
        BranchStatus::NoChanges => json!({ "state": "no_changes" }),
        BranchStatus::Interrupted => json!({ "state": "interrupted" }),
        BranchStatus::BudgetExceeded { limit } => {
            json!({ "state": "budget_exceeded", "limit": limit })
        }
        BranchStatus::Failed { reason } => json!({ "state": "failed", "reason": reason }),
        BranchStatus::Merged { target, commit } => {
            json!({ "state": "merged", "target": target, "commit": commit })
        }
    }
}

pub fn branch(info: &BranchInfo) -> Value {
    json!({
        "name": info.name,
        "git_branch": info.git_branch,
        "worktree": info.worktree.display().to_string(),
        "prompt": info.prompt,
        "harness": info.harness,
        "profile": info.profile,
        "session": info.session,
        "parent": info.parent,
        "base": info.base,
        "candidate": info.candidate.as_ref().map(|c| json!({
            "commit": c.commit,
            "files_changed": c.files_changed,
            "insertions": c.insertions,
            "deletions": c.deletions,
        })),
        "status": status(&info.status),
        "turns": info.turns,
        "cost_usd": info.cost_usd,
        "created_at": info.created_at,
    })
}

pub fn harness(info: &HarnessInfo) -> Value {
    json!({
        "harness": info.harness,
        "profile": info.profile,
        "default": info.default,
        "available": info.available,
        "qualification": info.qualification,
    })
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
        }),
        Event::InterruptAcknowledged { turn } => {
            json!({ "type": "interrupt_acknowledged", "turn": turn })
        }
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
        Event::Unrecognized { kind } => json!({ "type": "unrecognized", "kind": kind }),
        Event::SessionClosed => json!({ "type": "session_closed" }),
    }
}

pub fn recorded(recorded: &RecordedEvent) -> Value {
    json!({ "at_ms": recorded.at_ms, "event": event(&recorded.event) })
}

/// Pretty JSON with a trailing newline.
pub fn text(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("a JSON value always serializes");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::{CandidateInfo, PermissionRequest, Usage};
    use branchyard_harness::{NativeSession, PermissionKey};
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
        };
        let value = branch(&info);
        assert_eq!(value["status"], json!({ "state": "failed", "reason": "r" }));
        assert_eq!(value["candidate"]["deletions"], 3);
        assert_eq!(value["parent"], Value::Null);
        assert_eq!(value["cost_usd"], Value::Null);
        assert_eq!(value["worktree"], "/w");
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
                event: Event::SessionClosed
            }),
            json!({ "at_ms": 5, "event": { "type": "session_closed" } })
        );
    }
}
