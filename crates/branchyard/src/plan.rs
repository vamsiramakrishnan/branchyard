//! Plan approval: a branch whose first turn plans under a read-only
//! policy, then waits for a person (or, for a delegated child, its parent)
//! to approve, edit or reject the plan before anything changes. See
//! `docs/plans-and-goals.md`.
//!
//! The plan's phase lives in the branch's record. While it is `planning`,
//! every turn runs under [`read_only`] whatever policy the caller passes
//! (narrowed further by a delegating parent's denials), and a turn that
//! completes leaves the branch [`BranchStatus::AwaitingPlanApproval`] with
//! its last reply as the plan. Approval sends the plan (possibly edited) as
//! the next turn under the caller's own policy; rejection ends the branch,
//! or with `replan`, runs another read-only planning turn with the reason.

use serde::{Deserialize, Serialize};

use crate::engine::{self, Turn};
use crate::record::Recorder;
use crate::state::Record;
use crate::{
    Activity, Branch, BranchStatus, Error, Event, Policy, RecordedEvent, TaskOptions, Yard,
};
use branchyard_support::time::now_ms;

/// The first line of a planning turn's prompt.
pub const PLAN_HEADER: &str = "[branchyard plan mode]";
/// The first line of the turn an approval starts.
pub const APPROVED_HEADER: &str = "[branchyard plan approved]";

/// Tools a planning turn may use: reading and searching, by the names the
/// harnesses give them. Everything else is denied.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "LS",
    "NotebookRead",
    "TodoWrite",
    "read_file",
    "read_many_files",
    "list_directory",
    "glob",
    "search_file_content",
    "grep",
    "view",
];

/// The policy every planning turn runs under: [`READ_ONLY_TOOLS`]
/// allowed, every other request (writes, edits, commands) denied: the
/// `read-only` permission preset ([`crate::PolicyPreset::ReadOnly`]).
pub fn read_only() -> Policy {
    crate::PolicyPreset::ReadOnly.policy()
}

/// Where a branch's plan is.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanPhase {
    /// Its turns run read-only, to produce a plan.
    Planning,
    /// A plan waits for approval; the branch is `awaiting_plan_approval`.
    Awaiting,
    /// The plan was approved and sent as a turn.
    Approved,
    /// The plan was rejected, and the branch ended.
    Rejected,
}

/// One step of a plan's task list.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanTask {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A proposed plan: the planning turn's reply, and the task list it ended
/// with, if it gave one that parses.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The reply, in Markdown.
    pub markdown: String,
    /// The fenced JSON task list, when the reply ended with a valid one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Vec<PlanTask>>,
    /// Why a task list in the reply was not used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks_error: Option<String>,
    /// The branch's turn that produced it.
    pub turn: u32,
    /// From 1; one more for each re-plan.
    pub round: u32,
}

/// The plan's state, kept in the branch's record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlanState {
    pub phase: PlanPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    /// Planning rounds started, from 1.
    pub round: u32,
}

/// A branch's plan as [`Yard::plan`] reports it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanInfo {
    pub branch: String,
    pub phase: PlanPhase,
    /// The latest plan proposed, if any.
    pub plan: Option<Plan>,
    pub round: u32,
}

/// Plan activity on a branch, recorded as [`Activity::Plan`]. Serialized
/// as an object tagged by `type`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanActivity {
    /// The branch plans before it changes anything: round `round` runs
    /// read-only.
    Planning { round: u32 },
    /// A planning turn proposed this plan; the branch awaits approval.
    Proposed(Plan),
    /// Someone approved the plan, as proposed or edited, and it was sent as
    /// the next turn.
    Approved {
        by: String,
        edited: bool,
        round: u32,
    },
    /// Someone rejected the plan; with `replan`, another planning round
    /// runs with the reason.
    Rejected {
        by: String,
        reason: Option<String>,
        replan: bool,
        round: u32,
    },
    /// The plan of this delegated child was escalated to its parent's
    /// inbox for approval, as message `message`.
    Escalated { to: String, message: u64 },
}

