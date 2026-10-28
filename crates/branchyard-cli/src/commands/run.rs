//! Starting work: `by run`, `by fan`, `by send`, `by steer`, `by fork` and `by reincarnate`.

use super::*;

pub fn run(env: &Env, target: &Target, prompt: &str, task: &TaskArgs, json: bool) -> Outcome {
    if let Target::Remote(remote) = target {
        if crate::fleet_cmd::is_routed(task) || task.kind.is_some() {
            return Err(crate::fleet_cmd::local_only());
        }
        // The issue's link lives in the prompt's header on a server.
        let (prompt, task, _) = crate::pr::issue_task(prompt, task, None)?;
        return match json {
            true => {
                let info = remote::run_json(env, remote, &prompt, &task)?;
                show(env, target, &info.name, true, false)?;
                branch_outcome(&info)
            }
            false => remote::run(env, remote, &prompt, &task),
        };
    }
    admit(
        env,
        task,
        &[run_harness(task)],
        task.harness.is_none(),
        json,
    )?;
    run_admitted(env, &open()?, prompt, task, None, json)
}

/// The harness a `by run` without `--harness` runs.
pub(crate) fn run_harness(task: &TaskArgs) -> String {
    task.harness.clone().unwrap_or_else(|| "claude-code".into())
}

