//! A harness acting through its branch: `by spawn`, `inspect`, `events`, `check`,
//! `integrate`, `wait`, `cancel`, `discard`, `ask`, `report`, the inbox, the graph,
//! `delegate` and `brief`.

use super::*;

/// Relay a frozen result using the caller's existing authority.
pub fn handoff(target: &Target, args: &args::HandoffArgs) -> Outcome {
    let expected = args
        .expected_version
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| {
            Report::usage(format!(
                "--expected-version is not a source version: {error}"
            ))
        })?;
    let request = branchyard::HandoffRequest {
        from: args.source.clone(),
        to: args.to.clone(),
        artifacts: args.artifacts.clone(),
        note: args.note.clone(),
        expected,
    };
    let result = match (harness_delegate(args.json)?, target) {
        (Some(delegate), _) => delegate.handoff(request),
        (None, Target::Local) => open_yard()?.handoff(request),
        (None, Target::Remote(remote)) => remote.repo.handoff(&request).map_err(remote::sdk_error),
    };
    emit(args.json, result, |message| {
        format!(
            "handoff #{} queued for {}; no turn was started\n",
            message.id, args.to
        )
    })
}

pub fn spawn(env: &Env, target: &Target, prompt: &str, args: &SpawnArgs) -> Outcome {
    let json = args.json;
    let result = match result_contract(
        args.result_schema.as_deref(),
        args.result_schema_file.as_deref(),
        args.result_required,
        args.result_deliverable,
        args.result_capture.as_ref(),
    ) {
        Ok(result) => result,
        Err(error) => return fail(json, &error),
    };
    // A child's issue link lives in its prompt's header: a harness has no
    // yard of its own to record it in.
    let (prompt, task, _) = crate::pr::issue_task(prompt, &args.task, None)?;
    let args = &SpawnArgs {
        task,
        ..args.clone()
    };
    let (prompt, task) = (prompt.as_str(), &args.task);
    let request = Spawn {
        prompt: prompt.to_owned(),
        harness: task.harness.clone(),
        name: task.name.clone(),
        base: task.base.clone(),
        budget: Budget {
            max_usd: task.budget_usd,
            max_turns: task.max_turns,
            max_duration: task.max_duration,
            stall_after: task.stall_after,
            stall_action: task.stall_action,
            hold_cap: None,
            follow_up_grace: None,
        },
        check: task.check.clone(),
        max_depth: args.max_depth,
        max_children: args.max_children,
        harnesses: args.harnesses.clone(),
        deny: args.deny.clone(),
        seat: args.seat.clone(),
        depends_on: args.depends_on.clone(),
        after: args.after,
        bindings: args.bindings.clone(),
        connectors: (!args.connectors.is_empty()).then(|| args.connectors.clone()),
        plan: args.plan,
        model: args.model.clone(),
        result,
        why: args.why.clone(),
        owns: args.owns.clone(),
        land: args.land,
        review: args.review.as_deref().cloned(),
    };
    if let Some(delegate) = harness_delegate(json)? {
        if args.parent.is_some()
            || task.permissions != args::Permissions::Unset
            || task.unapproved_tools
        {
            let error = branchyard::Error::Denied(
                "inside a harness, the parent is the harness's own branch and the child \
                 inherits its policy and approval routing; drop --parent, --yes, --ask, \
                 --permissions and --allow-unapproved-tools (by spawn --help says why)"
                    .into(),
            );
            return fail(json, &error);
        }
        let spawned = match delegate.spawn(request) {
            Ok(spawned) => spawned,
            Err(error) => return fail(json, &error),
        };
        if !args.wait {
            return emit(json, Ok(spawned), |s| {
                let on = match &s.model {
                    Some(model) => format!(
                        "{} with model {model}",
                        render::harness_label(&s.harness, &s.profile)
                    ),
                    None => render::harness_label(&s.harness, &s.profile),
                };
                match &s.status {
                    branchyard::BranchStatus::Waiting => format!(
                        "created {} on {on}, waiting for {}\n",
                        s.name,
                        match (&s.waiting_for, s.depends_on.is_empty()) {
                            (Some(slot), true) => slot.clone(),
                            _ => s.depends_on.join(", "),
                        }
                    ),
                    branchyard::BranchStatus::Blocked { reason } => {
                        format!("created {}, blocked: {reason}\n", s.name)
                    }
                    _ => format!(
                        "spawned {} on {on} from {}\n{}",
                        s.name,
                        short(&s.base),
                        check_note(s)
                    ),
                }
            });
        }
        let done = delegate.wait(&spawned.name, std::time::Duration::MAX);
        return emit(json, done, |i| render::inspection(i, env.style()));
    }
    // Outside a harness the child runs on this process's threads, so the
    // command waits for it.
    let parent = match required_outside(args.parent.clone(), "spawn --parent") {
        Ok(parent) => parent,
        Err(error) => return fail(json, &error),
    };
    if let Target::Remote(remote) = target {
        let result = remote::spawn(env, remote, &parent, prompt, args, request.result.clone());
        return emit(json, result, |i| render::inspection(i, env.style()));
    }
    let live = Live::start_to(env, task, true, json, None);
    let result = (|| {
        // One yard, so the wait sees the child's thread.
        let parent = open_yard()?.branch(&parent)?;
        let delegate = parent.delegate(live.options(task)?)?;
        let spawned = delegate.spawn(request)?;
        parent.wait_subtree()?;
        delegate.inspect(&spawned.name)
    })();
    live.console.finish();
    emit(json, result, |i| render::inspection(i, env.style()))
}