impl PlanActivity {
    /// One line for logs.
    pub fn describe(&self) -> String {
        match self {
            PlanActivity::Planning { round } => {
                format!("planning (round {round}, read-only)")
            }
            PlanActivity::Proposed(plan) => {
                let tasks = match (&plan.tasks, &plan.tasks_error) {
                    (Some(tasks), _) => format!(", {} task(s)", tasks.len()),
                    (None, Some(why)) => format!(", task list not used: {why}"),
                    (None, None) => String::new(),
                };
                let first = plan
                    .markdown
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("(empty)");
                let first: String = first.chars().take(80).collect();
                format!(
                    "plan proposed (round {}{tasks}), awaiting approval: {first}",
                    plan.round
                )
            }
            PlanActivity::Approved { by, edited, round } => format!(
                "plan round {round} approved{} by {by}",
                if *edited { " with edits" } else { "" }
            ),
            PlanActivity::Rejected {
                by,
                reason,
                replan,
                round,
            } => format!(
                "plan round {round} rejected by {by}{}{}",
                reason
                    .as_ref()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default(),
                if *replan { "; planning again" } else { "" }
            ),
            PlanActivity::Escalated { to, message } => {
                format!("plan escalated to {to} for approval (message #{message})")
            }
        }
    }
}

/// The prompt of a branch's first, planning turn.
pub fn planning_prompt(task: &str) -> String {
    format!(
        "{PLAN_HEADER}\nPlan this task before anything changes. This turn is read-only: every \
         tool that would write a file or run a command is denied, so read and search the code, \
         then propose. Change nothing.\n\nAnswer with the plan in Markdown: the approach, the \
         files to change and how, the risks, and how to check the result. If you can, end with \
         one fenced JSON block listing the steps, of this shape:\n```json\n{{\"tasks\": \
         [{{\"title\": \"...\", \"detail\": \"...\"}}]}}\n```\n\n## Task\n{task}"
    )
}

/// The prompt of a planning turn after a rejection.
pub fn replan_prompt(reason: Option<&str>) -> String {
    let reason = reason
        .filter(|r| !r.trim().is_empty())
        .unwrap_or("no reason given");
    format!(
        "{PLAN_HEADER}\nYour plan was not approved: {reason}\n\nRevise it. This turn is still \
         read-only: change nothing. Answer with the whole revised plan in Markdown, ending if \
         you can with the fenced JSON task list as before."
    )
}

/// The prompt of the turn an approval starts.
pub fn approved_prompt(plan: &str, edited: bool) -> String {
    let who = match edited {
        true => "approved, with edits by a person",
        false => "approved",
    };
    format!(
        "{APPROVED_HEADER}\nYour plan was {who}. Carry it out now, as written below; your tools \
         are no longer read-only.\n\n## The approved plan\n{plan}"
    )
}

/// The task list at the end of `markdown`: the last fenced block, when it
/// is a JSON object with exactly `tasks`, each with a non-empty `title`
/// and an optional `detail`. `Ok(None)` when there is no fenced JSON.
pub fn parse_tasks(markdown: &str) -> Result<Option<Vec<PlanTask>>, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct List {
        tasks: Vec<PlanTask>,
    }
    let Some(open) = markdown.rfind("```json") else {
        return Ok(None);
    };
    let rest = &markdown[open + "```json".len()..];
    let close = rest
        .find("```")
        .ok_or("the task list's fence is not closed")?;
    let list: List =
        serde_json::from_str(rest[..close].trim()).map_err(|e| format!("not a task list: {e}"))?;
    if let Some(i) = list.tasks.iter().position(|t| t.title.trim().is_empty()) {
        return Err(format!("tasks[{i}] has no title"));
    }
    Ok(Some(list.tasks))
}

/// Whether a branch's turns now plan, read-only.
pub(crate) fn planning(record: &Record) -> bool {
    record
        .plan
        .as_ref()
        .is_some_and(|p| p.phase == PlanPhase::Planning)
}

