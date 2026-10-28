//! Looking at and finishing branches: `by ls`, `show`, `diff`, `log`, `merge` and `rm`.

use super::*;

pub fn ls(env: &Env, target: &Target, as_json: bool) -> Outcome {
    let infos = match target {
        Target::Local => open()?.branches()?,
        Target::Remote(remote) => remote.repo.branches()?,
    };
    if as_json {
        let list = infos.iter().map(json::branch).collect();
        return print(&json::text(&serde_json::Value::Array(list)));
    }
    // Recorded maps and their progress follow the branches (docs/map.md).
    let maps = crate::map_cmd::ls_section(target, env.style()).unwrap_or_default();
    // A server's queued work has no branch yet: say so, or ten runs read
    // as eight.
    let queued = match target {
        Target::Local => String::new(),
        Target::Remote(remote) => remote::queued_line(remote, &infos),
    };
    if infos.is_empty() {
        return print(&format!(
            "no branches; start one with: by run \"<prompt>\"\n{queued}{maps}"
        ));
    }
    print(&format!(
        "{}{queued}{maps}",
        render::branch_table(
            &infos,
            branchyard_support::time::now_ms() / 1000,
            env.style()
        )
    ))
}

/// `by show`, with the merge-readiness line `by pr` and `by pr --watch`
/// recorded; `refresh` asks GitHub first (local mode).
/// `by show` of a branch a queued operation will create.
pub(crate) fn show_queued(
    branch: &str,
    op: &branchyard_client::api::Operation,
    as_json: bool,
) -> Outcome {
    if as_json {
        let value = serde_json::json!({ "branch": branch, "operation": op });
        return print(&json::text(&value));
    }
    let word = |value: serde_json::Value| value.as_str().unwrap_or_default().to_owned();
    let mut text = format!(
        "branch    {branch} (not created yet)\noperation {} ({}, {})\n",
        op.id,
        word(serde_json::to_value(op.kind).unwrap_or_default()),
        word(serde_json::to_value(op.state).unwrap_or_default()),
    );
    if !op.requires.is_empty() {
        text.push_str(&format!("requires  {}\n", op.requires.join(", ")));
    }
    if let Some(waiting) = &op.waiting {
        text.push_str(&format!("waiting   {waiting}\n"));
    }
    print(&text)
}

