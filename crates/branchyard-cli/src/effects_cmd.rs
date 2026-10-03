//! `by approvals`, `by effects` and `by undo`, locally, on a server and
//! inside a harness, and the `[approvals]` settings a local yard runs
//! with. See docs/effects.md.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use branchyard::effects::undo::{self as plan_text, UndoOutcome, UndoPlan};
use branchyard::effects::{
    short_id, ApprovalAsk, ApprovalSettings, EffectActivity, EffectEntry, EffectState,
};
use branchyard::{Activity, RecordedEvent, Yard};
use branchyard_client::effects_api::{ApprovalAnswerRequest, UndoRequest};
use serde_json::{json, Value};

use crate::args::{ApprovalsAction, EffectsAction, UndoArgs};
use crate::commands::{self, emit, harness_delegate, print, Env, Failure, Outcome, Target};
use crate::plan_cmd::person;
use crate::setup_io;

/// Give `yard` your `[approvals]`: the person layer.
pub fn configure(yard: &Yard) -> Result<(), Failure> {
    if std::env::var_os("BRANCHYARD_BRANCH").is_some_and(|v| !v.is_empty()) {
        return Ok(());
    }
    let located = setup_io::locate(yard.root());
    if !located.project_exists && !located.user.is_file() {
        return Ok(());
    }
    let effective = setup_io::load(yard.root(), None).map_err(|e| {
        Failure::Message(format!(
            "{e}\n(fix it, or check it with `by config validate`)"
        ))
    })?;
    let person = effective
        .config
        .approvals
        .policy()
        .map_err(|e| Failure::Message(e.to_string()))?;
    if person.is_some() {
        yard.use_approvals(ApprovalSettings {
            person,
            ..ApprovalSettings::default()
        });
    }
    Ok(())
}

/// One line per ask.
fn approvals_table(asks: &[ApprovalAsk]) -> String {
    if asks.is_empty() {
        return "no approvals are waiting\n".into();
    }
    let mut out = format!(
        "{:<10} {:<16} {:>4}  {:<44} {}\n",
        "ID", "BRANCH", "TURN", "ASKS ABOUT", "STATE"
    );
    for ask in asks {
        let state = match &ask.answer {
            None => "waiting".to_owned(),
            Some(a) => format!(
                "{} by {} ({})",
                if a.allow { "allowed" } else { "denied" },
                a.by,
                a.surface
            ),
        };
        let why = ask
            .resolved
            .as_ref()
            .map(|r| format!(" [{}]", r.layer.as_str()))
            .unwrap_or_default();
        out.push_str(&format!(
            "{:<10} {:<16} {:>4}  {:<44} {}\n",
            short_id(&ask.id),
            ask.branch,
            ask.turn,
            format!("{}{why}", ask.about.describe()),
            state
        ));
    }
    out
}

fn answered_text(ask: &ApprovalAsk) -> String {
    match &ask.answer {
        Some(a) => format!(
            "{} approval {} ({}): {}\n",
            if a.allow { "allowed" } else { "denied" },
            short_id(&ask.id),
            ask.branch,
            ask.about.describe()
        ),
        None => format!("approval {} waits\n", short_id(&ask.id)),
    }
}

/// `by approvals [ls [--all] | allow ID | deny ID] [--reason TEXT]`.
pub fn approvals(target: &Target, action: Option<&ApprovalsAction>, json: bool) -> Outcome {
    let (verb, id, branch, reason, all) = match action {
        None => ("ls", None, None, None, false),
        Some(ApprovalsAction::Ls { all }) => ("ls", None, None, None, *all),
        Some(ApprovalsAction::Allow { id, branch, reason }) => (
            "allow",
            id.as_ref(),
            branch.as_deref(),
            reason.as_deref(),
            false,
        ),
        Some(ApprovalsAction::Deny { id, branch, reason }) => (
            "deny",
            id.as_ref(),
            branch.as_deref(),
            reason.as_deref(),
            false,
        ),
    };
    let allow = verb == "allow";
    // `--branch B`: the oldest approval waiting on B, as `by watch` asks.
    let found;
    let id = match (id, branch) {
        (Some(id), _) => Some(id),
        (None, None) => None,
        (None, Some(branch)) => {
            let waiting = match target {
                Target::Remote(remote) => remote.repo.approvals(false)?,
                Target::Local => commands::open()?.approvals(true)?,
            };
            found = waiting
                .into_iter()
                .find(|a| a.branch == branch)
                .map(|a| a.id)
                .ok_or_else(|| Failure::Message(format!("no approval waits on {branch}")))?;
            Some(&found)
        }
    };
    if let (Some(id), Some(delegate)) = (id, harness_delegate(json)?) {
        // A delegating parent answers its descendants' asks.
        return emit(
            json,
            delegate.answer_approval(id, allow, reason),
            answered_text,
        );
    }
    match target {
        Target::Remote(remote) => match id {
            None => {
                let asks = remote.repo.approvals(all)?;
                match json {
                    true => print(&format!("{}\n", commands::to_json(&asks))),
                    false => print(&approvals_table(&asks)),
                }
            }
            Some(id) => {
                let request = ApprovalAnswerRequest {
                    reason: reason.map(str::to_owned),
                    surface: None,
                };
                let ask = remote.repo.answer_approval(id, allow, &request)?;
                match json {
                    true => print(&format!("{}\n", commands::to_json(&ask))),
                    false => print(&answered_text(&ask)),
                }
            }
        },
        Target::Local => {
            let yard = commands::open()?;
            match id {
                None => emit(json, yard.approvals(!all), |asks| approvals_table(asks)),
                Some(id) => emit(
                    json,
                    yard.answer_approval(id, allow, &person(), "cli", reason),
                    answered_text,
                ),
            }
        }
    }
}