pub fn inspect(env: &Env, target: &Target, branch: Option<String>, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => {
            let branch = branch.unwrap_or_else(|| delegate.branch().to_owned());
            delegate.inspect(&branch)
        }
        (None, Target::Remote(remote)) => {
            required_outside(branch, "inspect").and_then(|b| remote::inspect(remote, &b))
        }
        (None, Target::Local) => required_outside(branch, "inspect")
            .and_then(|b| as_user(&b, TaskOptions::default())?.inspect(&b)),
    };
    emit(json, result, |i| render::inspection(i, env.style()))
}

pub fn events(
    env: &Env,
    target: &Target,
    branch: Option<String>,
    cursor: Option<usize>,
    limit: Option<usize>,
    json: bool,
) -> Outcome {
    let limit = limit.unwrap_or(50);
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => {
            let branch = branch.unwrap_or_else(|| delegate.branch().to_owned());
            delegate.events(&branch, cursor, limit)
        }
        (None, Target::Remote(remote)) => required_outside(branch, "events")
            .and_then(|b| remote::events(remote, &b, cursor, limit)),
        (None, Target::Local) => required_outside(branch, "events")
            .and_then(|b| as_user(&b, TaskOptions::default())?.events(&b, cursor, limit)),
    };
    emit(json, result, |page| {
        format!(
            "{}next cursor: {} of {}\n",
            render::log_text(&page.events, env.style()),
            page.next_cursor,
            page.total
        )
    })
}

/// Run a branch's check on its current work as integrating it would:
/// inside a harness, on itself or a descendant; otherwise on any branch,
/// with yours, or the server's with `--remote`. Exits 1 when the work
/// would not get past its check.
pub fn check(env: &Env, target: &Target, branch: Option<String>, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => {
            let branch = branch.unwrap_or_else(|| delegate.branch().to_owned());
            delegate.check(&branch)
        }
        (None, Target::Remote(remote)) => {
            required_outside(branch, "check").and_then(|b| remote::check(remote, &b))
        }
        (None, Target::Local) => required_outside(branch, "check")
            .and_then(|b| as_user(&b, TaskOptions::default())?.check(&b)),
    };
    let passed = result.as_ref().is_ok_and(branchyard::CheckReport::passed);
    emit(json, result, |r| render::check_report(r, env.style()))?;
    match passed {
        true => Ok(()),
        false => Err(Failure::Reported),
    }
}

/// Which check a spawned child must pass when it is integrated, and who
/// integrates it.
pub(crate) fn check_note(spawned: &branchyard::Spawned) -> String {
    let check = match (&spawned.check, spawned.check_inherited, spawned.land) {
        (Some(check), true, false) => format!(
            "its check, inherited from its parent: {}; siblings that share it are integrated \
             together (by integrate a b), and a child that should land alone needs its own \
             (--check)\n",
            check.join(" ")
        ),
        (Some(check), true, true) => format!(
            "its check, inherited from its parent: {}; siblings that share it land together\n",
            check.join(" ")
        ),
        (Some(check), false, _) => format!("its check: {}\n", check.join(" ")),
        (None, ..) => String::new(),
    };
    match spawned.land {
        true => {
            check
                + "left to the engine to land: it is integrated into your branch when it is ready, \
               and your next turn says what landed\n"
        }
        false => check,
    }
}

