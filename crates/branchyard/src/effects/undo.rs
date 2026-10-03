//! Undo: a plan from the ledger, then the chosen inverses.
//!
//! The branch's files and conversation are restored exactly by rewinding
//! to a checkpoint ([`crate::Branch::rewind`]). What it did upstream is
//! grouped by what the upstream supports: effects that can be undone (a
//! true inverse, before its deadline), that can be compensated, and that
//! cannot; plus calls whose outcome is unknown and staged calls that never
//! happened. Each inverse is a new effectful call through the same
//! gateway, under the branch's grant, approved by the person who chose it
//! and allowed by the approval policy; its outcome is recorded on the
//! original entry (`undone`, `compensated`, `undo_failed`, `expired`).

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::ask::note;
use super::mcp::{CallError, Client, ToolDecl};
use super::{
    resolve, Approval, ApprovalRecord, EffectActivity, EffectClass, EffectEntry, EffectMove,
    EffectState, Layers, Subject, UndoKind,
};
use crate::{Error, Yard};

/// One upstream effect in a plan.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UndoItem {
    pub entry: EffectEntry,
    /// What undoing it does, or why it cannot be, for people.
    pub note: String,
}

/// What undoing a branch to a turn would do.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UndoPlan {
    pub branch: String,
    /// The checkpoint files and conversation go back to; effects of later
    /// turns are planned.
    pub to: u32,
    /// Effects with a true inverse, before its deadline.
    pub reversible: Vec<UndoItem>,
    /// Effects with a compensation, which leaves a trace.
    pub compensable: Vec<UndoItem>,
    /// Effects that cannot be undone, or whose undo expired.
    pub irreversible: Vec<UndoItem>,
    /// Calls whose outcome is unknown: reconcile them first.
    pub unknown: Vec<UndoItem>,
    /// Staged calls that never happened: undoing discards them.
    pub staged: Vec<UndoItem>,
    /// When the plan was made (milliseconds since the Unix epoch).
    pub at_ms: u64,
}

impl UndoPlan {
    /// The ids undoing would act on by default: every reversible effect,
    /// and every staged call.
    pub fn default_choice(&self) -> Vec<String> {
        self.reversible
            .iter()
            .chain(&self.staged)
            .map(|i| i.entry.id.clone())
            .collect()
    }

    /// Whether anything upstream happened after the checkpoint.
    pub fn is_empty(&self) -> bool {
        self.reversible.is_empty()
            && self.compensable.is_empty()
            && self.irreversible.is_empty()
            && self.unknown.is_empty()
            && self.staged.is_empty()
    }

    /// Every entry it acts on or reports, in plan order.
    pub fn items(&self) -> impl Iterator<Item = &UndoItem> {
        self.reversible
            .iter()
            .chain(&self.compensable)
            .chain(&self.staged)
            .chain(&self.unknown)
            .chain(&self.irreversible)
    }
}

/// `HH:MM` (UTC) of `ms`.
pub fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60)
}

/// The plan for `branch`'s effects after turn `to` (every turn for 0), at
/// `now_ms`.
pub(crate) fn plan(yard: &Yard, branch: &str, to: u32, now_ms: u64) -> Result<UndoPlan, Error> {
    let mut plan = UndoPlan {
        branch: branch.to_owned(),
        to,
        reversible: Vec::new(),
        compensable: Vec::new(),
        irreversible: Vec::new(),
        unknown: Vec::new(),
        staged: Vec::new(),
        at_ms: now_ms,
    };
    for entry in yard.store().effects().effects(Some(branch))? {
        if entry.turn <= to {
            continue;
        }
        match entry.state {
            EffectState::Staged => plan.staged.push(UndoItem {
                note: "staged, never performed: it is discarded".into(),
                entry,
            }),
            EffectState::Unknown | EffectState::Begun => plan.unknown.push(UndoItem {
                note: "outcome unknown: reconcile it first (by effects reconcile)".into(),
                entry,
            }),
            EffectState::Confirmed | EffectState::UndoFailed => {
                let undo = entry.undo.clone();
                match (entry.class, undo) {
                    (EffectClass::Read, _) => {}
                    (_, Some(undo)) if entry.expired_at(now_ms) => {
                        let deadline = undo.deadline_ms.unwrap_or(0);
                        plan.irreversible.push(UndoItem {
                            note: format!("its undo expired at {}", clock(deadline)),
                            entry,
                        })
                    }
                    (_, Some(undo)) if undo.kind == UndoKind::Inverse => {
                        let note = match undo.deadline_ms {
                            Some(d) => format!("undoable until {}", clock(d)),
                            None => "undoable".into(),
                        };
                        plan.reversible.push(UndoItem {
                            note: undo
                                .summary
                                .clone()
                                .map_or(note.clone(), |s| format!("{s} ({note})")),
                            entry,
                        })
                    }
                    (_, Some(undo)) => plan.compensable.push(UndoItem {
                        note: undo
                            .summary
                            .clone()
                            .unwrap_or_else(|| format!("compensated by {}", undo.operation)),
                        entry,
                    }),
                    (_, None) => plan.irreversible.push(UndoItem {
                        note: match entry.declared {
                            true => format!("happened at {}", clock(entry.updated_ms)),
                            false => format!(
                                "happened at {}; the gateway described no undo",
                                clock(entry.updated_ms)
                            ),
                        },
                        entry,
                    }),
                }
            }
            // Failed calls changed nothing; undone ones are done.
            _ => {}
        }
    }
    Ok(plan)
}

