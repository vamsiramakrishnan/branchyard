//! Asking a person: an [`ApprovalAsk`] in the store, recorded on the
//! branch, escalated to a delegating parent's inbox, and waited for within
//! the turn's budget. Every surface answers through
//! [`crate::Yard::answer_approval`], which writes the answer once.

use std::time::Duration;

use serde_json::Value;

use super::{ulid, ApprovalAsk, AskAbout, AskAnswer, EffectActivity, Resolved};
use crate::state::now_ms;
use crate::{Activity, Error, RecordedEvent, TaskOptions, Yard};

/// What to ask.
pub(crate) struct AskSpec {
    pub branch: String,
    pub turn: u32,
    pub subject: String,
    pub about: AskAbout,
    pub effect: Option<String>,
    pub resolved: Resolved,
    pub request: Option<Value>,
    pub deadline_ms: Option<u64>,
}

/// Record `activity` on `branch`, best effort: the ledger and the asks are
/// the record of truth, the event is how `by log` and `by watch` see it.
pub(crate) fn note(yard: &Yard, branch: &str, activity: EffectActivity) {
    let event = RecordedEvent {
        at_ms: now_ms(),
        activity: Activity::Effect(Box::new(activity)),
    };
    let _ = yard.store().append(branch, &event, None);
}

/// Store the ask, record it on the branch, and escalate it to a delegating
/// parent's inbox.
pub(crate) fn open(yard: &Yard, spec: AskSpec) -> Result<ApprovalAsk, Error> {
    let ask = ApprovalAsk {
        id: ulid(now_ms())?,
        branch: spec.branch,
        turn: spec.turn,
        subject: spec.subject,
        about: spec.about,
        effect: spec.effect,
        resolved: Some(spec.resolved.clone()),
        request: spec.request,
        created_ms: now_ms(),
        deadline_ms: spec.deadline_ms,
        answer: None,
    };
    yard.store().effects().put_ask(&ask)?;
    yard.store().notify();
    note(
        yard,
        &ask.branch,
        EffectActivity::Asked {
            ask: ask.id.clone(),
            about: ask.about.clone(),
            resolved: spec.resolved,
        },
    );
    escalate(yard, &ask);
    Ok(ask)
}

/// A delegated child's ask goes to its parent's inbox too, which may
/// answer it (`by approvals allow ID` in its shell, or the
/// `answer_approval` tool).
fn escalate(yard: &Yard, ask: &ApprovalAsk) {
    let Ok(record) = yard.store().read(&ask.branch) else {
        return;
    };
    if record.info.depth == 0 || record.info.parent.is_none() {
        return;
    }
    let text = format!(
        "Approval {} waits: may I {}? Allow it with `by approvals allow {}` (or the \
         answer_approval tool), or deny it with `by approvals deny {} --reason \"...\"`.",
        ask.id,
        ask.about.describe(),
        ask.id,
        ask.id
    );
    if let Ok(delegate) = crate::delegation::trusted(yard, &ask.branch, TaskOptions::default()) {
        let _ = delegate.escalate(&text);
    }
}

/// Write `answer` to the ask unless it is answered already, and record it
/// on the branch. Returns the ask as answered, by whichever answer won.
pub(crate) fn answer(yard: &Yard, id: &str, answer: &AskAnswer) -> Result<ApprovalAsk, Error> {
    let (ask, answered) = yard
        .store()
        .effects()
        .answer_ask(id, answer)?
        .ok_or_else(|| Error::Denied(format!("no approval {id}")))?;
    if answered {
        yard.store().notify();
        note(
            yard,
            &ask.branch,
            EffectActivity::Answered {
                ask: ask.id.clone(),
                allow: answer.allow,
                by: answer.by.clone(),
                surface: answer.surface.clone(),
                reason: answer.reason.clone(),
            },
        );
    }
    Ok(ask)
}

/// Wait for `ask`'s answer until its deadline, or until `stop` says the
/// turn is ending; then it is answered `expired` (denied).
pub(crate) fn wait(
    yard: &Yard,
    ask: &ApprovalAsk,
    stop: &dyn Fn() -> bool,
) -> Result<AskAnswer, Error> {
    let store = yard.store();
    loop {
        let now = now_ms();
        let expired = ask.deadline_ms.is_some_and(|d| now >= d);
        if expired || stop() {
            let why = match expired {
                true => "the turn's budget ran out before an answer",
                false => "the turn ended before an answer",
            };
            let settled = answer(
                yard,
                &ask.id,
                &AskAnswer {
                    allow: false,
                    by: "branchyard".into(),
                    surface: "expired".into(),
                    at_ms: now,
                    reason: Some(why.into()),
                },
            )?;
            return Ok(settled.answer.expect("answered above"));
        }
        let left = ask.deadline_ms.map_or(Duration::from_millis(500), |d| {
            Duration::from_millis(d.saturating_sub(now).min(500))
        });
        let found = store.wait(left, || {
            Ok(store.effects().ask(&ask.id)?.and_then(|a| a.answer))
        })?;
        if let Some(answer) = found {
            return Ok(answer);
        }
    }
}