pub fn integrate(target: &Target, branches: &[String], json: bool) -> Outcome {
    let [branch] = branches else {
        return integrate_all(target, branches, json);
    };
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => delegate.integrate(branch),
        (None, Target::Remote(remote)) => remote::integrate(remote, branch),
        // A person integrates a child into the parent that delegated it.
        (None, Target::Local) => (|| {
            let yard = open_yard()?;
            let info = yard.branch(branch)?.info().clone();
            let parent = info
                .parent
                .filter(|p| {
                    yard.branch(p)
                        .is_ok_and(|p| p.info().children.iter().any(|c| c == branch))
                })
                .ok_or_else(|| {
                    branchyard::Error::Denied(format!(
                        "{branch} was not delegated by another branch; merge it with by merge"
                    ))
                })?;
            // One yard, so the wait sees a sibling this integration
            // started on this process's threads.
            let parent = yard.branch(&parent)?;
            let merged = parent.delegate(TaskOptions::default())?.integrate(branch)?;
            parent.wait_subtree()?;
            Ok(merged)
        })(),
    };
    emit(json, result, merged_line)
}

pub(crate) fn merged_line(m: &branchyard::Merged) -> String {
    match (&m.via, m.already) {
        (Some(via), true) => format!(
            "{} was already in {}, brought in by {via}; recorded as merged\n",
            m.branch, m.target
        ),
        _ => format!(
            "merged {} into {} ({}..{})\n",
            m.branch,
            m.target,
            short(&m.previous),
            short(&m.commit)
        ),
    }
}

/// `by integrate a b c`: several children of one parent, merged together,
/// checked once, all or none.
pub(crate) fn integrate_all(target: &Target, branches: &[String], json: bool) -> Outcome {
    let names: Vec<&str> = branches.iter().map(String::as_str).collect();
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => delegate.integrate_all(&names),
        (None, Target::Remote(remote)) => remote::integrate_all(remote, &names),
        (None, Target::Local) => (|| {
            let yard = open_yard()?;
            let mut parent = None;
            for branch in &names {
                let info = yard.branch(branch)?.info().clone();
                let found = info
                    .parent
                    .filter(|p| {
                        yard.branch(p)
                            .is_ok_and(|p| p.info().children.iter().any(|c| c == branch))
                    })
                    .ok_or_else(|| {
                        branchyard::Error::Denied(format!(
                            "{branch} was not delegated by another branch; merge it with by merge"
                        ))
                    })?;
                match &parent {
                    Some(p) if *p != found => {
                        return Err(branchyard::Error::Denied(format!(
                            "{branch} was delegated by {found}, not {p}; integrate together only \
                             children of one parent"
                        )))
                    }
                    _ => parent = Some(found),
                }
            }
            let parent = yard.branch(&parent.unwrap_or_default())?;
            let merged = parent
                .delegate(TaskOptions::default())?
                .integrate_all(&names)?;
            parent.wait_subtree()?;
            Ok(merged)
        })(),
    };
    emit(json, result, |all| {
        let mut text: String = all.branches.iter().map(merged_line).collect();
        if all.commit != all.previous {
            let checked = match all.check_results.is_empty() {
                true => "with no check: none of them has one".to_owned(),
                false => format!(
                    "after {} passed once on the result",
                    all.check_results
                        .iter()
                        .map(|c| format!("`{}` ({})", c.check.join(" "), c.branches.join(", ")))
                        .collect::<Vec<_>>()
                        .join(" and ")
                ),
            };
            text.push_str(&format!(
                "{} moved once: {}..{}, {checked}\n",
                all.target,
                short(&all.previous),
                short(&all.commit)
            ));
        }
        text
    })
}