/// The plan as the person reads it.
pub fn render(plan: &UndoPlan, title: &str, files: bool) -> String {
    let mut out = match plan.to {
        0 => format!("Undoing \"{title}\":\n"),
        to => format!("Rewinding \"{title}\" to turn {to}:\n"),
    };
    let row = |out: &mut String, label: &str, text: &str| {
        out.push_str(&format!("  {label:<30}{text}\n"));
    };
    match files {
        true => row(&mut out, "files and conversation", "restored exactly"),
        false => row(&mut out, "files and conversation", "not changed"),
    }
    let groups: [(&str, &Vec<UndoItem>); 5] = [
        ("upstream, can be undone", &plan.reversible),
        ("upstream, can be compensated", &plan.compensable),
        ("upstream, cannot be undone", &plan.irreversible),
        ("upstream, outcome unknown", &plan.unknown),
        ("upstream, staged", &plan.staged),
    ];
    for (label, items) in groups {
        for (i, item) in items.iter().enumerate() {
            let label = if i == 0 { label } else { "" };
            row(
                &mut out,
                label,
                &format!(
                    "{} ({}) [{}]",
                    item.entry.title(),
                    item.note,
                    super::short_id(&item.entry.id)
                ),
            );
            if let (EffectState::Confirmed, EffectClass::Irreversible) =
                (item.entry.state, item.entry.class)
            {
                if let Some(detail) = item.entry.detail.as_deref().filter(|d| d.contains("draft")) {
                    row(&mut out, "", &format!("  -> {detail}"));
                }
            }
        }
    }
    if plan.is_empty() {
        row(&mut out, "upstream", "nothing happened after this turn");
    }
    out
}

/// What undoing one entry did.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UndoOutcome {
    pub id: String,
    /// The state the entry reached; unchanged when refused.
    pub state: EffectState,
    pub detail: String,
}

