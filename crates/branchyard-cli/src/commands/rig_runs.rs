//! `by rig`: checking and running a rig's seats.

use super::*;

/// `by rig check FILE` prints the plan; `by rig run FILE PROMPT` runs the
/// root seat with it, here or on the server, and waits for every seat it
/// spawned.
pub fn rig(env: &Env, target: &Target, args: &args::RigArgs) -> Outcome {
    let planned = rig::load(Path::new(&args.file)).and_then(|spec| rig::plan(&spec));
    let plan = match planned {
        Ok(plan) => plan,
        Err(error) => return rig_refused(args, &error),
    };
    let Some(prompt) = &args.prompt else {
        return match args.json {
            true => print(&format!("{}\n", to_json(&plan))),
            false => print(&rig::render(&plan)),
        };
    };
    if !plan.unapproved_tools.is_empty() && !args.unapproved_tools {
        let error = branchyard::Error::Unsupported(format!(
            "seats {} run profiles that do not route tool permission requests to Branchyard, so \
             their tools would run without the rig's policy; pass --allow-unapproved-tools to \
             run them anyway",
            plan.unapproved_tools.join(", ")
        ));
        return fail(args.json, &error);
    }
    let seats = match &plan.seats {
        Some(seats) => seats.delegates_to.join(", "),
        None => "none".into(),
    };
    eprintln!(
        "by: rig {}: root seat {} on {}; it may spawn seats {seats}",
        plan.rig, plan.root.seat, plan.root.harness
    );
    if let Target::Remote(remote) = target {
        return remote::rig(env, remote, &plan, prompt, args);
    }
    let root = &plan.root;
    let permissions = match root.policy.default {
        rig::Fallback::Allow => args::Permissions::Yes,
        rig::Fallback::Ask => args::Permissions::Ask,
        rig::Fallback::Deny => args::Permissions::Unset,
    };
    let task = TaskArgs {
        permissions,
        command: args.command.clone(),
        unapproved_tools: args.unapproved_tools,
        ..TaskArgs::default()
    };
    let workspace = workspace(env, &open()?)?;
    let live = Live::start_to(env, &task, true, args.json, None);
    let result = (|| {
        let options = TaskOptions {
            workspace,
            ..live.options(&task)?
        };
        let mut policy = match root.policy.default {
            rig::Fallback::Allow => Policy::allow_all(),
            rig::Fallback::Deny => Policy::deny_all(),
            rig::Fallback::Ask => live.policy.clone(),
        };
        for tool in &root.policy.deny {
            policy = policy.deny(tool.clone());
        }
        for tool in &root.policy.allow {
            policy = policy.allow(tool.clone());
        }
        if let (true, Some(by)) = (root.policy.delegation_commands, &options.delegation_cli) {
            policy = policy.allow_delegation_commands(by);
        }
        let options = TaskOptions {
            harness: Some(root.harness.clone()),
            name: Some(args.name.clone().unwrap_or_else(|| root.name.clone())),
            base: args.base.clone(),
            budget: Budget {
                max_usd: root.budget.max_usd,
                max_turns: root.budget.max_turns,
                max_duration: root
                    .budget
                    .max_minutes
                    .and_then(|m| std::time::Duration::try_from_secs_f64(m * 60.0).ok()),
                ..Budget::default()
            },
            policy,
            check: root.check.clone(),
            result: root.result.clone(),
            isolated: root.isolated,
            delegation: root.delegation.clone(),
            provision: Some(root.provision.clone()),
            seats: plan.seats.clone(),
            ..options
        };
        Ok::<_, Failure>(open()?.task(prompt.as_str()).options(options).run())
    })();
    let result = match result {
        Ok(result) => result,
        Err(failure) => {
            live.console.finish();
            return Err(failure);
        }
    };
    if !args.json {
        return live.finish(env, result);
    }
    let branch = match result {
        Ok(branch) => branch,
        Err(error) => {
            live.console.finish();
            return fail(true, &error);
        }
    };
    let descendants = wait_for_descendants(&[&branch]);
    live.console.finish();
    let descendants = descendants?.unwrap_or_default();
    // Read again: the root's record gained its children during its turn.
    let root = branch.yard().branch(&branch.info().name)?;
    print_rig_run(&plan.rig, root.info(), &descendants)
}

/// `by rig run --json`'s result: the root and every branch below it, the
/// same from a server.
pub fn print_rig_run(rig: &str, root: &BranchInfo, descendants: &[BranchInfo]) -> Outcome {
    let value = serde_json::json!({
        "rig": rig,
        "root": json::branch(root),
        "descendants": descendants.iter().map(json::branch).collect::<Vec<_>>(),
    });
    print(&json::text(&value))?;
    branch_outcome(root)
}

/// A spec `by rig` cannot honor: the field, its line and why; with
/// `--json`, `{"error": {"kind": "invalid_rig", "message", "field", "line",
/// "column"}}`.
pub(crate) fn rig_refused(args: &args::RigArgs, error: &rig::RigError) -> Outcome {
    let report = Report::new("invalid_rig", format!("{}: {error}", args.file))
        .field("field", serde_json::json!(error.field))
        .field("line", serde_json::json!(error.line))
        .field("column", serde_json::json!(error.column));
    emit_report(args.json, report)
}