/// The refusals of a local `by run` or `by fan` that need no repository
/// state, made before the yard is opened, which creates `.branchyard/`: a
/// refused run leaves the repository as it found it. Each harness must be
/// one a profile drives, routing its tools' permission requests unless
/// `--allow-unapproved-tools` says otherwise, installed and not logged out
/// (preflight.rs), and not near a 5-hour or weekly limit (docs/usage.md).
/// A routed task's harnesses are chosen later, by its router.
pub(crate) fn admit(
    env: &Env,
    task: &TaskArgs,
    harnesses: &[String],
    by_default: bool,
    json: bool,
) -> Outcome {
    if crate::fleet_cmd::is_routed(task) {
        return Ok(());
    }
    crate::preflight::check(env, task, harnesses, by_default, json)?;
    crate::usage::guard(harnesses)?;
    for id in harnesses {
        // As the engine's own refusal would have failed the run.
        match branchyard::admit_harness(id, task.unapproved_tools) {
            Ok(_) => {}
            Err(error) if json => return fail(true, &error),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// `by run`'s local half, in `yard`. `join` makes the branch another
/// attempt of that task, from its `main` (a task with a repository of its
/// own; docs/task-repos.md).
pub(crate) fn run_in(
    env: &Env,
    yard: &Yard,
    prompt: &str,
    task: &TaskArgs,
    join: Option<&str>,
) -> Outcome {
    admit(
        env,
        task,
        &[run_harness(task)],
        task.harness.is_none(),
        false,
    )?;
    run_admitted(env, yard, prompt, task, join, false)
}

/// [`run_in`] once [`admit`] let the run in, with `json` the branch as `by
/// show --json` prints it.
pub(crate) fn run_admitted(
    env: &Env,
    yard: &Yard,
    prompt: &str,
    task: &TaskArgs,
    join: Option<&str>,
    json: bool,
) -> Outcome {
    let yard = yard.clone();
    let (prompt, task, issue) = crate::pr::issue_task(prompt, task, Some(&yard))?;
    let task = &task;
    let workspace = workspace(env, &yard)?;
    let live = Live::start_to(env, task, task.delegate.is_some(), json, None);
    let mut options = TaskOptions {
        workspace,
        ..live.options(task)?
    };
    if let Some(id) = join {
        options.join_task = Some(id.to_owned());
        options.base = Some("main".into());
    }
    // `[fleet.<kind>] plan` and `goal_judge` (docs/plans-and-goals.md).
    let options = crate::plan_cmd::with_fleet(options, task, &prompt);
    // Routed (docs/fleet.md): the router picks the harness and fails over;
    // the branch that ends the chain is the one summarized.
    let result = match (crate::fleet_cmd::is_routed(task), task.kind) {
        (true, _) => match crate::fleet_cmd::routed(&yard, &prompt, &options, task, false, None) {
            Ok(mut routed) => Ok(routed.branches.remove(0)),
            Err(Failure::Sdk(error)) => Err(error),
            Err(other) => {
                live.console.finish();
                return Err(other);
            }
        },
        (false, Some(kind)) => yard.run_with_kind(&prompt, &options, kind),
        (false, None) => yard.task(prompt).options(options).run(),
    };
    if let (Ok(branch), Some(issue)) = (&result, &issue) {
        crate::pr::link_issue(branch, issue)?;
    }
    live.finish_as(env, result, json)
}

/// `by fan`'s routing and judging; see docs/fleet.md.
#[derive(Clone, Debug, Default)]
pub struct FanRoute {
    /// Branches to start when routed, instead of the entry's attempts.
    pub attempts: Option<u32>,
    /// Judge the attempts afterwards.
    pub judge: bool,
    /// Print JSON: the branches and the judgement.
    pub json: bool,
}

pub fn fan(
    env: &Env,
    target: &Target,
    prompt: &str,
    harnesses: Option<&[String]>,
    task: &TaskArgs,
    route: &FanRoute,
) -> Outcome {
    let routed = crate::fleet_cmd::is_routed(task);
    if harnesses.is_some() && task.auto {
        return Err(Report::usage(
            "--auto routes through the [fleet] table; it takes no --harness",
        )
        .next("drop --harness, or drop --auto")
        .into());
    }
    let harnesses = match (harnesses, routed) {
        (Some(harnesses), _) => harnesses,
        (None, true) => &[],
        (None, false) => {
            return Err(Report::usage(
                "fan needs the harnesses to run: --harness ID,ID,... or a [fleet] table to \
                 route by (--auto)",
            )
            .next("by fan \"<prompt>\" --harness ID,ID (by harnesses lists them)")
            .next("add a [fleet] table (docs/fleet.md) and pass --auto")
            .into())
        }
    };
    if let Target::Remote(remote) = target {
        if routed || task.kind.is_some() || route.judge {
            return Err(crate::fleet_cmd::local_only());
        }
        let (prompt, task, _) = crate::pr::issue_task(prompt, task, None)?;
        if route.json {
            let infos = remote::fan_json(env, remote, &prompt, harnesses, &task)?;
            return fan_json(&infos, None);
        }
        return remote::fan(env, remote, &prompt, harnesses, &task);
    }
    admit(env, task, harnesses, false, route.json)?;
    let yard = open()?;
    let (prompt, task, issue) = crate::pr::issue_task(prompt, task, Some(&yard))?;
    let (prompt, task) = (prompt.as_str(), &task);
    let workspace = workspace(env, &yard)?;
    let live = Live::start_to(env, task, true, route.json, None);
    let ids: Vec<&str> = harnesses.iter().map(String::as_str).collect();
    let options = TaskOptions {
        workspace,
        ..live.options(task)?
    };
    let options = crate::plan_cmd::with_fleet(options, task, prompt);
    // The judge's branch runs with the attempts' options.
    let judge_options = options.clone();
    let result = match routed {
        true => {
            match crate::fleet_cmd::routed(&yard, prompt, &options, task, true, route.attempts) {
                Ok(routed) => Ok(routed.branches),
                Err(Failure::Sdk(error)) => Err(error),
                Err(other) => {
                    live.console.finish();
                    return Err(other);
                }
            }
        }
        false => {
            let builder = yard.task(prompt).options(options);
            // Knowing the names up front lines the prefixes up from the
            // first line.
            if let Ok(names) = builder.planned_names(&ids) {
                live.console.reserve(&names);
            }
            builder.run_on(&ids)
        }
    };
    let branches = match result {
        Ok(branches) => branches,
        Err(error) => {
            live.console.finish();
            return Err(error.into());
        }
    };
    if let Some(issue) = &issue {
        for branch in &branches {
            crate::pr::link_issue(branch, issue)?;
        }
    }
    let descendants = wait_for_descendants(&branches.iter().collect::<Vec<_>>());
    live.console.finish();
    let descendants = descendants?.unwrap_or_default();
    let infos: Vec<&BranchInfo> = branches
        .iter()
        .map(Branch::info)
        .chain(descendants.iter())
        .collect();
    if route.json {
        let judgement = match route.judge {
            true => {
                let names: Vec<String> = branches.iter().map(|b| b.info().name.clone()).collect();
                Some(crate::fleet_cmd::judge_names(
                    &yard,
                    &names,
                    None,
                    None,
                    task.fleet.as_ref(),
                    &judge_options,
                )?)
            }
            false => None,
        };
        let owned: Vec<BranchInfo> = infos.iter().map(|info| (*info).clone()).collect();
        let judgement = judgement.map(|j| serde_json::to_value(j).unwrap_or_default());
        return fan_json(&owned, judgement);
    }
    let summary = fan_summary(env, &infos);
    if route.judge {
        let names: Vec<String> = branches.iter().map(|b| b.info().name.clone()).collect();
        let judgement = crate::fleet_cmd::judge_names(
            &yard,
            &names,
            None,
            None,
            task.fleet.as_ref(),
            &judge_options,
        )?;
        print(&format!(
            "\n{}",
            crate::fleet_cmd::judgement_table(&judgement, env.style())
        ))?;
        if judgement.pick.is_some() {
            print(&format!(
                "\n{}\n  by judge {} --pick\n",
                env.style().paint(Tone::Dim, "next"),
                names.join(" ")
            ))?;
        }
    }
    summary
}

/// `by fan --json`: the branches as `by ls --json` prints them, and the
/// judgement when `--judge` made one; the exit status is
/// [`fan_summary`]'s.
pub(crate) fn fan_json(infos: &[BranchInfo], judgement: Option<serde_json::Value>) -> Outcome {
    let mut value = serde_json::json!({
        "branches": infos.iter().map(json::branch).collect::<Vec<_>>(),
    });
    if let Some(judgement) = judgement {
        value["judgement"] = judgement;
    }
    print(&json::text(&value))?;
    match infos
        .iter()
        .all(|info| matches!(info.status, BranchStatus::Failed { .. }))
    {
        true => Err(Failure::Reported),
        false => Ok(()),
    }
}

/// The comparison closing `by fan`, and its exit status: failure only when
/// every branch failed.
pub fn fan_summary(env: &Env, infos: &[&BranchInfo]) -> Outcome {
    let style = env.style();
    let mut text = format!("\n{}", render::comparison_table(infos, style));
    let ready: Vec<&str> = infos
        .iter()
        .filter(|info| info.status == BranchStatus::Ready)
        .map(|info| info.name.as_str())
        .collect();
    if !ready.is_empty() {
        text.push_str(&format!("\n{}\n", style.paint(Tone::Dim, "next")));
        for name in &ready {
            text.push_str(&format!("  by diff {}\n", shell_quote(name)));
        }
        text.push_str("  by merge <branch>\n");
    }
    print(&text)?;
    if infos
        .iter()
        .all(|info| matches!(info.status, BranchStatus::Failed { .. }))
    {
        return Err(Failure::Reported);
    }
    Ok(())
}

/// What `by send` submits.
#[derive(Clone, Copy)]
pub enum Prompt<'a> {
    Text(&'a str),
    /// `--retry`: the prompt of the branch's last turn that was cut off.
    Retry,
}

pub fn send(
    env: &Env,
    target: &Target,
    branch: &str,
    prompt: Prompt<'_>,
    task: &TaskArgs,
    wait: bool,
    json: bool,
) -> Outcome {
    if let Some(delegate) = harness_delegate(json)? {
        if *task != TaskArgs::default() {
            return fail(
                json,
                &branchyard::Error::Denied(
                    "inside a harness, send takes only --wait and --json; the child keeps its \
                     own limits"
                        .into(),
                ),
            );
        }
        let sent = match prompt {
            Prompt::Text(prompt) => delegate.send(branch, prompt),
            Prompt::Retry => delegate.retry(branch),
        };
        if wait {
            let done = sent.and_then(|_| delegate.wait(branch, std::time::Duration::MAX));
            return emit(json, done, |i| render::inspection(i, env.style()));
        }
        return emit(json, sent, |sent| {
            format!("sent to {}; its turn is running\n", sent.name)
        });
    }
    if let Target::Remote(remote) = target {
        if json {
            let sent = remote::send_json(env, remote, branch, prompt, task);
            return emit(true, sent, |_| String::new());
        }
        return remote::send(env, remote, branch, prompt, task);
    }
    let retried: String;
    let prompt = match prompt {
        Prompt::Text(prompt) => prompt,
        Prompt::Retry => {
            retried = match open_yard().and_then(|yard| yard.branch(branch)?.retry_prompt()) {
                Ok(prompt) => prompt,
                Err(error) => return fail(json, &error),
            };
            &retried
        }
    };
    let branch = open()?.branch(branch)?;
    // The branch keeps its harness; check that one still starts (preflight.rs),
    // unless it runs a command of its own or somewhere else.
    if Provider::is_local(branch.provider()?.as_ref()) && branch.command()?.is_none() {
        let profile = branch.info().profile.clone();
        crate::preflight::check(env, task, &[profile], false, json)?;
    }
    if json {
        let live = Live::start_for(env, task, true, true, &branch)?;
        let options = live.options(task)?;
        let result = failed_over(branch.send(prompt, options.clone()), &options);
        live.console.finish();
        let branch = match result {
            Ok(branch) => branch,
            Err(error) => return fail(true, &error),
        };
        let descendants = wait_for_descendants(&[&branch]);
        descendants?;
        let sent = branchyard::Sent {
            name: branch.info().name.clone(),
            status: branch.info().status.clone(),
        };
        return print(&format!("{}\n", to_json(&sent)));
    }
    let delegating = task.delegate.is_some() || !branch.info().children.is_empty();
    let live = Live::start_for(env, task, delegating, false, &branch)?;
    let options = live.options(task)?;
    let result = failed_over(branch.send(prompt, options.clone()), &options);
    live.finish(env, result)
}

/// A routed branch whose turn failed for its harness goes on, on the next
/// candidate, when its route asked for failover (docs/fleet.md).
pub(crate) fn failed_over(
    result: Result<Branch, branchyard::Error>,
    options: &TaskOptions,
) -> Result<Branch, branchyard::Error> {
    let sent = result?;
    match sent.yard().failover(&sent.info().name, options)? {
        Some(next) => {
            eprintln!(
                "by: {}'s harness failed; the task went on as {}",
                sent.info().name,
                next.info().name
            );
            Ok(next)
        }
        None => Ok(sent),
    }
}

/// How long `by send --steer` waits for the engine running the turn to
/// deliver the input.
pub(crate) const STEER_WAIT: Duration = Duration::from_secs(10);

/// `by send --steer`: add `prompt` to `branch`'s running turn. Inside a
/// harness, as that harness's branch and only to a descendant; outside, with
/// your authority, recorded as from `sender` (the command that steered).
/// Waits for delivery; a refusal exits with failure.
pub fn steer(
    target: &Target,
    branch: &str,
    prompt: &str,
    task: &TaskArgs,
    sender: &str,
    json: bool,
) -> Outcome {
    if *task != TaskArgs::default() {
        return fail(
            json,
            &branchyard::Error::Denied(
                "send --steer takes only --json: the running turn keeps its own limits and \
                 permissions"
                    .into(),
            ),
        );
    }
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => delegate.steer(branch, prompt),
        (None, Target::Remote(remote)) => {
            remote
                .repo
                .steer(branch, prompt)
                .map_err(|e| branchyard::Error::Remote {
                    kind: e.code().unwrap_or("remote").to_owned(),
                    message: e.to_string(),
                    detail: None,
                })
        }
        (None, Target::Local) => open_yard().and_then(|yard| {
            let steer = yard.steer_as(branch, prompt, sender)?;
            yard.wait_steer(branch, steer.id, STEER_WAIT)
        }),
    };
    let steer = match result {
        Ok(steer) => steer,
        Err(error) => return fail(json, &error),
    };
    match &steer.state {
        branchyard::SteerState::Refused { reason } => {
            let report = Report::new(
                "steer_refused",
                format!("{branch}'s turn did not take the input: {reason}"),
            )
            .field("steer", serde_json::to_value(&steer).unwrap_or_default())
            .next(format!(
                "by send {} \"<prompt>\" once the turn ends (starts a new turn)",
                shell_quote(branch)
            ));
            emit_report(json, report)
        }
        _ if json => print(&format!("{}\n", to_json(&steer))),
        // The state first, in the words the delegate skill and the JSON
        // use, then what it means; the boundary in plain words.
        branchyard::SteerState::Pending => print(&format!(
            "pending: queued for {branch}'s running turn; its engine has not delivered it yet \
             (steer {})\n",
            steer.id
        )),
        branchyard::SteerState::Written => print(&format!(
            "written: sent to {branch}'s harness for its running turn; the harness has not \
             confirmed it yet (steer {})\n",
            steer.id
        )),
        branchyard::SteerState::Accepted => print(&format!(
            "accepted: it joined {branch}'s running turn, and the model reads it {} (steer {})\n",
            branchyard_harness::steer_boundary_text(steer.boundary.as_deref().unwrap_or("")),
            steer.id
        )),
    }
}

pub fn fork(
    env: &Env,
    target: &Target,
    branch: &str,
    prompt: &str,
    fresh_session: bool,
    task: &TaskArgs,
    json: bool,
) -> Outcome {
    if let Target::Remote(remote) = target {
        return match json {
            true => {
                let info =
                    remote::fork_json(env, remote, branch, prompt, fresh_session, None, task)?;
                show(env, target, &info.name, true, false)?;
                branch_outcome(&info)
            }
            false => remote::fork(env, remote, branch, prompt, fresh_session, task),
        };
    }
    let yard = open()?;
    let workspace = workspace(env, &yard)?;
    let branch = yard.branch(branch)?;
    let live = Live::start_for(env, task, task.delegate.is_some(), json, &branch)?;
    let options = TaskOptions {
        workspace,
        ..live.options(task)?
    };
    let result = branch.fork(prompt, fresh_session, options);
    live.finish_as(env, result, json)
}

/// `by fork BRANCH --at N`: a new branch from checkpoint N, saying how its
/// session continues.
pub fn fork_at(
    env: &Env,
    target: &Target,
    branch: &str,
    turn: u32,
    prompt: &str,
    task: &TaskArgs,
    json: bool,
) -> Outcome {
    if let Target::Remote(remote) = target {
        return match json {
            true => {
                let info = remote::fork_json(env, remote, branch, prompt, false, Some(turn), task)?;
                show(env, target, &info.name, true, false)?;
                branch_outcome(&info)
            }
            false => remote::fork_at(env, remote, branch, turn, prompt, task),
        };
    }
    let branch = open()?.branch(branch)?;
    let live = Live::start_for(env, task, task.delegate.is_some(), json, &branch)?;
    let result = branch.fork_at(turn, prompt, live.options(task)?);
    if let Ok(forked) = &result {
        crate::attempts::announce_fork(forked);
    }
    live.finish_as(env, result, json)
}

pub fn reincarnate(env: &Env, target: &Target, branch: &str, task: &TaskArgs) -> Outcome {
    if let Target::Remote(remote) = target {
        return remote::reincarnate(env, remote, branch, task);
    }
    let yard = open()?;
    let workspace = workspace(env, &yard)?;
    let branch = yard.branch(branch)?;
    let live = Live::start_for(env, task, task.delegate.is_some(), false, &branch)?;
    let options = TaskOptions {
        workspace,
        ..live.options(task)?
    };
    let result = branch.reincarnate(options);
    live.finish(env, result)
}