pub fn show(env: &Env, target: &Target, branch: &str, as_json: bool, refresh: bool) -> Outcome {
    let mut listening = None;
    let (info, events) = match target {
        Target::Local => {
            let yard = open()?;
            if refresh {
                crate::pr::refresh(&yard, branch)?;
            }
            let branch = match yard.branch(branch) {
                Ok(branch) => branch,
                // A map's name shows the map (docs/map.md).
                Err(error @ branchyard::Error::UnknownBranch(_)) => {
                    return match crate::map_cmd::show_if_map(env, target, branch, as_json) {
                        Some(shown) => shown,
                        None => Err(error.into()),
                    }
                }
                Err(error) => return Err(error.into()),
            };
            listening = Some(
                crate::ports::of_yard(&yard)
                    .remove(&branch.info().name)
                    .unwrap_or_default(),
            );
            (branch.info().clone(), branch.events()?)
        }
        Target::Remote(_) if refresh => {
            return Err(Failure::Sdk(branchyard::Error::Unsupported(
                "by show --refresh asks GitHub about a pull request by pr opened from this \
                 repository; it works in local mode only"
                    .into(),
            )))
        }
        Target::Remote(remote) => match remote.repo.branch(branch) {
            Ok(info) => (info, remote.repo.events(branch, 0)?.events),
            // Not created yet: an operation that will create it may be
            // queued, and say why no worker has claimed it.
            Err(error) if error.code() == Some("unknown_branch") => {
                match remote.repo.operations(Some(branch))?.into_iter().next() {
                    Some(op) => return show_queued(branch, &op, as_json),
                    None => {
                        return match crate::map_cmd::show_if_map(env, target, branch, as_json) {
                            Some(shown) => shown,
                            None => Err(error.into()),
                        }
                    }
                }
            }
            Err(error) => return Err(error.into()),
        },
    };
    let checkpoints = crate::attempts::checkpoints(target, &info)?;
    // What it may still spend, and what its tree has spent (docs/delegation.md
    // "Budgets"); a server that cannot inspect it, or a branch that cannot be
    // inspected, shows neither.
    let budget = match target {
        Target::Local => open().ok().and_then(|yard| {
            yard.branch(&info.name)
                .ok()?
                .delegate(TaskOptions::default())
                .ok()?
                .inspect(&info.name)
                .ok()
        }),
        Target::Remote(remote) => remote::inspect(remote, &info.name).ok(),
    };
    let (readiness, line) = crate::pr::show_readiness(&info, &events, env.style());
    // The task it is an attempt of (docs/task-repos.md); a server too old
    // to say has none.
    let task = match target {
        Target::Local => open()
            .ok()
            .and_then(|yard| branchyard::tasks::view(&yard, &info.name).ok()),
        Target::Remote(remote) => remote.repo.task(&info.name).ok(),
    }
    .filter(|view| view.attempts.iter().any(|a| a.name == info.name))
    .map(|view| crate::task_cmd::show_line(&view, &info.name));
    if as_json {
        let mut value = json::branch(&info);
        if let Some((_, task)) = &task {
            value["task"] = task.clone();
        }
        value["checkpoints"] = serde_json::to_value(&checkpoints).unwrap_or_default();
        if let Some(i) = &budget {
            value["subtree_cost_usd"] = serde_json::json!(i.subtree_cost_usd);
            value["max_usd"] = serde_json::json!(i.max_usd);
            value["remaining_usd"] = serde_json::json!(i.remaining_usd);
            if let (Some(left), Some(by)) = (i.tree_remaining_usd, &i.tree_capped_by) {
                value["tree_remaining_usd"] = serde_json::json!(left);
                value["tree_capped_by"] = serde_json::json!(by);
            }
        }
        value["merge_readiness"] = readiness;
        // Only when something listens, so local and remote `show --json`
        // agree for a branch with no servers.
        if let Some(listening) = listening.as_ref().filter(|l| !l.is_empty()) {
            value["listening"] = serde_json::to_value(listening).unwrap_or_default();
        }
        // Its plan and goal, when it has them (docs/plans-and-goals.md).
        crate::plan_cmd::show_json(&info.name, &events, &mut value);
        // How its last turn's network policy was applied (docs/egress.md).
        if let Some((summary, _)) = egress_summary(&events) {
            value["egress"] = summary;
        }
        // Its own account of what it is doing, beside the protocol's status
        // (docs/delegation.md#self-reported-status).
        if let Some((summary, _)) = reported_summary(&events, branchyard_support::time::now_ms()) {
            value["reported"] = summary;
        }
        // Its calls through the model gateway (docs/model-gateway.md).
        if let Some((summary, _)) = crate::models_cmd::summary(&events) {
            value["models"] = summary;
        }
        // Its effects and waiting approvals (docs/effects.md).
        if let Some((summary, _)) = crate::effects_cmd::summary(&events) {
            value["effects"] = summary;
        }
        return print(&json::text(&value));
    }
    let mut extra: Vec<(&str, String)> = line
        .map(|line| ("merge readiness", line))
        .into_iter()
        .collect();
    if let Some((text, _)) = task {
        extra.push(("task", text));
    }
    if let Some((_, text)) = egress_summary(&events) {
        extra.push(("egress", text));
    }
    if let Some((_, text)) = reported_summary(&events, branchyard_support::time::now_ms()) {
        extra.push(("reported", text));
    }
    if let Some((_, text)) = crate::models_cmd::summary(&events) {
        extra.push(("models", text));
    }
    if let Some((_, text)) = crate::effects_cmd::summary(&events) {
        extra.push(("effects", text));
    }
    if let Some(text) = survived_line(&branchyard::survived_failures(&events), &info.name) {
        extra.push(("survived", text));
    }
    if let Some(listening) = listening.filter(|l| !l.is_empty()) {
        extra.push(("listening", crate::ports::lines(&listening).join("; ")));
    }
    extra.extend(crate::plan_cmd::show_lines(&info.name, &events));
    let mut text = render::details(
        &info,
        budget.as_ref(),
        branchyard_support::time::now_ms() / 1000,
        env.style(),
        extra,
    );
    text.push_str(&crate::attempts::checkpoint_lines(
        &checkpoints,
        env.style(),
    ));
    print(&text)
}

