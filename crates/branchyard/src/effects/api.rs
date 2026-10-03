//! The ledger and approvals on [`Yard`]: what every surface (`by`, the
//! server, the companion, a delegating parent) calls.

use std::sync::Arc;

use serde_json::{json, Value};

use super::ask::{self, note};
use super::mcp::{CallError, Client};
use super::proxy::finish_move;
use super::reconcile::Reconciled;
use super::undo::{UndoOutcome, UndoPlan};
use super::{
    find_ask, find_effect, ApprovalAsk, ApprovalRecord, ApprovalSettings, AskAbout, AskAnswer,
    EffectActivity, EffectEntry, EffectEvent, EffectMove, EffectState,
};
use crate::state::now_ms;
use crate::{Error, Yard};

impl Yard {
    /// Resolve approvals with `settings`: an administrator's locked policy
    /// and the people's (`docs/effects.md#approvals`). Replaces any set
    /// before. Shared by every clone of this `Yard`.
    pub fn use_approvals(&self, settings: ApprovalSettings) {
        *self.hub.approvals.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(settings);
    }

    /// The settings set with [`Yard::use_approvals`].
    pub fn approval_settings(&self) -> Arc<ApprovalSettings> {
        self.hub
            .approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The ledger, or one branch's, oldest first. Entries outlive their
    /// branch.
    pub fn effects(&self, branch: Option<&str>) -> Result<Vec<EffectEntry>, Error> {
        self.store().effects().effects(branch)
    }

    /// One entry, by its id or the end of it.
    pub fn effect(&self, id: &str) -> Result<EffectEntry, Error> {
        find_effect(self.store().effects(), id)
    }

    /// An entry's events: what the current state is projected from.
    pub fn effect_history(&self, id: &str) -> Result<Vec<EffectEvent>, Error> {
        let entry = self.effect(id)?;
        self.store().effects().effect_events(&entry.id)
    }

    /// Every ask, or only those waiting, oldest first.
    pub fn approvals(&self, pending_only: bool) -> Result<Vec<ApprovalAsk>, Error> {
        self.store().effects().asks(pending_only)
    }

    /// One ask, by its id or the end of it.
    pub fn approval(&self, id: &str) -> Result<ApprovalAsk, Error> {
        find_ask(self.store().effects(), id)
    }

    /// Answer an ask as `by` through `surface` (`cli`, `watch`,
    /// `companion`, `api`, `parent`, `sdk`). The turn waiting on it goes
    /// on; an ask to perform a staged effect performs it when allowed, and
    /// discards it when denied. Refused when it was answered already.
    pub fn answer_approval(
        &self,
        id: &str,
        allow: bool,
        by: &str,
        surface: &str,
        reason: Option<&str>,
    ) -> Result<ApprovalAsk, Error> {
        let found = self.approval(id)?;
        if let Some(answer) = &found.answer {
            return Err(Error::Denied(format!(
                "approval {} was already {} by {} ({})",
                found.id,
                if answer.allow { "allowed" } else { "denied" },
                answer.by,
                answer.surface
            )));
        }
        let answer = AskAnswer {
            allow,
            by: by.to_owned(),
            surface: surface.to_owned(),
            at_ms: now_ms(),
            reason: reason.map(str::to_owned),
        };
        let ask = ask::answer(self, &found.id, &answer)?;
        let won = ask.answer.as_ref() == Some(&answer);
        if !won {
            return Err(Error::Denied(format!(
                "approval {} was answered by someone else first",
                ask.id
            )));
        }
        if let (AskAbout::Promote { .. }, Some(effect)) = (&ask.about, &ask.effect) {
            match allow {
                true => {
                    self.promote_effect(effect, by, surface)?;
                }
                false => {
                    let mut change = EffectMove::to(EffectState::Failed).detail(format!(
                        "not approved: denied by {by} ({surface}){}; it never happened",
                        reason.map(|r| format!(": {r}")).unwrap_or_default()
                    ));
                    change.approval = Some(answer.record());
                    if let Some(done) = self.store().effects().move_effect(
                        effect,
                        &[EffectState::Staged],
                        &change,
                        now_ms(),
                    )? {
                        note(self, &done.branch, EffectActivity::of(&done));
                    }
                }
            }
        }
        self.approval(&ask.id)
    }

    /// Perform a staged effect for real, approved by `by` through
    /// `surface`: promote its draft, or make the call held in the outbox.
    /// The entry is `begun` before the call, as any effect is.
    pub fn promote_effect(&self, id: &str, by: &str, surface: &str) -> Result<EffectEntry, Error> {
        let entry = self.effect(id)?;
        if entry.state != EffectState::Staged {
            return Err(Error::Denied(format!(
                "effect {} is {}, not staged",
                entry.id, entry.state
            )));
        }
        let staged = entry
            .staged
            .clone()
            .ok_or_else(|| Error::State(format!("staged effect {} names no approval", entry.id)))?;
        let held = self.store().effects().ask(&staged.ask)?;
        let request: Value = held
            .as_ref()
            .and_then(|a| a.request.clone())
            .unwrap_or(json!({}));
        let record = self.store().read(&entry.branch)?;
        let (gateway, token) =
            crate::connectors::branch_token(self, &record, entry.turn).map_err(Error::Denied)?;
        let now = now_ms();
        let approval = ApprovalRecord {
            by: by.to_owned(),
            at_ms: now,
            surface: surface.to_owned(),
            allowed: true,
            reason: Some("promoted".into()),
        };
        // The approval's own record, when it still waits.
        if held.as_ref().is_some_and(ApprovalAsk::pending) {
            let _ = ask::answer(
                self,
                &staged.ask,
                &AskAnswer {
                    allow: true,
                    by: by.to_owned(),
                    surface: surface.to_owned(),
                    at_ms: now,
                    reason: Some("promoted".into()),
                },
            );
        }
        let mut begin = EffectMove::to(EffectState::Begun);
        begin.approval = Some(approval);
        let Some(begun) =
            self.store()
                .effects()
                .move_effect(&entry.id, &[EffectState::Staged], &begin, now)?
        else {
            return Err(Error::Denied(format!(
                "effect {} is no longer staged",
                entry.id
            )));
        };
        note(self, &begun.branch, EffectActivity::of(&begun));
        let tool = request["name"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}__{}", entry.connector, entry.operation));
        let arguments = request.get("arguments").cloned().unwrap_or(json!({}));
        let mut meta = json!({"idempotency_key": entry.id});
        if let Some(draft) = &staged.draft {
            meta["promote"] = json!(draft);
        }
        let change = match Client::new(&gateway.url, token).call(
            &tool,
            &arguments,
            &meta,
            Some(&entry.id),
        ) {
            Ok(called) => finish_move(&called),
            Err(CallError::NotSent(why)) => {
                EffectMove::to(EffectState::Failed).detail(format!("not sent: {why}"))
            }
            Err(CallError::Lost(why)) => EffectMove::to(EffectState::Unknown)
                .detail(format!("the gateway's answer was lost: {why}")),
        };
        let done = self
            .store()
            .effects()
            .move_effect(&entry.id, &[EffectState::Begun], &change, now_ms())?
            .unwrap_or(begun);
        note(self, &done.branch, EffectActivity::of(&done));
        Ok(done)
    }

    /// Promote every staged effect of `branch`, as accepting it does with
    /// `--promote-effects`.
    pub fn promote_staged(
        &self,
        branch: &str,
        by: &str,
        surface: &str,
    ) -> Result<Vec<EffectEntry>, Error> {
        let staged: Vec<EffectEntry> = self
            .effects(Some(branch))?
            .into_iter()
            .filter(|e| e.state == EffectState::Staged)
            .collect();
        staged
            .iter()
            .map(|e| self.promote_effect(&e.id, by, surface))
            .collect()
    }

    /// Reconcile the ledger now: `begun` entries whose turn ended become
    /// `unknown`, `unknown` ones are looked up upstream, expired undos are
    /// marked. See [`super::reconcile`].
    pub fn reconcile_effects(&self) -> Result<Reconciled, Error> {
        super::reconcile::now(self)
    }

    /// [`Yard::reconcile_effects`] as of `now_ms` (milliseconds since the
    /// Unix epoch), for deadlines.
    pub fn reconcile_effects_at(&self, now_ms: u64) -> Result<Reconciled, Error> {
        super::reconcile::run(self, now_ms)
    }

    /// What undoing `branch` to checkpoint `to` (0: everything) would do
    /// upstream, as of `now_ms`.
    pub fn undo_plan_at(&self, branch: &str, to: u32, now_ms: u64) -> Result<UndoPlan, Error> {
        self.store().read(branch)?;
        super::undo::plan(self, branch, to, now_ms)
    }

    /// [`Yard::undo_plan_at`] now.
    pub fn undo_plan(&self, branch: &str, to: u32) -> Result<UndoPlan, Error> {
        self.undo_plan_at(branch, to, now_ms())
    }

    /// Perform the inverses of `chosen` (entry ids from `plan`, or the end
    /// of them), as of `now_ms`, approved by `by` through `surface`. The
    /// branch's files are not touched: rewind it for those.
    pub fn undo_effects_at(
        &self,
        plan: &UndoPlan,
        chosen: &[String],
        by: &str,
        surface: &str,
        now_ms: u64,
    ) -> Result<Vec<UndoOutcome>, Error> {
        let mut ids = Vec::new();
        for id in chosen {
            let wanted = id.to_ascii_uppercase();
            let matches: Vec<&str> = plan
                .items()
                .map(|i| i.entry.id.as_str())
                .filter(|full| *full == id || full.ends_with(&wanted))
                .collect();
            match matches.as_slice() {
                [one] => ids.push((*one).to_owned()),
                [] => {
                    return Err(Error::Denied(format!(
                        "{id} is not an effect in the plan for {}",
                        plan.branch
                    )))
                }
                _ => return Err(Error::Denied(format!("{id} names several effects"))),
            }
        }
        super::undo::execute(self, plan, &ids, by, surface, now_ms)
    }

    /// [`Yard::undo_effects_at`] now.
    pub fn undo_effects(
        &self,
        plan: &UndoPlan,
        chosen: &[String],
        by: &str,
        surface: &str,
    ) -> Result<Vec<UndoOutcome>, Error> {
        self.undo_effects_at(plan, chosen, by, surface, now_ms())
    }
}