/// `by wait [BRANCH...] [--any|--all] [--timeout S]`: block until
/// delegated branches settle, on the store's notifications.
pub fn wait(
    env: &Env,
    target: &Target,
    branches: &[String],
    any: bool,
    timeout: Option<f64>,
    json: bool,
) -> Outcome {
    let timeout = match timeout {
        None => None,
        Some(secs) if secs.is_finite() && secs >= 0.0 => {
            Some(std::time::Duration::try_from_secs_f64(secs).unwrap_or(std::time::Duration::MAX))
        }
        Some(secs) => {
            let error = branchyard::Error::Usage(format!(
                "--timeout takes a number of seconds, such as 30 or 2.5, not {secs}"
            ));
            return fail(json, &error);
        }
    };
    let delegate = harness_delegate(json)?;
    let wait_for = |names: &[&str], any: bool, timeout: Option<Duration>| {
        match (&delegate, target)
    {
        (Some(delegate), _) => delegate.wait_for(names, any, timeout),
        (None, _) if names.is_empty() => Err(branchyard::Error::Usage(
            "wait needs the branches to wait for outside a harness: `by wait <branch>...` (`by ls` \
             lists them)"
                .into(),
        )),
        (None, Target::Remote(remote)) => remote
            .repo
            .wait_for(names, any, timeout)
            .map_err(remote::sdk_error),
        (None, Target::Local) => open_yard().and_then(|yard| yard.wait_for(names, any, timeout)),
    }
    };
    let deadline = timeout.and_then(|t| std::time::Instant::now().checked_add(t));
    let left = || deadline.map(|d| d.saturating_duration_since(std::time::Instant::now()));
    // Each branch is reported as it settles, so what settled survives a
    // caller that stops `by wait` before it returns: as text on stdout,
    // with --json as a line on stderr naming the branch and its status
    // (stdout keeps the one result, and a harness that reads both
    // streams does not see each inspection twice).
    let mut reported = std::collections::BTreeSet::new();
    let mut report = |inspection: &branchyard::Inspection| -> Outcome {
        if !reported.insert(inspection.name.clone()) {
            return Ok(());
        }
        match json {
            true => {
                let line = serde_json::json!({
                    "event": "settled",
                    "name": inspection.name,
                    "status": inspection.status,
                });
                eprintln!("{line}");
                Ok(())
            }
            false => print(&render::inspection(inspection, env.style())),
        }
    };
    let mut names: Vec<String> = branches.to_vec();
    let mut pending = names.clone();
    let result = loop {
        let asked: Vec<&str> = pending.iter().map(String::as_str).collect();
        let step = match wait_for(&asked, true, left()) {
            Ok(step) => step,
            Err(error) => break Err(error),
        };
        if names.is_empty() {
            // Inside a harness with no branches named: the children it
            // resolved to, waited for from here on by name.
            names = step.settled.iter().map(|i| i.name.clone()).collect();
            names.extend(step.pending.iter().cloned());
        }
        for inspection in &step.settled {
            report(inspection)?;
        }
        if any || step.timed_out || step.pending.is_empty() {
            let asked: Vec<&str> = names.iter().map(String::as_str).collect();
            break match (any, step.timed_out) {
                (true, _) => Ok(step),
                (false, timed_out) => wait_for(
                    &asked,
                    false,
                    match timed_out {
                        true => Some(Duration::ZERO),
                        false => left(),
                    },
                ),
            };
        }
        pending = step.pending;
    };
    if let Ok(waited) = &result {
        for inspection in &waited.settled {
            report(inspection)?;
        }
    }
    let timed_out = result.as_ref().is_ok_and(|w| w.timed_out);
    emit(json, result, |w| match w.pending.is_empty() {
        true => String::new(),
        false => format!(
            "{}: {}\n",
            match w.timed_out {
                true => "timed out; still running",
                false => "still running",
            },
            w.pending.join(", ")
        ),
    })?;
    match timed_out {
        true => Err(Failure::Reported),
        false => Ok(()),
    }
}

/// Inside a harness, cancel a descendant with the branch's authority;
/// otherwise cancel the branch and its descendants, locally or on the
/// server.
pub fn cancel(target: &Target, branch: &str, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => delegate.cancel(branch),
        (None, Target::Remote(remote)) => {
            let cancelled = remote.repo.cancel(branch)?;
            let status = remote.repo.branch(branch)?.status;
            Ok(branchyard::Cancelled::of(cancelled, branch, &status))
        }
        (None, Target::Local) => open_yard().and_then(|yard| {
            let cancelled = yard.cancel_as(branch, "by cancel")?;
            let status = yard.branch(branch)?.info().status.clone();
            Ok(branchyard::Cancelled::of(cancelled, branch, &status))
        }),
    };
    emit(json, result, |c| match (&c.note, c.cancelled.is_empty()) {
        (Some(note), _) => format!("{note}\n"),
        (None, true) => "nothing was running\n".into(),
        (None, false) => format!("asked {} to stop\n", c.cancelled.join(", ")),
    })
}

/// Set a settled child aside: inside a harness, a descendant, with the
/// branch's authority; otherwise any branch, with yours.
pub fn discard(target: &Target, branch: &str, reason: Option<&str>, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => delegate.discard(branch, reason),
        (None, Target::Remote(remote)) => remote
            .repo
            .discard(branch, reason)
            .map_err(remote::sdk_error),
        (None, Target::Local) => (|| {
            let reason = reason
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .unwrap_or("discarded with by discard");
            open_yard()?.discard(branch, reason)?;
            as_user(branch, TaskOptions::default())?.inspect(branch)
        })(),
    };
    // One line: a meta discards several children in a row, and `by
    // inspect` shows the rest.
    emit(json, result, |i| {
        let reason = match &i.status {
            branchyard::BranchStatus::Discarded { reason } => format!(": {reason}"),
            _ => String::new(),
        };
        format!(
            "discarded {}{reason}; its record, worktree and cost stay until `by rm {}`\n",
            i.name, i.name
        )
    })
}

