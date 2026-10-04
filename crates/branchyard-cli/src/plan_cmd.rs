//! `by plan` (show, approve, reject a branch's plan) and the `--plan` and
//! `--goal` options of `by run` and `by fan`; see docs/plans-and-goals.md.

use std::path::PathBuf;

use branchyard::{Goal, JudgeSpec, PlanInfo, PlanPhase, TaskOptions};

use crate::args::{PlanAction, TaskArgs};
use crate::commands::{self, emit, harness_delegate, print, Env, Failure, Live, Outcome, Target};
use crate::render::{self, Tone};

/// Who a person is, as plan approvals and knowledge decisions name them.
pub fn person() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "a person".into())
}

/// The goal `--goal`, `--goal-rounds`, `--goal-judge` and
/// `--goal-judge-command` describe.
pub fn goal(task: &TaskArgs) -> Option<Goal> {
    let text = task.goal.as_ref()?;
    Some(Goal {
        text: text.clone(),
        rounds: task.goal_rounds.unwrap_or(branchyard::GOAL_DEFAULT_ROUNDS),
        judge: task.goal_judge.as_ref().map(|harness| JudgeSpec {
            harness: harness.clone(),
            model: None,
            effort: None,
            command: task.goal_judge_command.clone(),
            rubric: None,
        }),
        custom: None,
    })
}

/// `options` with what the configuration's `[fleet]` entry for the task's
/// kind adds when the run is not routed (a routed run gets it from the
/// router): `plan = true`, and `goal_judge` for a goal that names none.
pub fn with_fleet(mut options: TaskOptions, task: &TaskArgs, prompt: &str) -> TaskOptions {
    let Some(fleet) = &task.fleet else {
        return options;
    };
    let kind = task
        .kind
        .unwrap_or_else(|| branchyard::classify(prompt).kind);
    let Some((_, entry)) = fleet.entry(kind) else {
        return options;
    };
    options.plan |= entry.plan;
    if let (Some(goal), Some(judge)) = (options.goal.as_mut(), &entry.goal_judge) {
        if goal.judge.is_none() {
            goal.judge = Some(judge.clone());
        }
    }
    options
}

/// Open `text` in the editor and return what was saved. The file is kept
/// under `.branchyard/` (or the temporary directory remotely) until then.
pub fn edit_text(text: &str, editor: Option<&str>, stem: &str) -> Result<String, Failure> {
    let dir = match commands::open() {
        Ok(yard) => yard.root().join(".branchyard").join("edits"),
        Err(_) => std::env::temp_dir().join("branchyard-edits"),
    };
    std::fs::create_dir_all(&dir)?;
    let path: PathBuf = dir.join(format!("{stem}-{}.md", std::process::id()));
    std::fs::write(&path, text)?;
    let plan = crate::open::plan(&path, editor, &|name| std::env::var(name).ok())?;
    eprintln!("by: editing in {}", plan.argv[0]);
    let launched = crate::open::launch(&plan);
    let edited = std::fs::read_to_string(&path);
    branchyard_support::cleanup_file(&path);
    launched?;
    Ok(edited?)
}

fn phase_word(phase: PlanPhase) -> &'static str {
    match phase {
        PlanPhase::Planning => "being written (read-only)",
        PlanPhase::Awaiting => "awaiting approval",
        PlanPhase::Approved => "approved",
        PlanPhase::Rejected => "rejected",
    }
}

