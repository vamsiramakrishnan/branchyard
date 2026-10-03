//! Approvals for a harness's tools. They tighten what the turn's
//! permission policy allowed and never loosen it: a tool the policy
//! denies stays denied whatever an approval says, so a planning turn stays
//! read-only. A tool no layer names is left to the policy.

use serde_json::Value;

use super::ask::{self, AskSpec};
use super::{resolve, Approval, AskAbout, Layers, Subject};
use crate::state::Record;
use crate::{DecisionSource, PermissionDecision, PolicyPreset, Yard};

/// The decision for `tool`, which the policy allowed, when an approval
/// layer names it: `None` leaves the policy's allow as it is. An ask waits
/// until it is answered, the turn's deadline passes, or `cancelled` says
/// the turn is stopping.
pub(crate) fn decide(
    yard: &Yard,
    record: &Record,
    tool: &str,
    input: &Value,
    preset: Option<PolicyPreset>,
    deadline_ms: Option<u64>,
    cancelled: &dyn Fn() -> bool,
) -> Option<(PermissionDecision, DecisionSource)> {
    let settings = yard.approval_settings();
    let subject = record
        .actor
        .as_ref()
        .map(|a| a.subject.clone())
        .or_else(|| yard.connectors().map(|g| g.subject.clone()))
        .unwrap_or_else(crate::connectors::local_subject);
    let preset = preset.map(|p| p.approvals());
    let layers = Layers {
        admin: settings.admin.as_ref(),
        seat: record.provision.as_ref().and_then(|p| p.approvals.as_ref()),
        person: settings.person_for(&subject),
        preset: preset.as_ref(),
    };
    let resolved = resolve(&layers, &Subject::Tool(tool))?;
    let source = |ask: Option<String>, by: Option<String>| DecisionSource::Approval {
        resolved: resolved.clone(),
        ask,
        by,
    };
    match resolved.approval {
        Approval::Allow => None,
        Approval::Block => Some((
            PermissionDecision::Deny {
                message: format!(
                    "Blocked by Branchyard's approval policy ({}).",
                    resolved.describe()
                ),
            },
            source(None, None),
        )),
        // A tool has no draft form: staging it is asking.
        Approval::Ask | Approval::Stage => {
            let asked = ask::open(
                yard,
                AskSpec {
                    branch: record.info.name.clone(),
                    turn: record.info.turns + 1,
                    subject,
                    about: AskAbout::Tool {
                        tool: tool.to_owned(),
                    },
                    effect: None,
                    resolved: resolved.clone(),
                    request: Some(input.clone()),
                    deadline_ms,
                },
            );
            let asked = match asked {
                Ok(asked) => asked,
                Err(why) => {
                    return Some((
                        PermissionDecision::Deny {
                            message: format!("Branchyard could not ask a person: {why}"),
                        },
                        source(None, None),
                    ))
                }
            };
            let answer = ask::wait(yard, &asked, cancelled);
            Some(match answer {
                Ok(answer) if answer.allow => (
                    PermissionDecision::Allow,
                    source(Some(asked.id), Some(answer.by)),
                ),
                Ok(answer) => (
                    PermissionDecision::Deny {
                        message: format!(
                            "Not approved: denied by {} ({}){}.",
                            answer.by,
                            answer.surface,
                            answer
                                .reason
                                .as_deref()
                                .map(|r| format!(": {r}"))
                                .unwrap_or_default()
                        ),
                    },
                    source(Some(asked.id), Some(answer.by)),
                ),
                Err(why) => (
                    PermissionDecision::Deny {
                        message: format!("Branchyard could not wait for an answer: {why}"),
                    },
                    source(Some(asked.id), None),
                ),
            })
        }
    }
}