/// The one line `by show` gives steps a branch's engine survived (a sandbox
/// that would not tear down, a lease that would not release): how many, the
/// first, and where the rest are. `None` when there are none. The same
/// failures were printed as `by: warning: ...` when they happened.
pub(crate) fn survived_line(failures: &[&str], branch: &str) -> Option<String> {
    let first = failures.first()?;
    let count = match failures.len() {
        1 => "1 step failed".to_owned(),
        n => format!("{n} steps failed"),
    };
    Some(format!(
        "{count} and the run carried on, the first: {first}. See: by log {branch}"
    ))
}

/// The branch's latest self-reported status, as JSON and as one line for
/// `by show`: `blocked: waiting on the API key (https://...), 3m ago`, or
/// `blocked: waiting on the API key (stale, 14m old)` once it is stale
/// (`docs/delegation.md#self-reported-status`). `None` when it never
/// reported one. A claim of the harness's, shown beside the status the
/// protocol gives.
pub(crate) fn reported_summary(
    events: &[branchyard::RecordedEvent],
    now_ms: u64,
) -> Option<(serde_json::Value, String)> {
    let report = branchyard::reported::latest(events, now_ms)?;
    let age = render::age_text(now_ms.saturating_sub(report.at_ms) / 1000);
    let mut text = render::reported_text(&report);
    text.push_str(&match (report.stale, age.as_str()) {
        (true, "now") => " (stale, just now)".to_owned(),
        (true, age) => format!(" (stale, {age} old)"),
        (false, "now") => ", just now".to_owned(),
        (false, age) => format!(", {age} ago"),
    });
    Some((serde_json::to_value(&report).ok()?, text))
}

/// How the branch's last turn with a network policy applied it, and what
/// its proxy decided in that turn: as JSON and as one line. `None` when no
/// turn had one.
pub(crate) fn egress_summary(
    events: &[branchyard::RecordedEvent],
) -> Option<(serde_json::Value, String)> {
    use branchyard::EgressActivity;
    let at = events.iter().rposition(|e| {
        matches!(&e.activity, Activity::Egress(egress)
            if matches!(egress.as_ref(), EgressActivity::Applied { .. }))
    })?;
    let Activity::Egress(applied) = &events[at].activity else {
        return None;
    };
    let EgressActivity::Applied {
        policy,
        allow,
        enforcement,
        reason,
        ..
    } = applied.as_ref()
    else {
        return None;
    };
    let (mut allowed, mut denied) = (0, 0);
    for event in &events[at + 1..] {
        if let Activity::Egress(egress) = &event.activity {
            if let EgressActivity::Decision { allowed: yes, .. } = egress.as_ref() {
                match yes {
                    true => allowed += 1,
                    false => denied += 1,
                }
            }
        }
    }
    let mut text = format!(
        "{} ({policy}); {allowed} allowed, {denied} denied",
        enforcement.as_str()
    );
    if let Some(reason) = reason {
        text.push_str(&format!("; not enforced: {reason}"));
    }
    let value = serde_json::json!({
        "policy": policy,
        "allow": allow,
        "enforcement": enforcement,
        "reason": reason,
        "allowed": allowed,
        "denied": denied,
    });
    Some((value, text))
}

pub fn diff(env: &Env, target: &Target, branch: &str) -> Outcome {
    let diff = match target {
        Target::Local => open()?.branch(branch)?.diff()?,
        Target::Remote(remote) => remote.repo.diff(branch)?,
    };
    if !env.stdout_tty || diff.is_empty() {
        return print(&diff);
    }
    let text = if env.color { color_diff(&diff) } else { diff };
    page(&text)
}

/// Color a unified diff the way `git diff` does.
pub(crate) fn color_diff(diff: &str) -> String {
    let style = Style { color: true };
    diff.lines()
        .map(|line| {
            let tone = if line.starts_with("+++") || line.starts_with("---") {
                Some(Tone::Bold)
            } else if line.starts_with('+') {
                Some(Tone::Green)
            } else if line.starts_with('-') {
                Some(Tone::Red)
            } else if line.starts_with("@@") {
                Some(Tone::Cyan)
            } else if line.starts_with("diff ") {
                Some(Tone::Bold)
            } else {
                None
            };
            let line = match tone {
                Some(tone) => style.paint(tone, line),
                None => line.to_owned(),
            };
            line + "\n"
        })
        .collect()
}