/// Inside a harness, a command acts as the harness's own branch only: a
/// `--branch` or `--as` naming another is refused, not ignored.
pub(crate) fn as_itself(
    delegate: &Delegate,
    named: Option<&str>,
    flag: &str,
) -> Result<(), branchyard::Error> {
    match named {
        Some(other) if other != delegate.branch() => Err(branchyard::Error::Denied(format!(
            "inside a harness, by acts only as {}, the harness's own branch; {flag} {other} \
             names another",
            delegate.branch()
        ))),
        _ => Ok(()),
    }
}

/// The acting branch outside a harness: `--as`, required, since these
/// commands have no other way to name who is asking.
pub(crate) fn as_branch_required(
    as_branch: Option<String>,
    command: &str,
) -> Result<String, branchyard::Error> {
    as_branch.ok_or_else(|| {
        branchyard::Error::Usage(format!(
            "{command} needs --as <BRANCH> outside a harness, the branch that is acting: `by \
             {command} --as <branch> ...` (`by ls` lists them)"
        ))
    })
}

pub fn ask(
    target: &Target,
    as_branch: Option<String>,
    text: &str,
    wait_seconds: Option<f64>,
    json: bool,
) -> Outcome {
    let wait = wait_seconds.map(std::time::Duration::from_secs_f64);
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => as_itself(&delegate, as_branch.as_deref(), "--as")
            .and_then(|()| delegate.ask(text, wait)),
        (None, Target::Remote(remote)) => as_branch_required(as_branch, "ask").and_then(|b| {
            remote
                .repo
                .ask(&b, text, wait_seconds)
                .map_err(remote::sdk_error)
        }),
        (None, Target::Local) => as_branch_required(as_branch, "ask")
            .and_then(|b| as_user(&b, TaskOptions::default())?.ask(text, wait)),
    };
    emit(json, result, |asked| match &asked.answer {
        Some(answer) => format!(
            "asked #{}: {}\nanswer: {}\n",
            asked.message.id, asked.message.text, answer.text
        ),
        None => format!(
            "asked #{}: {}\nno answer yet\n",
            asked.message.id, asked.message.text
        ),
    })
}

/// `by report`, with the result `--result` or `--result-file` gave, if any:
/// checked against the branch's result contract and recorded before the
/// message is sent.
pub fn report(
    target: &Target,
    as_branch: Option<String>,
    text: &str,
    result: (Option<&str>, Option<&str>),
    json: bool,
) -> Outcome {
    let value = match json_flag("--result", result) {
        Ok(value) => value,
        Err(error) => return fail(json, &error),
    };
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => {
            as_itself(&delegate, as_branch.as_deref(), "--as").and_then(|()| match &value {
                Some(value) => delegate.report_result(text, value),
                None => delegate.report(text),
            })
        }
        (None, Target::Remote(remote)) => as_branch_required(as_branch, "report").and_then(|b| {
            remote
                .repo
                .report_result(&b, text, value.clone())
                .map_err(remote::sdk_error)
        }),
        (None, Target::Local) => as_branch_required(as_branch, "report").and_then(|b| {
            let delegate = as_user(&b, TaskOptions::default())?;
            match &value {
                Some(value) => delegate.report_result(text, value),
                None => delegate.report(text),
            }
        }),
    };
    let with_result = value.is_some();
    emit(json, result, |m| match with_result {
        true => format!("reported #{} with a result: {}\n", m.id, m.text),
        false => format!("reported #{}: {}\n", m.id, m.text),
    })
}

pub fn escalate(target: &Target, as_branch: Option<String>, text: &str, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => as_itself(&delegate, as_branch.as_deref(), "--as")
            .and_then(|()| delegate.escalate(text)),
        (None, Target::Remote(remote)) => as_branch_required(as_branch, "escalate")
            .and_then(|b| remote.repo.escalate(&b, text).map_err(remote::sdk_error)),
        (None, Target::Local) => as_branch_required(as_branch, "escalate")
            .and_then(|b| as_user(&b, TaskOptions::default())?.escalate(text)),
    };
    emit(json, result, |m| {
        format!("escalated #{} to {}: {}\n", m.id, m.to, m.text)
    })
}