/// The ledger as a table.
fn effects_table(entries: &[EffectEntry]) -> String {
    if entries.is_empty() {
        return "the effect ledger is empty\n".into();
    }
    let mut out = format!(
        "{:<10} {:<16} {:>4}  {:<10} {:<20} {:<13} {:<12} {}\n",
        "ID", "BRANCH", "TURN", "CONNECTOR", "OPERATION", "CLASS", "STATE", "UNDO"
    );
    for e in entries {
        let undo = match &e.undo {
            Some(u) => match u.deadline_ms {
                Some(d) => format!("{} until {}", u.operation, plan_text::clock(d)),
                None => u.operation.clone(),
            },
            None => "-".into(),
        };
        out.push_str(&format!(
            "{:<10} {:<16} {:>4}  {:<10} {:<20} {:<13} {:<12} {}\n",
            short_id(&e.id),
            e.branch,
            e.turn,
            e.connector,
            e.operation,
            e.class.as_str(),
            e.state.as_str(),
            undo
        ));
    }
    out
}

fn entry_text(entry: &EffectEntry) -> String {
    let mut out = format!(
        "effect {} ({})\n  {} {} on {} turn {}, for {}\n  class {}, state {}\n",
        short_id(&entry.id),
        entry.id,
        entry.connector,
        entry.operation,
        entry.branch,
        entry.turn,
        entry.subject,
        entry.class,
        entry.state
    );
    if let Some(operation) = &entry.operation_id {
        out.push_str(&format!("  operation {operation}\n"));
    }
    if let Some(undo) = &entry.undo {
        out.push_str(&format!(
            "  undo: {} {}{}\n",
            undo.tool,
            undo.arguments,
            undo.deadline_ms
                .map(|d| format!(" until {}", plan_text::clock(d)))
                .unwrap_or_default()
        ));
    }
    if let Some(compensate) = &entry.compensate {
        out.push_str(&format!(
            "  compensate: {} {}{}\n",
            compensate.tool,
            compensate.arguments,
            compensate
                .deadline_ms
                .map(|d| format!(" until {}", plan_text::clock(d)))
                .unwrap_or_default()
        ));
    }
    if let Some(why) = &entry.undo_unavailable {
        out.push_str(&format!("  no undo: {why}\n"));
    }
    if let Some(draft) = entry.staged.as_ref().and_then(|s| s.draft.as_ref()) {
        out.push_str(&format!(
            "  draft {} ({}): promote {}, discard {}\n",
            draft.handle,
            draft.draft_operation,
            draft
                .promote
                .as_ref()
                .map_or("unavailable", |p| p.tool.as_str()),
            draft
                .discard
                .as_ref()
                .map_or("unavailable", |d| d.tool.as_str())
        ));
    }
    if let Some(approval) = &entry.approval {
        out.push_str(&format!(
            "  approved by {} ({})\n",
            approval.by, approval.surface
        ));
    }
    if let Some(detail) = &entry.detail {
        out.push_str(&format!("  {detail}\n"));
    }
    out
}