/// Show `text` through `$PAGER` (default `less`), or print it if the pager
/// cannot start. Quitting the pager early is not an error.
pub(crate) fn page(text: &str) -> Outcome {
    let pager = std::env::var("PAGER")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| "less".into());
    let argv = args::split_words(&pager).map_err(|e| {
        Report::new("config", format!("PAGER is not a command line: {e}"))
            .next("fix PAGER, or unset it to use less")
    })?;
    let Some((program, rest)) = argv.split_first() else {
        return print(text);
    };
    let mut command = Command::new(program);
    command.args(rest).stdin(Stdio::piped());
    if std::env::var_os("LESS").is_none() {
        // Quit if one screen, pass color through, keep the screen.
        command.env("LESS", "FRX");
    }
    let Ok(mut child) = command.spawn() else {
        return print(text);
    };
    if let Some(mut stdin) = child.stdin.take() {
        match stdin.write_all(text.as_bytes()) {
            Err(error) if error.kind() != io::ErrorKind::BrokenPipe => return Err(error.into()),
            _ => {}
        }
    }
    child.wait()?;
    Ok(())
}

pub fn log(env: &Env, target: &Target, branch: &str, as_json: bool, follow: bool) -> Outcome {
    if follow {
        return log_follow(env, target, branch, as_json);
    }
    let events = match target {
        Target::Local => {
            let yard = open()?;
            // The gateway's newest calls, if it has written any.
            best_effort(
                "ingest the connector audit log",
                yard.ingest_connector_audit(),
            );
            yard.branch(branch)?.events()?
        }
        Target::Remote(remote) => remote.repo.events(branch, 0)?.events,
    };
    if as_json {
        let list = events.iter().map(json::recorded).collect();
        return print(&json::text(&serde_json::Value::Array(list)));
    }
    print(&render::log_text(&events, env.style()))
}

/// How long `by log --follow` waits for an event before it prints message
/// text it held back (and, locally, before it looks again).
pub(crate) const FOLLOW_POLL: Duration = Duration::from_millis(500);

/// `by log --follow`: print what is recorded, then each new event as it is
/// recorded, until interrupted. Locally it waits on the store; remotely it
/// reads the repository's event stream ([`remote::follow_log`]) and rides
/// out a server restart.
pub(crate) fn log_follow(env: &Env, target: &Target, branch: &str, as_json: bool) -> Outcome {
    let mut out = Following::new(env, as_json);
    if let Target::Remote(remote) = target {
        return remote::follow_log(remote, branch, &mut out);
    }
    let yard = open()?;
    let local = yard.branch(branch)?;
    let mut cursor = 0u64;
    loop {
        // The gateway's newest calls, as connector_call events.
        branchyard_support::best_effort_once(
            "ingest the connector audit log",
            yard.ingest_connector_audit(),
        );
        let page = local.wait_for_events(cursor, 500, FOLLOW_POLL)?;
        cursor = page.next_cursor;
        out.take(page.events)?;
    }
}

/// What `by log --follow` prints, as events arrive. Message text still
/// streaming is held back until the events pause, so a reply is not cut
/// into many stamped pieces.
pub struct Following<'a> {
    env: &'a Env,
    as_json: bool,
    held: Vec<RecordedEvent>,
}

impl<'a> Following<'a> {
    pub(crate) fn new(env: &'a Env, as_json: bool) -> Self {
        Following {
            env,
            as_json,
            held: Vec::new(),
        }
    }

    /// Print what `events` complete; none means the events paused.
    pub fn take(&mut self, events: Vec<RecordedEvent>) -> Outcome {
        let quiet = events.is_empty();
        self.held.extend(events);
        // Everything up to the last event that is not message text is
        // complete; trailing text waits for more unless the events paused.
        let split = match quiet {
            true => self.held.len(),
            false => self
                .held
                .iter()
                .rposition(|e| !matches!(e.activity, Activity::Harness(Event::MessageDelta { .. })))
                .map_or(0, |i| i + 1),
        };
        let ready: Vec<RecordedEvent> = self.held.drain(..split).collect();
        if ready.is_empty() {
            return Ok(());
        }
        let text = match self.as_json {
            true => ready
                .iter()
                .map(|event| format!("{}\n", json::recorded(event)))
                .collect(),
            false => render::log_text(&ready, self.env.style()),
        };
        print(&text)
    }
}

/// [`merge`] without removing, and locally without printing, for a command
/// whose stdout is JSON (`by compare --pick --json`, `by judge --pick
/// --json`). Remotely the server's merge line is still printed.
pub fn merge_quietly(target: &Target, branch: &str, into: Option<&str>) -> Outcome {
    if let Target::Remote(remote) = target {
        return remote::merge(remote, branch, into).map(drop);
    }
    let yard = open()?;
    let target = match into {
        Some(target) => target.to_owned(),
        None => yard.current_branch()?.ok_or_else(detached_head)?,
    };
    yard.merge(branch, &target)?;
    Ok(())
}