/// `by report-status`: record the branch's own status beside the protocol's.
pub fn report_status(
    target: &Target,
    as_branch: Option<String>,
    report: (&str, &str, Option<&str>),
    ttl: Option<std::time::Duration>,
    json: bool,
) -> Outcome {
    let (state, summary, link) = report;
    let delegate = harness_delegate(json)?;
    let state = branchyard::ReportedState::parse(state);
    let result = match (delegate, target) {
        (Some(delegate), _) => as_itself(&delegate, as_branch.as_deref(), "--as")
            .and_then(|()| delegate.report_status(state?, summary, link, ttl)),
        // The rules are the engine's, checked here as well so a refusal
        // reads as a local one does; the server checks them again.
        (None, Target::Remote(remote)) => {
            as_branch_required(as_branch, "report-status").and_then(|b| {
                let state = state?;
                branchyard::reported::check(summary, link)?;
                branchyard::reported::check_ttl(ttl.map(|t| t.as_secs_f64()))?;
                remote
                    .repo
                    .report_status(&b, state, summary, link, ttl)
                    .map_err(remote::sdk_error)
            })
        }
        (None, Target::Local) => as_branch_required(as_branch, "report-status").and_then(|b| {
            as_user(&b, TaskOptions::default())?.report_status(state?, summary, link, ttl)
        }),
    };
    emit(json, result, |reported| {
        let mut text = format!("{}\n", reported.report.describe());
        if let Some(message) = &reported.escalation {
            text.push_str(&format!("escalated #{} to {}\n", message.id, message.to));
        }
        text
    })
}

pub fn answer(
    target: &Target,
    as_branch: Option<String>,
    message_id: u64,
    text: &str,
    json: bool,
) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => as_itself(&delegate, as_branch.as_deref(), "--as")
            .and_then(|()| delegate.answer(message_id, text)),
        (None, Target::Remote(remote)) => as_branch_required(as_branch, "answer").and_then(|b| {
            remote
                .repo
                .answer(&b, message_id, text)
                .map_err(remote::sdk_error)
        }),
        (None, Target::Local) => as_branch_required(as_branch, "answer")
            .and_then(|b| as_user(&b, TaskOptions::default())?.answer(message_id, text)),
    };
    emit(json, result, |m| {
        format!("answered #{message_id} as #{}: {}\n", m.id, m.text)
    })
}

pub fn inbox(target: &Target, as_branch: Option<String>, unread: bool, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => {
            as_itself(&delegate, as_branch.as_deref(), "--as").and_then(|()| delegate.inbox())
        }
        (None, Target::Remote(remote)) => as_branch_required(as_branch, "inbox")
            .and_then(|b| remote.repo.inbox(&b).map_err(remote::sdk_error)),
        (None, Target::Local) => as_branch_required(as_branch, "inbox")
            .and_then(|b| as_user(&b, TaskOptions::default())?.inbox()),
    };
    // --unread holds for the JSON too, which the Python module reads.
    let result = result.map(|inbox| match unread {
        true => inbox.unread_only(),
        false => inbox,
    });
    emit(json, result, |inbox| {
        let messages: Vec<&branchyard::Message> = inbox.messages.iter().collect();
        if messages.is_empty() {
            return "empty\n".to_owned();
        }
        messages
            .iter()
            .map(|m| {
                let reply = match m.in_reply_to {
                    Some(id) => format!(" (re #{id})"),
                    None => String::new(),
                };
                let read = if inbox.is_unread(m) { " [unread]" } else { "" };
                format!(
                    "#{} {} from {}{reply}{read}: {}\n",
                    m.id,
                    m.kind,
                    m.from,
                    render::message_body(m)
                )
            })
            .collect()
    })
}

pub fn children(env: &Env, target: &Target, branch: Option<String>, json: bool) -> Outcome {
    let result = match (harness_delegate(json)?, target) {
        (Some(delegate), _) => match branch {
            Some(other) if other != delegate.branch() => Err(branchyard::Error::Denied(
                "inside a harness, by children lists your own branch's descendants".into(),
            )),
            _ => delegate.children(),
        },
        (None, Target::Remote(remote)) => {
            required_outside(branch, "children").and_then(|b| remote::children(remote, &b))
        }
        (None, Target::Local) => required_outside(branch, "children")
            .and_then(|b| as_user(&b, TaskOptions::default())?.children()),
    };
    emit(json, result, |c| match c.descendants.is_empty() {
        true => format!("{} has no children\n", c.branch),
        false => render::branch_table(
            &c.descendants,
            branchyard_support::time::now_ms() / 1000,
            env.style(),
        ),
    })
}