/// `by plan show`'s text.
pub fn render_plan(info: &PlanInfo, style: render::Style) -> String {
    let mut text = format!(
        "{} plan of {} (round {}): {}\n",
        style.paint(Tone::Bold, "plan"),
        info.branch,
        info.round,
        style.paint(
            match info.phase {
                PlanPhase::Awaiting => Tone::Yellow,
                PlanPhase::Approved => Tone::Green,
                PlanPhase::Rejected => Tone::Red,
                PlanPhase::Planning => Tone::Cyan,
            },
            phase_word(info.phase)
        )
    );
    match &info.plan {
        None => text.push_str("\n(no plan proposed yet)\n"),
        Some(plan) => {
            text.push('\n');
            text.push_str(plan.markdown.trim_end());
            text.push('\n');
            if let Some(tasks) = &plan.tasks {
                text.push_str(&format!("\n{}\n", style.paint(Tone::Bold, "tasks")));
                for (n, task) in tasks.iter().enumerate() {
                    match &task.detail {
                        Some(detail) => {
                            text.push_str(&format!("  {}. {} — {detail}\n", n + 1, task.title))
                        }
                        None => text.push_str(&format!("  {}. {}\n", n + 1, task.title)),
                    }
                }
            }
            if let Some(why) = &plan.tasks_error {
                text.push_str(&format!("\n(task list not used: {why})\n"));
            }
        }
    }
    if info.phase == PlanPhase::Awaiting {
        text.push_str(&format!(
            "\nnext: by plan approve {0} [--edit]  |  by plan reject {0} --reason \"...\" \
             [--replan]\n",
            info.branch
        ));
    }
    text
}

pub fn main(env: &Env, target: &Target, action: &PlanAction, json: bool) -> Outcome {
    match action {
        PlanAction::Show { branch } => show(env, target, branch, json),
        PlanAction::Approve {
            branch,
            edit,
            file,
            editor,
            task,
        } => {
            let edited = match (edit, file) {
                (_, Some(file)) => Some(
                    std::fs::read_to_string(file)
                        .map_err(|e| Failure::Message(format!("--file {file}: {e}")))?,
                ),
                (true, None) => {
                    let info = plan_of(target, branch)?;
                    let proposed = info.plan.map(|p| p.markdown).unwrap_or_default();
                    Some(edit_text(
                        &proposed,
                        editor.as_deref(),
                        &format!("plan-{branch}"),
                    )?)
                }
                (false, None) => None,
            };
            approve(env, target, branch, edited.as_deref(), task, json)
        }
        PlanAction::Reject {
            branch,
            reason,
            replan,
            task,
        } => reject(env, target, branch, reason.as_deref(), *replan, task, json),
    }
}

fn plan_of(target: &Target, branch: &str) -> Result<PlanInfo, Failure> {
    if let Some(delegate) = harness_delegate(false)? {
        // Inside a harness: a descendant's plan, read from its events.
        let page = delegate.events(branch, Some(0), 10_000)?;
        return branchyard::plan_from_events(branch, &page.events).ok_or_else(|| {
            Failure::Sdk(branchyard::Error::NoPlan(format!(
                "{branch} was not started with a plan"
            )))
        });
    }
    match target {
        Target::Local => Ok(commands::open()?.plan(branch)?),
        Target::Remote(remote) => Ok(remote.repo.plan(branch)?),
    }
}

fn show(env: &Env, target: &Target, branch: &str, json: bool) -> Outcome {
    let info = match plan_of(target, branch) {
        Ok(info) => info,
        Err(Failure::Sdk(error)) => return commands::fail(json, &error),
        Err(other) => return Err(other),
    };
    match json {
        true => print(&format!("{}\n", commands::to_json(&info))),
        false => print(&render_plan(&info, env.style())),
    }
}

fn approve(
    env: &Env,
    target: &Target,
    branch: &str,
    edited: Option<&str>,
    task: &TaskArgs,
    json: bool,
) -> Outcome {
    if let Some(delegate) = harness_delegate(json)? {
        if *task != TaskArgs::default() {
            return commands::fail(
                json,
                &branchyard::Error::Denied(
                    "inside a harness, plan approve takes only --edit, --file and --json; the \
                     child keeps its own limits"
                        .into(),
                ),
            );
        }
        return emit(json, delegate.approve_plan(branch, edited), |sent| {
            format!("approved {}'s plan; its turn is running\n", sent.name)
        });
    }
    if let Target::Remote(remote) = target {
        return crate::remote::plan_approve(env, remote, branch, edited, task, json);
    }
    let yard = commands::open()?;
    // Refused up front, before any console starts.
    if let Err(error) = yard.plan(branch) {
        return commands::fail(json, &error);
    }
    let provider = yard.branch(branch)?.provider()?;
    let live = Live::start_to(env, task, false, json, provider);
    let options = live.options(task)?;
    let result = yard.approve_plan(branch, edited, &person(), &options);
    finish(env, live, result, json)
}