/// `by effects [--branch B] [show ID | promote ID | reconcile]`.
pub fn effects(
    target: &Target,
    branch: Option<&str>,
    action: Option<&EffectsAction>,
    json: bool,
) -> Outcome {
    match (target, action) {
        (Target::Remote(remote), None) => {
            let entries = remote.repo.effects(branch)?;
            match json {
                true => print(&format!("{}\n", commands::to_json(&entries))),
                false => print(&effects_table(&entries)),
            }
        }
        (Target::Remote(remote), Some(EffectsAction::Show { id })) => {
            let detail = remote.repo.effect(id)?;
            match json {
                true => print(&format!("{}\n", commands::to_json(&detail))),
                false => print(&entry_text(&detail.entry)),
            }
        }
        (Target::Remote(remote), Some(EffectsAction::Promote { id })) => {
            let entry = remote.repo.promote_effect(id)?;
            match json {
                true => print(&format!("{}\n", commands::to_json(&entry))),
                false => print(&entry_text(&entry)),
            }
        }
        (Target::Remote(remote), Some(EffectsAction::Reconcile)) => {
            let report = remote.repo.reconcile_effects()?;
            match json {
                true => print(&format!("{}\n", commands::to_json(&report))),
                false => print(&reconciled_text(&report)),
            }
        }
        (Target::Local, action) => {
            let yard = commands::open()?;
            match action {
                None => emit(json, yard.effects(branch), |e| effects_table(e)),
                Some(EffectsAction::Show { id }) => {
                    let detail = yard.effect(id).and_then(|entry| {
                        let events = yard.effect_history(&entry.id)?;
                        Ok(json!({"entry": entry, "events": events}))
                    });
                    emit(json, detail, |d: &Value| {
                        serde_json::from_value::<EffectEntry>(d["entry"].clone())
                            .map(|e| entry_text(&e))
                            .unwrap_or_default()
                    })
                }
                Some(EffectsAction::Promote { id }) => {
                    emit(json, yard.promote_effect(id, &person(), "cli"), entry_text)
                }
                Some(EffectsAction::Reconcile) => {
                    emit(json, yard.reconcile_effects(), reconciled_text)
                }
            }
        }
    }
}

fn reconciled_text(report: &branchyard::effects::reconcile::Reconciled) -> String {
    let mut out = String::new();
    for (id, state) in &report.settled {
        out.push_str(&format!("{} {state}\n", short_id(id)));
    }
    for (id, why) in &report.unknown {
        out.push_str(&format!("{} still unknown: {why}\n", short_id(id)));
    }
    if out.is_empty() {
        out.push_str("nothing to reconcile\n");
    }
    out
}

fn outcomes_text(outcomes: &[UndoOutcome]) -> String {
    let mut out = String::new();
    for outcome in outcomes {
        out.push_str(&format!(
            "{} {}: {}\n",
            short_id(&outcome.id),
            outcome.state,
            outcome.detail
        ));
    }
    out
}