/// `by graph show|apply|resume`; see `docs/graph.md`.
pub fn graph(env: &Env, target: &Target, args: &GraphArgs) -> Outcome {
    let json = args.json;
    let inside = harness_delegate(json)?;
    if inside.is_some() && (args.parent.is_some() || args.task.permissions != Permissions::Unset) {
        let error = branchyard::Error::Denied(
            "inside a harness, the graph is the harness's own branch's and its children inherit \
             its policy; drop --parent, --yes and --ask"
                .into(),
        );
        return fail(json, &error);
    }
    match args.action.as_str() {
        "show" => {
            let result = match (inside, target) {
                (Some(delegate), _) => {
                    let branch = args
                        .arg
                        .clone()
                        .unwrap_or_else(|| delegate.branch().to_owned());
                    delegate.graph(&branch)
                }
                (None, Target::Remote(remote)) => required_outside(args.arg.clone(), "graph show")
                    .and_then(|b| remote.repo.graph(&b).map_err(remote::sdk_error)),
                (None, Target::Local) => required_outside(args.arg.clone(), "graph show")
                    .and_then(|b| as_user(&b, TaskOptions::default())?.graph(&b)),
            };
            emit(json, result, |g| render::graph(g, env.style()))
        }
        "apply" => {
            let proposal = match proposal(args) {
                Ok(proposal) => proposal,
                Err(error) => return fail(json, &error),
            };
            let result = match (inside, target) {
                (Some(delegate), _) => {
                    delegate.apply_graph(proposal.edits, proposal.expected_revision)
                }
                (None, Target::Remote(remote)) => {
                    required_outside(args.parent.clone(), "graph apply --parent").and_then(
                        |parent| remote::apply_graph(remote, &parent, proposal, &args.task),
                    )
                }
                (None, Target::Local) => {
                    let parent = match required_outside(args.parent.clone(), "graph apply --parent")
                    {
                        Ok(parent) => parent,
                        Err(error) => return fail(json, &error),
                    };
                    // The children run on this process's threads, so the
                    // command waits for them, and for what they start.
                    let live = Live::start_to(env, &args.task, true, json, None);
                    let result = (|| {
                        let parent = open_yard()?.branch(&parent)?;
                        let delegate = parent.delegate(live.options(&args.task)?)?;
                        let applied =
                            delegate.apply_graph(proposal.edits, proposal.expected_revision)?;
                        parent.wait_subtree()?;
                        Ok(applied)
                    })();
                    live.console.finish();
                    result
                }
            };
            emit(json, result, |a| render::graph_applied(a, env.style()))
        }
        _ => {
            // resume
            let result = match (inside, target) {
                (Some(_), _) => Err(branchyard::Error::Denied(
                    "by graph resume is for a person, outside a harness".into(),
                )),
                (None, Target::Remote(_)) => Err(branchyard::Error::Unsupported(
                    "a server resumes its graphs itself, on its recovery interval".into(),
                )),
                (None, Target::Local) => {
                    let live = Live::start_to(env, &args.task, true, json, None);
                    let result = (|| {
                        let yard = open_yard()?;
                        let started = yard.resume_graph(&live.options(&args.task)?)?;
                        for name in &started {
                            if let Some(parent) = yard.branch(name)?.info().parent.clone() {
                                yard.branch(&parent)?.wait_subtree()?;
                            }
                        }
                        Ok(serde_json::json!({ "started": started }))
                    })();
                    live.console.finish();
                    result
                }
            };
            emit(json, result, |value| match value["started"].as_array() {
                Some(started) if !started.is_empty() => format!(
                    "started {}\n",
                    started
                        .iter()
                        .filter_map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                _ => "nothing was waiting on settled prerequisites\n".into(),
            })
        }
    }
}

/// `by delegate --plan FILE`: a plan of children, created all or none,
/// with the brief they share.
/// `by delegate`: `given` is `--brief` and `--land`, which override the
/// plan's own.
pub fn delegate(
    env: &Env,
    target: &Target,
    plan: &str,
    given: (Option<String>, bool),
    parent: Option<String>,
    task: &TaskArgs,
    json: bool,
) -> Outcome {
    let inside = harness_delegate(json)?;
    if inside.is_some()
        && (parent.is_some() || task.permissions != Permissions::Unset || task.unapproved_tools)
    {
        let error = branchyard::Error::Denied(
            "inside a harness, the plan's children are the harness's own branch's and inherit \
             its policy and approval routing; drop --parent, --yes, --ask, --permissions and \
             --allow-unapproved-tools"
                .into(),
        );
        return fail(json, &error);
    }
    let mut plan = match read_plan(plan) {
        Ok(plan) => plan,
        Err(error) => return fail(json, &error),
    };
    let (brief, land) = given;
    if let Some(brief) = brief {
        plan.brief = Some(brief);
    }
    plan.land |= land;
    let result = match (inside, target) {
        (Some(delegate), _) => delegate.delegate(plan),
        (None, Target::Remote(_)) => Err(no_plan_route()),
        (None, Target::Local) => {
            let parent = match required_outside(parent, "delegate --parent") {
                Ok(parent) => parent,
                Err(error) => return fail(json, &error),
            };
            // The children run on this process's threads, so the command
            // waits for them, and for what they start.
            let live = Live::start_to(env, task, true, json, None);
            let result = (|| {
                let parent = open_yard()?.branch(&parent)?;
                let delegate = parent.delegate(live.options(task)?)?;
                let delegated = delegate.delegate(plan)?;
                parent.wait_subtree()?;
                Ok(delegated)
            })();
            live.console.finish();
            result
        }
    };
    emit(json, result, |d| render::delegated(d, env.style()))
}

/// `by brief [TEXT]`: the shared brief, after adding TEXT to it.
pub fn brief(
    env: &Env,
    target: &Target,
    text: Option<String>,
    parent: Option<String>,
    json: bool,
) -> Outcome {
    let inside = harness_delegate(json)?;
    if inside.is_some() && parent.is_some() {
        let error = branchyard::Error::Denied(
            "inside a harness, the brief is the harness's own branch's; drop --parent".into(),
        );
        return fail(json, &error);
    }
    let result = match (inside, target) {
        (Some(delegate), _) => delegate.brief(text.as_deref()),
        (None, Target::Remote(_)) => Err(no_plan_route()),
        (None, Target::Local) => required_outside(parent, "brief --parent")
            .and_then(|parent| as_user(&parent, TaskOptions::default())?.brief(text.as_deref())),
    };
    emit(json, result, |b| render::brief(b, env.style()))
}

pub(crate) fn no_plan_route() -> branchyard::Error {
    branchyard::Error::Unsupported(
        "a server has no route for a plan or a brief yet: spawn each child with by spawn \
         --parent, or the plan's tasks as a proposal with by graph apply --parent"
            .into(),
    )
}

/// The plan `by delegate --plan` was given: a JSON file, `-` for standard
/// input, read as the `delegate` tool reads its arguments (a task may give
/// its limits flat).
pub(crate) fn read_plan(path: &str) -> Result<branchyard::DelegationPlan, branchyard::Error> {
    let invalid = |why: String| branchyard::Error::Denied(format!("invalid plan: {why}"));
    let text = match path {
        "-" => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
            text
        }
        path => std::fs::read_to_string(path)
            .map_err(|e| branchyard::Error::Denied(format!("--plan {path}: {e}")))?,
    };
    let mut plan: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| invalid(e.to_string()))?;
    if let Some(tasks) = plan.get_mut("tasks") {
        branchyard::nest_plan_budgets(tasks)?;
    }
    serde_json::from_value(plan).map_err(|e| invalid(e.to_string()))
}