/// The policy a turn of `record` runs under: [`read_only`] while it plans,
/// else the caller's.
pub(crate) fn policy_for(record: &Record, policy: &Policy) -> Policy {
    match planning(record) {
        true => read_only(),
        false => policy.clone(),
    }
}

/// Every message the last turn sent, whole.
fn reply(events: &[RecordedEvent]) -> String {
    let start = events
        .iter()
        .rposition(|e| matches!(e.activity, Activity::Prompt(_)))
        .map_or(0, |i| i + 1);
    events[start..]
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Harness(Event::MessageDelta { text, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Settle a planning turn that just ended, in `conclude`: one that
/// completed proposes its reply as the plan and leaves the branch awaiting
/// approval. Any other ending leaves it planning, as its status says.
#[allow(clippy::expect_used)] // ratchet: branchyard
pub(crate) fn conclude(
    yard: &Yard,
    record: &mut Record,
    recorder: &mut Recorder,
    changed: bool,
) -> Result<(), Error> {
    if !planning(record) {
        return Ok(());
    }
    if !matches!(
        record.info.status,
        BranchStatus::Ready | BranchStatus::NoChanges
    ) {
        return Ok(());
    }
    let events = crate::record::read(&yard.store(), &record.info.name)?;
    let markdown = reply(&events).trim().to_owned();
    let (tasks, tasks_error) = match parse_tasks(&markdown) {
        Ok(tasks) => (tasks, None),
        Err(why) => (None, Some(why)),
    };
    let state = record.plan.as_mut().expect("planning checked above");
    let plan = Plan {
        markdown,
        tasks,
        tasks_error,
        turn: record.info.turns,
        round: state.round,
    };
    if changed {
        recorder.record(Activity::Warning(
            "the planning turn changed files although its tools were read-only; the changes \
             are kept in the candidate"
                .into(),
        ))?;
    }
    if plan.markdown.is_empty() {
        recorder.record(Activity::Warning(
            "the planning turn replied nothing; reject the plan with --replan to ask again".into(),
        ))?;
    }
    state.phase = PlanPhase::Awaiting;
    state.plan = Some(plan.clone());
    recorder.record(Activity::Plan(Box::new(PlanActivity::Proposed(plan))))?;
    record.info.status = BranchStatus::AwaitingPlanApproval;
    Ok(())
}

/// After a turn's lease is released: a delegated child whose plan now
/// awaits approval escalates it to its parent's inbox, once per round.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard
pub(crate) fn settled(yard: &Yard, name: &str) {
    let store = yard.store();
    let Ok(record) = store.read(name) else {
        return;
    };
    let (Some(parent), Some(state)) = (&record.info.parent, &record.plan) else {
        return;
    };
    if record.info.depth == 0
        || state.phase != PlanPhase::Awaiting
        || record.info.status != BranchStatus::AwaitingPlanApproval
    {
        return;
    }
    let events = crate::record::read(&store, name).unwrap_or_default();
    let round = state.round;
    if escalated_round(&events) == Some(round) {
        return;
    }
    let plan = state
        .plan
        .as_ref()
        .map(|p| p.markdown.chars().take(4000).collect::<String>())
        .unwrap_or_default();
    let text = format!(
        "My plan (round {round}) awaits your approval before I change anything. Approve it \
         with `by plan approve {name}` (or the approve_plan tool), or reject it with `by plan \
         reject {name} --reason \"...\" [--replan]`.\n\n{plan}"
    );
    let Ok(delegate) = crate::delegation::trusted(yard, name, TaskOptions::default()) else {
        return;
    };
    if let Ok(message) = delegate.escalate(&text) {
        if let Ok(mut recorder) = Recorder::open(&store, name, None) {
            let _ = recorder.record(Activity::Plan(Box::new(PlanActivity::Escalated {
                to: parent.clone(),
                message: message.id,
            })));
        }
    }
}

fn last_proposed_round(events: &[RecordedEvent]) -> Option<u32> {
    events.iter().rev().find_map(|e| match &e.activity {
        Activity::Plan(a) => match a.as_ref() {
            PlanActivity::Proposed(plan) => Some(plan.round),
            _ => None,
        },
        _ => None,
    })
}

/// The round of the last escalation: the round of the last proposal before
/// it.
fn escalated_round(events: &[RecordedEvent]) -> Option<u32> {
    let at = events.iter().rposition(|e| {
        matches!(&e.activity, Activity::Plan(a) if matches!(a.as_ref(), PlanActivity::Escalated { .. }))
    })?;
    last_proposed_round(&events[..at])
}

/// `name`'s plan.
pub(crate) fn info(yard: &Yard, name: &str) -> Result<PlanInfo, Error> {
    let record = yard.store().read(name)?;
    let state = record.plan.ok_or_else(|| no_plan(name))?;
    Ok(PlanInfo {
        branch: name.to_owned(),
        phase: state.phase,
        plan: state.plan,
        round: state.round,
    })
}

fn no_plan(name: &str) -> Error {
    Error::NoPlan(format!("{name} was not started with a plan (--plan)"))
}

fn not_awaiting(name: &str, record: &Record) -> Error {
    match &record.plan {
        None => no_plan(name),
        Some(state) => Error::NoPlan(format!(
            "{name} has no plan awaiting approval: its plan is {}",
            match state.phase {
                PlanPhase::Planning => "still being written",
                PlanPhase::Awaiting => "awaiting, but the branch is not",
                PlanPhase::Approved => "already approved",
                PlanPhase::Rejected => "rejected",
            }
        )),
    }
}

/// Check that `name` awaits plan approval.
fn awaiting(yard: &Yard, name: &str) -> Result<Record, Error> {
    let record = yard.store().read(name)?;
    let ok = record.info.status == BranchStatus::AwaitingPlanApproval
        && record
            .plan
            .as_ref()
            .is_some_and(|p| p.phase == PlanPhase::Awaiting);
    match ok {
        true => Ok(record),
        false => Err(not_awaiting(name, &record)),
    }
}

/// Mark `name` running its next turn for an approval or a re-plan,
/// recording `activity` and moving the plan to `phase`: the prepared send
/// and its prompt.
pub(crate) fn prepare(
    yard: &Yard,
    name: &str,
    options: &TaskOptions,
    activity: PlanActivity,
    phase: PlanPhase,
    prompt: String,
) -> Result<(crate::run::Prepared, String), Error> {
    awaiting(yard, name)?;
    let mut prepared = crate::run::prepare_send_with(yard, name, options, true, true)?;
    let store = yard.store();
    let fence = prepared.lease.fence().clone();
    // Recheck under the lease: another approval may have won the race.
    let current = store.read(name)?;
    if !current
        .plan
        .as_ref()
        .is_some_and(|p| p.phase == PlanPhase::Awaiting)
    {
        return Err(not_awaiting(name, &current));
    }
    let state = prepared.record.plan.as_mut().ok_or_else(|| no_plan(name))?;
    state.phase = phase;
    if phase == PlanPhase::Planning {
        state.round += 1;
    }
    store.write_fenced(&prepared.record, &fence)?;
    let mut recorder = Recorder::fenced(&store, &fence, options.observer.clone());
    recorder.record(Activity::Plan(Box::new(activity)))?;
    if phase == PlanPhase::Planning {
        let round = prepared.record.plan.as_ref().map_or(1, |p| p.round);
        recorder.record(Activity::Plan(Box::new(PlanActivity::Planning { round })))?;
    }
    Ok((prepared, prompt))
}

/// The approval's prepared turn: the plan, or `edited` in its place.
pub(crate) fn prepare_approval(
    yard: &Yard,
    name: &str,
    edited: Option<&str>,
    by: &str,
    options: &TaskOptions,
) -> Result<(crate::run::Prepared, String), Error> {
    let record = awaiting(yard, name)?;
    let state = record.plan.as_ref().ok_or_else(|| no_plan(name))?;
    let proposed = state
        .plan
        .as_ref()
        .map(|p| p.markdown.clone())
        .unwrap_or_default();
    let edited = edited
        .map(str::trim)
        .filter(|e| !e.is_empty() && *e != proposed.trim());
    let text = edited.map(str::to_owned).unwrap_or(proposed);
    if text.trim().is_empty() {
        return Err(Error::NoPlan(format!(
            "{name}'s plan is empty; approve it with an edit, or reject it with --replan"
        )));
    }
    prepare(
        yard,
        name,
        options,
        PlanActivity::Approved {
            by: by.to_owned(),
            edited: edited.is_some(),
            round: state.round,
        },
        PlanPhase::Approved,
        approved_prompt(&text, edited.is_some()),
    )
}

/// A rejection's prepared re-planning turn.
pub(crate) fn prepare_replan(
    yard: &Yard,
    name: &str,
    reason: Option<&str>,
    by: &str,
    options: &TaskOptions,
) -> Result<(crate::run::Prepared, String), Error> {
    let record = awaiting(yard, name)?;
    let round = record.plan.as_ref().map_or(1, |p| p.round);
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    prepare(
        yard,
        name,
        options,
        PlanActivity::Rejected {
            by: by.to_owned(),
            reason: reason.map(str::to_owned),
            replan: true,
            round,
        },
        PlanPhase::Planning,
        replan_prompt(reason),
    )
}

/// Approve `name`'s plan and run it as the next turn, under `options`.
pub(crate) fn approve(
    yard: &Yard,
    name: &str,
    edited: Option<&str>,
    by: &str,
    options: &TaskOptions,
) -> Result<Branch, Error> {
    let (prepared, prompt) = prepare_approval(yard, name, edited, by, options)?;
    let branch = execute(yard, prepared, &prompt, options)?;
    crate::goal::pursue(yard, branch, options)
}

/// Reject `name`'s plan: end the branch, or with `replan`, run another
/// read-only planning turn with the reason.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard
pub(crate) fn reject(
    yard: &Yard,
    name: &str,
    reason: Option<&str>,
    replan: bool,
    by: &str,
    options: &TaskOptions,
) -> Result<Branch, Error> {
    let record = awaiting(yard, name)?;
    let round = record.plan.as_ref().map_or(1, |p| p.round);
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    let activity = PlanActivity::Rejected {
        by: by.to_owned(),
        reason: reason.map(str::to_owned),
        replan,
        round,
    };
    if replan {
        let (prepared, prompt) = prepare_replan(yard, name, reason, by, options)?;
        return execute(yard, prepared, &prompt, options);
    }
    let (mut record, lease) = crate::ops::hold(yard, name)?;
    if !record
        .plan
        .as_ref()
        .is_some_and(|p| p.phase == PlanPhase::Awaiting)
    {
        return Err(not_awaiting(name, &record));
    }
    if let Some(state) = record.plan.as_mut() {
        state.phase = PlanPhase::Rejected;
    }
    record.info.status = BranchStatus::Failed {
        reason: format!(
            "its plan was rejected by {by}{}",
            reason.map(|r| format!(": {r}")).unwrap_or_default()
        ),
    };
    let store = yard.store();
    let mut recorder = Recorder::fenced(&store, lease.fence(), options.observer.clone());
    recorder.record(Activity::Plan(Box::new(activity)))?;
    recorder.finish(lease, &record)?;
    let _ = crate::fleet::observe(yard, name, None);
    yard.branch(name)
}

/// Run a prepared turn.
pub(crate) fn execute(
    yard: &Yard,
    prepared: crate::run::Prepared,
    prompt: &str,
    options: &TaskOptions,
) -> Result<Branch, Error> {
    let prompt = prepared.prompt(prompt);
    engine::execute(
        Turn {
            yard,
            record: prepared.record,
            profile: prepared.profile,
            command: prepared.command,
            mode: prepared.mode,
            prompt: &prompt,
            options,
            fork_source: None,
            note: prepared.note,
            sandbox: Default::default(),
        },
        prepared.lease,
    )
}

/// Start `record`, newly created with its first turn's lease, planning:
/// its plan state and a [`PlanActivity::Planning`] event. Refused for a
/// profile whose tools Branchyard's policy never sees, since its planning
/// could not be held read-only.
pub(crate) fn begin(
    yard: &Yard,
    record: &mut Record,
    fence: &crate::state::Fence,
    profile: &branchyard_harness::profiles::Profile,
) -> Result<(), Error> {
    check_profile(profile)?;
    record.plan = Some(PlanState {
        phase: PlanPhase::Planning,
        plan: None,
        round: 1,
    });
    let store = yard.store();
    store.write_fenced(record, fence)?;
    store.append(
        &record.info.name,
        &RecordedEvent {
            at_ms: now_ms(),
            activity: Activity::Plan(Box::new(PlanActivity::Planning { round: 1 })),
        },
        Some(fence),
    )?;
    Ok(())
}

/// Refuse planning with a profile whose tool requests never reach the
/// policy.
pub(crate) fn check_profile(profile: &branchyard_harness::profiles::Profile) -> Result<(), Error> {
    match profile.driver().capabilities().tool_approvals {
        true => Ok(()),
        false => Err(Error::Unsupported(format!(
            "plan mode needs a harness whose tool requests Branchyard's policy answers, so the \
             planning turn can be held read-only; {} runs its tools unapproved",
            profile.id
        ))),
    }
}

/// The plan described by `events`: the latest proposal and the phase the
/// events imply, for a surface that has events but no record (`by show`
/// remotely).
pub fn from_events(branch: &str, events: &[RecordedEvent]) -> Option<PlanInfo> {
    let mut info: Option<PlanInfo> = None;
    for event in events {
        let Activity::Plan(activity) = &event.activity else {
            continue;
        };
        let current = info.get_or_insert_with(|| PlanInfo {
            branch: branch.to_owned(),
            phase: PlanPhase::Planning,
            plan: None,
            round: 1,
        });
        match activity.as_ref() {
            PlanActivity::Planning { round } => {
                current.phase = PlanPhase::Planning;
                current.round = *round;
            }
            PlanActivity::Proposed(plan) => {
                current.phase = PlanPhase::Awaiting;
                current.round = plan.round;
                current.plan = Some(plan.clone());
            }
            PlanActivity::Approved { .. } => current.phase = PlanPhase::Approved,
            PlanActivity::Rejected { replan: false, .. } => current.phase = PlanPhase::Rejected,
            PlanActivity::Rejected { replan: true, .. } | PlanActivity::Escalated { .. } => {}
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_lists_parse_strictly_and_are_optional() {
        let plan = "1. Read\n2. Write\n```json\n{\"tasks\": [{\"title\": \"Read\"}, \
                    {\"title\": \"Write\", \"detail\": \"a.rs\"}]}\n```\n";
        let tasks = parse_tasks(plan).unwrap().unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].detail.as_deref(), Some("a.rs"));
        assert_eq!(parse_tasks("just prose").unwrap(), None);
        assert!(parse_tasks("```json\n{\"tasks\": [{\"title\": \"\"}]}\n```").is_err());
        assert!(parse_tasks("```json\n{\"steps\": []}\n```").is_err());
        assert!(parse_tasks("```json\n{\"tasks\": []").is_err());
    }

    #[test]
    fn the_read_only_policy_allows_reading_and_denies_the_rest() {
        let policy = read_only();
        let request = |tool: &str| crate::PermissionRequest {
            key: crate::PermissionKey("k".into()),
            tool: tool.into(),
            input: serde_json::json!({}),
        };
        for tool in ["Read", "Grep", "read_file"] {
            assert_eq!(
                policy.decide("b", &request(tool)),
                crate::PermissionDecision::Allow
            );
        }
        for tool in ["Write", "Edit", "Bash", "write marker", "shell"] {
            assert!(matches!(
                policy.decide("b", &request(tool)),
                crate::PermissionDecision::Deny { .. }
            ));
        }
    }
}