fn reject(
    env: &Env,
    target: &Target,
    branch: &str,
    reason: Option<&str>,
    replan: bool,
    task: &TaskArgs,
    json: bool,
) -> Outcome {
    if let Some(delegate) = harness_delegate(json)? {
        return emit(
            json,
            delegate.reject_plan(branch, reason, replan),
            |sent| match replan {
                true => format!("rejected {}'s plan; it plans again\n", sent.name),
                false => format!("rejected {}'s plan; it ended\n", sent.name),
            },
        );
    }
    if let Target::Remote(remote) = target {
        return crate::remote::plan_reject(env, remote, branch, reason, replan, task, json);
    }
    let yard = commands::open()?;
    if let Err(error) = yard.plan(branch) {
        return commands::fail(json, &error);
    }
    if !replan {
        let ended = yard.reject_plan(branch, reason, false, &person(), &TaskOptions::default());
        return emit(json, ended.map(|b| b.info().clone()), |info| {
            format!(
                "rejected {}'s plan; it ended {}\n",
                info.name,
                render::status_text(&info.status).0
            )
        });
    }
    let provider = yard.branch(branch)?.provider()?;
    let live = Live::start_to(env, task, false, json, provider);
    let options = live.options(task)?;
    let result = yard.reject_plan(branch, reason, true, &person(), &options);
    finish(env, live, result, json)
}

/// The turn's summary, or with `json` the branch's name and status.
fn finish(
    env: &Env,
    live: Live,
    result: Result<branchyard::Branch, branchyard::Error>,
    json: bool,
) -> Outcome {
    if !json {
        return live.finish(env, result);
    }
    live.console.finish();
    let branch = match result {
        Ok(branch) => branch,
        Err(error) => return commands::fail(true, &error),
    };
    commands::wait_for_descendants(&[&branch])?;
    let sent = branchyard::Sent {
        name: branch.info().name.clone(),
        status: branch.info().status.clone(),
    };
    print(&format!("{}\n", commands::to_json(&sent)))
}

/// `by show`'s lines for a branch's plan and goal, from its events.
pub fn show_lines(name: &str, events: &[branchyard::RecordedEvent]) -> Vec<(&'static str, String)> {
    let mut lines = Vec::new();
    if let Some(plan) = branchyard::plan_from_events(name, events) {
        let tasks = plan
            .plan
            .as_ref()
            .and_then(|p| p.tasks.as_ref())
            .map(|t| format!(", {} task(s)", t.len()))
            .unwrap_or_default();
        lines.push((
            "plan",
            format!(
                "round {}, {}{tasks} (by plan show {name})",
                plan.round,
                phase_word(plan.phase)
            ),
        ));
    }
    if let Some(goal) = branchyard::goal_from_events(events) {
        let state = match goal.met {
            Some(true) => format!("met: {}", goal.evidence.join("; ")),
            Some(false) => format!("not met: missing {}", goal.missing.join("; ")),
            None if !goal.missing.is_empty() => {
                format!("not met yet: missing {}", goal.missing.join("; "))
            }
            None => "not checked yet".into(),
        };
        lines.push((
            "goal",
            format!(
                "{} — {state} ({} of {} follow-up turn(s){})",
                goal.goal,
                goal.used,
                goal.rounds,
                goal.by.map(|b| format!(", by {b}")).unwrap_or_default()
            ),
        ));
    }
    lines
}

/// `by show --json`'s `plan` and `goal`.
pub fn show_json(name: &str, events: &[branchyard::RecordedEvent], value: &mut serde_json::Value) {
    if let Some(plan) = branchyard::plan_from_events(name, events) {
        value["plan"] = serde_json::to_value(plan).unwrap_or_default();
    }
    if let Some(goal) = branchyard::goal_from_events(events) {
        value["goal"] = serde_json::to_value(goal).unwrap_or_default();
    }
}