/// Ask which upstream effects to undo: empty for every reversible one (and
/// staged calls), `all` adds the compensable ones, `none`, or ids.
fn choose(env: &Env, plan: &UndoPlan) -> Result<Vec<String>, Failure> {
    if !(env.stdin_tty && env.stderr_tty) {
        return Err(Failure::Message(
            "by undo asks which upstream effects to undo; pass --yes for every reversible one, \
             or --only ID... (or run on a terminal)"
                .into(),
        ));
    }
    eprint!("Undo which upstream effects? [all reversible] (all, none, or ids) ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim();
    Ok(match answer {
        "" => plan.default_choice(),
        "none" => Vec::new(),
        "all" => plan
            .reversible
            .iter()
            .chain(&plan.compensable)
            .chain(&plan.staged)
            .map(|i| i.entry.id.clone())
            .collect(),
        ids => ids
            .split([' ', ','])
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    })
}

/// `by undo BRANCH [--to N] [--plan] [--only ID...] [--yes] [--json]`.
pub fn undo(env: &Env, target: &Target, args: &UndoArgs) -> Outcome {
    let (branch, plan_only, only, yes, json) =
        (&args.branch, args.plan, &args.only, args.yes, args.json);
    let to = args.to.unwrap_or(0);
    if let Target::Remote(remote) = target {
        let plan = remote.repo.undo_plan(branch, to)?;
        if plan_only {
            return match json {
                true => print(&format!("{}\n", commands::to_json(&plan))),
                false => print(&plan_text::render(&plan, branch, false)),
            };
        }
        if !json {
            eprint!("{}", plan_text::render(&plan, branch, false));
            eprintln!(
                "by: a server undoes upstream effects only; rewind files where the branch runs"
            );
        }
        let chosen = match (only.is_empty(), yes) {
            (false, _) => only.to_vec(),
            (true, true) => Vec::new(),
            (true, false) => choose(env, &plan)?,
        };
        if chosen.is_empty() && only.is_empty() && !yes {
            return print("nothing undone\n");
        }
        let report = remote
            .repo
            .undo(branch, &UndoRequest { to, only: chosen })?;
        return match json {
            true => print(&format!("{}\n", commands::to_json(&report))),
            false => print(&outcomes_text(&report.outcomes)),
        };
    }
    let yard = commands::open()?;
    let handle = yard.branch(branch)?;
    let plan = yard.undo_plan(branch, to)?;
    if plan_only {
        return match json {
            true => print(&format!("{}\n", commands::to_json(&plan))),
            false => print(&plan_text::render(&plan, branch, true)),
        };
    }
    if !json {
        eprint!("{}", plan_text::render(&plan, branch, true));
    }
    let chosen = match (only.is_empty(), yes) {
        (false, _) => only.to_vec(),
        (true, true) => plan.default_choice(),
        (true, false) => choose(env, &plan)?,
    };
    // Files and conversation first: exact, and kept to rewind forward.
    let rewound = handle.rewind(to)?;
    let outcomes = yard.undo_effects(&plan, &chosen, &person(), "cli")?;
    if json {
        return print(&format!(
            "{}\n",
            commands::to_json(&json!({"rewound": rewound, "plan": plan, "outcomes": outcomes}))
        ));
    }
    let mut text = format!(
        "rewound {branch} to checkpoint {to}; the next turn {}\n",
        rewound.session.describe()
    );
    text.push_str(&outcomes_text(&outcomes));
    if outcomes.is_empty() {
        text.push_str("no upstream effect undone\n");
    }
    print(&text)
}

/// `by merge --promote-effects`: perform the branch's staged effects
/// before merging it, as approving each would.
pub fn promote_before_merge(target: &Target, branch: &str) -> Outcome {
    let promoted = match target {
        Target::Remote(remote) => remote
            .repo
            .effects(Some(branch))?
            .into_iter()
            .filter(|e| e.state == EffectState::Staged)
            .map(|e| remote.repo.promote_effect(&e.id))
            .collect::<Result<Vec<_>, _>>()?,
        Target::Local => commands::open()?.promote_staged(branch, &person(), "cli")?,
    };
    for entry in &promoted {
        eprintln!(
            "by: performed staged effect {} ({} {}): {}",
            short_id(&entry.id),
            entry.connector,
            entry.operation,
            entry.state
        );
    }
    Ok(())
}

/// What a rewind to `to` leaves upstream, for `by rewind` to say before it
/// acts: the plan, or `None` when nothing happened upstream after `to`.
pub fn rewind_note(yard: &Yard, branch: &str, to: u32) -> Option<String> {
    let plan = yard.undo_plan(branch, to).ok()?;
    if plan.is_empty() {
        return None;
    }
    Some(format!(
        "{}A rewind restores files and the conversation only; undo upstream effects with: by \
         undo {branch} --to {to}\n",
        plan_text::render(&plan, branch, true)
    ))
}

/// The branch's effects and waiting approvals from its events, for `by
/// show`: as JSON and as one line. `None` when it has neither.
pub fn summary(events: &[RecordedEvent]) -> Option<(Value, String)> {
    let mut states: BTreeMap<String, EffectState> = BTreeMap::new();
    let mut asked: Vec<String> = Vec::new();
    for event in events {
        let Activity::Effect(activity) = &event.activity else {
            continue;
        };
        match activity.as_ref() {
            EffectActivity::Entry { id, state, .. } => {
                states.insert(id.clone(), *state);
            }
            EffectActivity::Asked { ask, .. } => asked.push(ask.clone()),
            EffectActivity::Answered { ask, .. } => asked.retain(|a| a != ask),
            _ => {}
        }
    }
    if states.is_empty() && asked.is_empty() {
        return None;
    }
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    for state in states.values() {
        *counts.entry(state.as_str()).or_default() += 1;
    }
    let mut parts: Vec<String> = counts.iter().map(|(s, n)| format!("{n} {s}")).collect();
    if !asked.is_empty() {
        parts.push(format!("{} approval(s) waiting", asked.len()));
    }
    let line = format!("{} in the ledger ({})", states.len(), parts.join(", "));
    Some((
        json!({"total": states.len(), "states": counts, "approvals_waiting": asked}),
        line,
    ))
}