/// The proposal `by graph apply` was given: a file (`-` for stdin) holding
/// `{"expected_revision", "edits"}`, or `--edits` with
/// `--expected-revision`, which also overrides a file's revision.
pub(crate) fn proposal(args: &GraphArgs) -> Result<branchyard::GraphProposal, branchyard::Error> {
    let invalid = |why: String| branchyard::Error::Denied(format!("invalid graph proposal: {why}"));
    // Read as the `apply_graph` tool reads its arguments: a spawn may give
    // its limits flat (`budget_usd`, `max_turns`, `max_minutes`).
    let read = |text: &str| -> Result<serde_json::Value, branchyard::Error> {
        serde_json::from_str(text).map_err(|e| invalid(e.to_string()))
    };
    let mut proposal = match (&args.edits, args.arg.as_deref()) {
        (Some(edits), _) => {
            let mut edits = read(edits)?;
            branchyard::nest_graph_budgets(&mut edits)?;
            branchyard::GraphProposal {
                expected_revision: args.expected_revision.unwrap_or_default(),
                edits: serde_json::from_value(edits).map_err(|e| invalid(e.to_string()))?,
            }
        }
        (None, Some(path)) => {
            let text = match path {
                "-" => {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                    text
                }
                path => std::fs::read_to_string(path)?,
            };
            let mut proposal = read(&text)?;
            if let Some(edits) = proposal.get_mut("edits") {
                branchyard::nest_graph_budgets(edits)?;
            }
            serde_json::from_value(proposal).map_err(|e| invalid(e.to_string()))?
        }
        (None, None) => return Err(invalid("give a FILE or --edits".into())),
    };
    if let Some(revision) = args.expected_revision {
        proposal.expected_revision = revision;
    }
    Ok(proposal)
}

pub(crate) fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

/// This command's acting branch: the harness's own, or `--branch` outside
/// one.
pub(crate) fn acting_branch(
    delegate: &Option<Delegate>,
    branch: &Option<String>,
    command: &str,
) -> Result<String, branchyard::Error> {
    match (delegate, branch) {
        (Some(delegate), named) => {
            as_itself(delegate, named.as_deref(), "--branch")?;
            Ok(delegate.branch().to_owned())
        }
        (None, Some(branch)) => Ok(branch.clone()),
        (None, None) => Err(branchyard::Error::Denied(format!(
            "outside a harness, by {command} needs --branch"
        ))),
    }
}