/// Perform the inverses of `chosen` (entry ids from `plan`) at `now_ms`,
/// approved by `by` through `surface`.
pub(crate) fn execute(
    yard: &Yard,
    plan: &UndoPlan,
    chosen: &[String],
    by: &str,
    surface: &str,
    now_ms: u64,
) -> Result<Vec<UndoOutcome>, Error> {
    let ledger = yard.store();
    let record = ledger.read(&plan.branch).ok();
    let settings = yard.approval_settings();
    let mut declared: Option<Vec<serde_json::Value>> = None;
    let mut outcomes = Vec::new();
    for id in chosen {
        let Some(item) = plan.items().find(|i| &i.entry.id == id) else {
            outcomes.push(UndoOutcome {
                id: id.clone(),
                state: ledger
                    .effects()
                    .effect(id)?
                    .map_or(EffectState::Failed, |e| e.state),
                detail: "not in the plan".into(),
            });
            continue;
        };
        let entry = &item.entry;
        let approval = ApprovalRecord {
            by: by.to_owned(),
            at_ms: now_ms,
            surface: surface.to_owned(),
            allowed: true,
            reason: Some("chosen in an undo plan".into()),
        };
        let settle = |from: &[EffectState], change: EffectMove| -> Result<UndoOutcome, Error> {
            let detail = change.detail.clone().unwrap_or_default();
            let done = ledger
                .effects()
                .move_effect(&entry.id, from, &change, now_ms)?;
            if let Some(done) = &done {
                note(yard, &done.branch, EffectActivity::of(done));
            }
            Ok(UndoOutcome {
                id: entry.id.clone(),
                state: done.map_or(entry.state, |d| d.state),
                detail,
            })
        };
        if entry.state == EffectState::Staged {
            let mut change = EffectMove::to(EffectState::Failed).detail(format!(
                "discarded by {by} in an undo before it was performed"
            ));
            change.undo_approval = Some(approval.clone());
            if let Some(staged) = &entry.staged {
                let _ = super::ask::answer(
                    yard,
                    &staged.ask,
                    &super::AskAnswer {
                        allow: false,
                        by: by.to_owned(),
                        surface: surface.to_owned(),
                        at_ms: now_ms,
                        reason: Some("discarded by undo".into()),
                    },
                );
            }
            outcomes.push(settle(&[EffectState::Staged], change)?);
            continue;
        }
        let Some(undo) = entry.undo.clone() else {
            outcomes.push(UndoOutcome {
                id: entry.id.clone(),
                state: entry.state,
                detail: "it has no undo".into(),
            });
            continue;
        };
        if entry.expired_at(now_ms) {
            outcomes.push(settle(
                &[EffectState::Confirmed, EffectState::UndoFailed],
                EffectMove::to(EffectState::Expired).detail(format!(
                    "its undo expired at {}",
                    clock(undo.deadline_ms.unwrap_or(0))
                )),
            )?);
            continue;
        }
        let Some(record) = record.as_ref() else {
            outcomes.push(UndoOutcome {
                id: entry.id.clone(),
                state: entry.state,
                detail: "its branch is gone, and with it the grant to undo under".into(),
            });
            continue;
        };
        let (gateway, token) = match crate::connectors::branch_token(yard, record, entry.turn) {
            Ok(pair) => pair,
            Err(why) => {
                outcomes.push(UndoOutcome {
                    id: entry.id.clone(),
                    state: entry.state,
                    detail: why,
                });
                continue;
            }
        };
        let client = Client::new(&gateway.url, token);
        // The inverse is an effect like any other: the policy may block it.
        let tool = format!("{}__{}", entry.connector, undo.operation);
        let tools = declared.get_or_insert_with(|| client.list_tools().unwrap_or_default());
        let decl = tools
            .iter()
            .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(tool.as_str()))
            .map(ToolDecl::read)
            .unwrap_or_default();
        let person = settings.person_for(&entry.subject);
        let seat = record.provision.as_ref().and_then(|p| p.approvals.as_ref());
        let layers = Layers {
            admin: settings.admin.as_ref(),
            seat,
            person,
            preset: None,
        };
        let subject = Subject::Operation {
            connector: &entry.connector,
            operation: &undo.operation,
            alias: decl.operation.as_deref(),
            class: decl.class.unwrap_or(EffectClass::Reversible),
            deletion: false,
        };
        if let Some(resolved) = resolve(&layers, &subject) {
            if resolved.approval == Approval::Block {
                outcomes.push(UndoOutcome {
                    id: entry.id.clone(),
                    state: entry.state,
                    detail: format!(
                        "the approval policy blocks {tool} ({})",
                        resolved.describe()
                    ),
                });
                continue;
            }
        }
        // A retried undo reuses its key, so the upstream does it once.
        let key = format!("{}-undo", entry.id);
        let result = client.call(
            &tool,
            &undo.arguments,
            &json!({"idempotency_key": key}),
            Some(&key),
        );
        let mut change = match result {
            Ok(called) if called.ok() => EffectMove::to(match undo.kind {
                UndoKind::Inverse => EffectState::Undone,
                UndoKind::Compensate => EffectState::Compensated,
            })
            .detail(format!("{}: {}", undo.operation, called.answer())),
            Ok(called) => EffectMove::to(EffectState::UndoFailed).detail(format!(
                "{} failed: {}",
                undo.operation,
                called.answer()
            )),
            Err(CallError::NotSent(why)) => EffectMove::to(EffectState::UndoFailed)
                .detail(format!("{} was not sent: {why}", undo.operation)),
            Err(CallError::Lost(why)) => EffectMove::to(EffectState::UndoFailed).detail(format!(
                "{}'s answer was lost ({why}); it may have happened, check upstream",
                undo.operation
            )),
        };
        change.undo_approval = Some(approval);
        outcomes.push(settle(
            &[EffectState::Confirmed, EffectState::UndoFailed],
            change,
        )?);
    }
    Ok(outcomes)
}