pub fn merge(target: &Target, branch: &str, into: Option<&str>, remove: bool) -> Outcome {
    merge_as(target, branch, into, remove, false)
}

/// `by merge`, and with `json` the branch, what it merged into, the merge
/// commit and the check's result (`null` for a branch with no check) as
/// one object.
pub fn merge_as(
    target: &Target,
    branch: &str,
    into: Option<&str>,
    remove: bool,
    json: bool,
) -> Outcome {
    let (merged, check) = match target {
        Target::Remote(remote) => {
            let check = remote::inspect(remote, branch).ok().and_then(|i| i.check);
            match remote::merge(remote, branch, into) {
                Ok(merged) => (merged, check),
                Err(Failure::Remote(error)) => {
                    return emit_report(json, crate::errors::remote(&error, Some(branch)))
                }
                Err(other) => return Err(other),
            }
        }
        Target::Local => {
            let yard = open()?;
            let into = match into {
                Some(target) => target.to_owned(),
                None => yard.current_branch()?.ok_or_else(detached_head)?,
            };
            let check = check_of(&yard, branch);
            match yard.merge(branch, &into) {
                Ok(merged) => (merged, check),
                Err(error) => return fail_in(json, &error, Some(branch)),
            }
        }
    };
    if json {
        if remove {
            remove_branch(target, branch, false)?;
        }
        let mut value = serde_json::to_value(&merged).unwrap_or_default();
        value["check"] = match check {
            Some(command) => serde_json::json!({ "command": command, "result": "passed" }),
            None => serde_json::Value::Null,
        };
        value["removed"] = serde_json::json!(remove);
        return print(&json::text(&value));
    }
    print_merged(
        &merged.branch,
        &merged.target,
        &merged.previous,
        &merged.commit,
        !remove,
    )?;
    if remove {
        remove_branch(target, branch, false)?;
        print(&format!("removed {branch}\n"))?;
    }
    Ok(())
}

/// The check a branch's merge must pass, when it has one.
pub(crate) fn check_of(yard: &Yard, branch: &str) -> Option<Vec<String>> {
    yard.branch(branch)
        .ok()?
        .delegate(TaskOptions::default())
        .ok()?
        .inspect(branch)
        .ok()?
        .check
}

/// `merged X into BASE (a..b)`; `free_name` adds that `by rm X` frees the
/// name, which a merged branch keeps until it is removed.
pub fn print_merged(
    branch: &str,
    target: &str,
    previous: &str,
    commit: &str,
    free_name: bool,
) -> Outcome {
    let short = |commit: &str| commit.get(..10).unwrap_or(commit).to_owned();
    let freed = match free_name {
        true => format!("; `by rm {}` frees the name", shell_quote(branch)),
        false => String::new(),
    };
    print(&format!(
        "merged {branch} into {target} ({}..{}){freed}\n",
        short(previous),
        short(commit)
    ))
}

/// `by rm`, and with `json` `{"removed": NAME}`.
pub fn rm(target: &Target, branch: &str, keep_credentials: bool, json: bool) -> Outcome {
    remove_branch(target, branch, keep_credentials)?;
    match json {
        true => print(&json::text(&serde_json::json!({ "removed": branch }))),
        false => print(&format!("removed {branch}\n")),
    }
}

/// Remove `branch`'s worktree and record, locally or on the server.
pub(crate) fn remove_branch(
    target: &Target,
    branch: &str,
    keep_credentials: bool,
) -> Result<(), Failure> {
    match target {
        Target::Local => {
            let teardown = open()?.remove_reporting(branch, &RemoveOptions { keep_credentials })?;
            if let Some(report) = teardown {
                // Best-effort: the branch is gone either way.
                eprintln!(
                    "by: {}",
                    render::workspace_line(&report, render::Style { color: false })
                );
            }
        }
        // The server decides what stays on its disk.
        Target::Remote(_) if keep_credentials => {
            return Err(Report::usage(
                "--keep-credentials is for a local yard; a server always removes them",
            )
            .next(format!(
                "by rm {} (without --keep-credentials)",
                shell_quote(branch)
            ))
            .into())
        }
        Target::Remote(remote) => remote.repo.remove(branch)?,
    }
    Ok(())
}
