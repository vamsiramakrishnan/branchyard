//! Routing and judging from the command line: what `by run --auto` and
//! `by fan --auto` print about the router's choice, `by judge`, and `by
//! fleet stats|route`. The decisions are the SDK's (`Yard::run_routed`,
//! `Yard::judge`); this renders them. See docs/fleet.md.

use std::sync::Arc;

use branchyard::{
    classify, fleet_stats, recorded_route, Advice, AdviceScope, Advised, CandidateStats, Fleet,
    InventoryDefault, Judge, JudgeOptions, JudgeSpec, Judgement, Route, RouteOptions, Routed,
    TaskKind, TaskOptions, Yard, ENVELOPE_ENTRY, INVENTORY_ENTRY,
};
use serde_json::json;

use crate::args::TaskArgs;
use crate::attempts::{self, CompareArgs};
use crate::commands::{open, print, Env, Failure, Outcome, Target};
use crate::json;
use crate::render::{self, Cell, Column, Style, Tone};

fn style(env: &Env) -> Style {
    Style { color: env.color }
}

/// Why routing does not run remotely.
pub fn local_only() -> Failure {
    Failure::Message(
        "routing (--auto, --kind, or a [fleet] with no --harness) and judging run in local mode \
         only for now; name a harness with --harness"
            .into(),
    )
}

/// The fleet table a routed task uses.
pub fn table(task: &TaskArgs) -> Result<&Fleet, Failure> {
    task.fleet.as_ref().ok_or_else(|| {
        Failure::Message(
            "--auto routes through a [fleet] table, and branchyard.toml has none; add \
             [fleet.default] with candidates (see docs/fleet.md), or name a harness"
                .into(),
        )
    })
}

/// The inventory default here (docs/fleet.md#the-inventory-default): the
/// harnesses `by harnesses` says are logged in on this machine, `[defaults]
/// harness` first, for a branch whose home is (`private_home`) or is not
/// its own.
pub fn inventory_default(private_home: bool) -> Result<InventoryDefault, Failure> {
    let inventory = crate::harness_cmd::detected_here().map_err(|e| {
        Failure::Message(format!(
            "with no [fleet], --auto routes among the harnesses logged in here, and they \
             could not be detected: {e}"
        ))
    })?;
    let cwd = std::env::current_dir()?;
    let config = crate::defaults::config_at(&cwd, &|name| std::env::var(name).ok())
        .map_err(Failure::Message)?;
    let preferred = config.and_then(|c| c.defaults.harness);
    let built = branchyard::inventory_default(&inventory, preferred.as_deref(), private_home);
    if built.entry.candidates.is_empty() {
        let why: Vec<String> = built
            .excluded
            .iter()
            .map(|e| format!("{}: {}", e.candidate.harness, e.reason))
            .collect();
        return Err(Failure::Message(format!(
            "with no [fleet], --auto routes among the harnesses logged in here, and none is{}; \
             log one in (by harnesses login ID), or add [fleet.default] with candidates (see \
             docs/fleet.md)",
            match why.is_empty() {
                true => " installed".to_owned(),
                false => format!(": {}", why.join("; ")),
            }
        )));
    }
    Ok(built)
}

/// `task` with the inventory default as its fleet when `--auto` routes it
/// and the configuration has no `[fleet]`: the configuration stays
/// authoritative, and a `[fleet]` turns the default off. A sandbox runs its
/// image's harnesses, which this machine's inventory does not describe, so
/// a sandboxed task needs a `[fleet]`.
pub fn with_inventory_default(task: &mut TaskArgs) -> Result<(), Failure> {
    if !task.auto || task.fleet.is_some() {
        return Ok(());
    }
    if task.sandbox.is_some() || task.substrate.is_some() || task.recipe.is_some() {
        return Err(Failure::Message(
            "with no [fleet], --auto routes among the harnesses logged in on this machine, which \
             says nothing of a sandbox's; add [fleet.default] with candidates (see docs/fleet.md)"
                .into(),
        ));
    }
    let built = inventory_default(task.isolated)?;
    task.fleet = Some(built.fleet());
    task.fleet_excluded = built.excluded;
    Ok(())
}

/// How the router runs for `task`: its kind and seed, fixed now so the
/// route printed before running is the one that runs.
pub fn route_options(task: &TaskArgs, attempts: Option<u32>) -> RouteOptions {
    RouteOptions {
        kind: task.kind,
        seed: Some(task.seed.unwrap_or_else(branchyard::fleet_seed)),
        attempts,
        // `--auto` always fails over; a route implied by [fleet] does when
        // its entry says so.
        failover: task.auto.then_some(true),
        excluded: Default::default(),
        harnesses: None,
    }
}

/// The router's choice, for stderr before the branches start. A route
/// from the inventory default says so, with its candidates.
pub fn route_text(route: &Route, fleet: &Fleet) -> String {
    let matched = match route.matched.is_empty() {
        true => String::new(),
        false => format!(": {}", route.matched.join(", ")),
    };
    let by = match route.entry.as_str() {
        INVENTORY_ENTRY => format!(
            "the inventory default (no [fleet]): candidates {};",
            fleet
                .entries
                .get(INVENTORY_ENTRY)
                .map(|e| {
                    e.candidates
                        .iter()
                        .map(branchyard::FleetCandidate::label)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default()
        ),
        entry => format!("[fleet.{entry}],"),
    };
    let mut text = format!(
        "by: routed as {} ({}{matched}) by {by} seed {}\n",
        route.kind, route.kind_source, route.seed
    );
    for pick in &route.picks {
        text.push_str(&format!(
            "by:   {} — {}\n",
            pick.candidate.label(),
            pick.reason
        ));
    }
    for excluded in &route.excluded {
        text.push_str(&format!(
            "by:   not {}: {}\n",
            excluded.candidate.label(),
            excluded.reason
        ));
    }
    text
}

/// Whether `task` is routed: `--auto`, or a `[fleet]` and no harness.
pub fn is_routed(task: &TaskArgs) -> bool {
    task.auto || task.implied_auto
}

/// Print the route, then run it: one branch, or a fan.
pub fn routed(
    yard: &Yard,
    prompt: &str,
    options: &TaskOptions,
    task: &TaskArgs,
    fan: bool,
    attempts: Option<u32>,
) -> Result<Routed, Failure> {
    let fleet = table(task)?;
    let mut how = route_options(task, attempts);
    // Candidates whose login is near its usage limit (docs/usage.md).
    how.excluded = crate::usage::route_exclusions(fleet);
    // What this machine has of each harness, installing on demand when
    // [harnesses] install = "auto" (docs/harness-lifecycle.md).
    how.harnesses = crate::harness_cmd::route_gate(fleet, options, false);
    let mut route = yard.route(
        prompt,
        options,
        fleet,
        &how,
        match fan {
            true => attempts,
            false => Some(1),
        },
    )?;
    route.excluded.extend(task.fleet_excluded.iter().cloned());
    eprint!("{}", route_text(&route, fleet));
    let routed = match fan {
        true => yard.fan_routed(prompt, options, fleet, &how)?,
        false => yard.run_routed(prompt, options, fleet, &how)?,
    };
    for (from, to) in &routed.failovers {
        eprintln!("by: {from}'s harness failed; the task went on as {to}");
    }
    Ok(routed)
}

/// The judge for `names`: `--harness`, else the [fleet] entry's for their
/// kind, else none.
fn chosen_judge(
    yard: &Yard,
    names: &[String],
    harness: Option<JudgeSpec>,
    fleet: Option<&Fleet>,
) -> Result<Option<JudgeSpec>, Failure> {
    if harness.is_some() {
        return Ok(harness);
    }
    let Some(fleet) = fleet else { return Ok(None) };
    let Some(first) = names.first() else {
        return Ok(None);
    };
    let branch = yard.branch(first)?;
    let kind = recorded_route(&branch.events()?)
        .map_or_else(|| classify(&branch.info().prompt).kind, |d| d.kind);
    Ok(fleet.entry(kind).and_then(|(_, e)| e.judge.clone()))
}

/// Judge `names` and record the scores. A judge harness runs its scratch
/// branch with `options`, the task's: its isolation, provider, secrets and
/// tool approvals, with the policy replaced by one that denies every tool
/// and no observer (the caller's console has finished). Until 16 October
/// 2026 the judge ran with default options, so an isolated fan's judge ran
/// in local mode without the fan's secrets and its provisioning failed.
pub fn judge_names(
    yard: &Yard,
    names: &[String],
    spec: Option<JudgeSpec>,
    rubric: Option<String>,
    fleet: Option<&Fleet>,
    options: &TaskOptions,
) -> Result<Judgement, Failure> {
    let spec = chosen_judge(yard, names, spec, fleet)?;
    let judge_options = TaskOptions {
        observer: None,
        ..options.clone()
    };
    let judge: Option<Arc<dyn Judge>> = spec
        .as_ref()
        .map(|s| branchyard::harness_judge(s, &judge_options));
    match &judge {
        Some(judge) => eprintln!(
            "by: running each attempt's check, then asking {} for a verdict",
            judge.name()
        ),
        None => eprintln!("by: running each attempt's check, then scoring without a model"),
    }
    Ok(yard.judge(
        names,
        &JudgeOptions {
            run_checks: true,
            judge,
            rubric: rubric.or(spec.and_then(|s| s.rubric)),
            record: true,
        },
    )?)
}

/// `by judge`'s ranked table and proposed pick.
pub fn judgement_table(judgement: &Judgement, style: Style) -> String {
    let column = |header, max, right| Column { header, max, right };
    let columns = [
        column("RANK", 4, true),
        column("BRANCH", 40, false),
        column("HARNESS", 16, false),
        column("SCORE", 5, true),
        column("CHECK", 9, false),
        column("STATUS", 16, false),
        column("+/-", 13, true),
        column("COST", 9, true),
        column("TIME", 8, true),
        column("WHY", 60, false),
    ];
    let rows: Vec<Vec<Cell>> = judgement
        .candidates
        .iter()
        .map(|s| {
            let (status, tone) = render::status_text(&s.attempt.status);
            vec![
                Cell::plain(s.rank.to_string()),
                Cell {
                    text: s.attempt.branch.clone(),
                    tone: (judgement.pick.as_deref() == Some(s.attempt.branch.as_str()))
                        .then_some(Tone::Bold),
                },
                Cell::plain(&s.attempt.harness),
                Cell::plain(format!("{:.0}", s.score)),
                Cell {
                    text: s.attempt.check.word().to_owned(),
                    tone: match s.attempt.check.word() {
                        "passed" => Some(Tone::Green),
                        "failed" | "timed out" | "error" => Some(Tone::Red),
                        _ => None,
                    },
                },
                Cell::toned(status, tone),
                Cell::plain(format!(
                    "+{} -{}",
                    s.attempt.insertions, s.attempt.deletions
                )),
                Cell::plain(render::cost_text(s.attempt.cost_usd)),
                Cell::plain(attempts::duration_text(s.attempt.duration_ms)),
                Cell::plain(&s.reason),
            ]
        })
        .collect();
    let mut text = render::table(&columns, &rows, style);
    text.push_str(&format!("\njudged by {}\n", judgement.by.describe()));
    if let branchyard::JudgedBy::Fallback { error, .. } = &judgement.by {
        text.push_str(&format!("  (its answer was not used: {error})\n"));
    }
    match &judgement.pick {
        Some(pick) => text.push_str(&format!("proposed pick: {pick}\n")),
        None => {
            text.push_str("no attempt can be picked: none has a candidate whose check passes\n")
        }
    }
    text
}

/// What `by judge` takes.
pub struct JudgeArgs {
    pub targets: Vec<String>,
    pub harness: Option<String>,
    pub deterministic: bool,
    pub command: Option<Vec<String>>,
    pub rubric: Option<String>,
    pub pick: bool,
    pub into: Option<String>,
    pub discard_others: bool,
    pub yes: bool,
    pub json: bool,
}

/// `by judge <fan|branches...>`.
pub fn judge(env: &Env, target: &Target, args: &JudgeArgs, fleet: Option<&Fleet>) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(local_only());
    }
    let yard = open()?;
    let names = match args.targets.as_slice() {
        [one] => match yard.fan_branches(one) {
            Ok(fan) if !fan.is_empty() => fan,
            _ => vec![one.clone()],
        },
        many => many.to_vec(),
    };
    let spec = args.harness.as_ref().map(|harness| JudgeSpec {
        harness: harness.clone(),
        model: None,
        effort: None,
        command: args.command.clone(),
        rubric: None,
    });
    let fleet = if args.deterministic { None } else { fleet };
    // The judge's branch runs as `by run` would here: the configuration's
    // defaults (isolation, provider, secrets by name) with no flags.
    let mut task = TaskArgs::default();
    let cwd = std::env::current_dir().map_err(|e| Failure::Message(e.to_string()))?;
    if let Some(config) = crate::defaults::config_at(&cwd, &|name| std::env::var(name).ok())
        .map_err(Failure::Message)?
    {
        crate::defaults::apply_task(&config, &mut task, crate::defaults::Scope::NewBranch)
            .map_err(Failure::Message)?;
    }
    let options = crate::commands::Live::start(env, &task, false, None).options(&task)?;
    let judgement = judge_names(&yard, &names, spec, args.rubric.clone(), fleet, &options)?;
    if !args.json {
        print(&judgement_table(&judgement, style(env)))?;
    }
    if !args.pick {
        if args.json {
            return print(&json::text(
                &serde_json::to_value(&judgement).unwrap_or_default(),
            ));
        }
        if let Some(pick) = &judgement.pick {
            print(&format!(
                "\n{}\n  by judge {} --pick\n  by diff {pick}\n",
                style(env).paint(Tone::Dim, "next"),
                args.targets.join(" ")
            ))?;
        }
        return Ok(());
    }
    let Some(pick) = judgement.pick.clone() else {
        return Err(Failure::Message(
            "nothing to pick: no attempt has a candidate whose check passes".into(),
        ));
    };
    let compare = CompareArgs {
        branches: names.clone(),
        fan: None,
        check: false,
        diff: None,
        pick: Some(pick.clone()),
        into: args.into.clone(),
        discard_others: args.discard_others,
        yes: args.yes,
        json: args.json,
    };
    let removed = attempts::pick_and_discard(env, target, &names, &pick, &compare)?;
    if args.json {
        return print(&json::text(&json!({
            "judgement": judgement,
            "picked": pick,
            "removed": removed,
        })));
    }
    Ok(())
}

/// `by fleet stats [--kind K]`.
pub fn stats(env: &Env, target: &Target, kind: Option<TaskKind>, as_json: bool) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(local_only());
    }
    let yard = open()?;
    let stats = fleet_stats(&yard.outcomes(kind)?);
    if as_json {
        let rows: Vec<serde_json::Value> = stats
            .iter()
            .map(|s| {
                let mut value = serde_json::to_value(s).unwrap_or_default();
                value["expected"] = json!(s.expected());
                value
            })
            .collect();
        return print(&json::text(&json!(rows)));
    }
    if stats.is_empty() {
        return print("No outcomes recorded yet: they are recorded as branches finish turns.\n");
    }
    print(&stats_table(&stats, style(env)))
}

pub fn stats_table(stats: &[CandidateStats], style: Style) -> String {
    let column = |header, max, right| Column { header, max, right };
    let columns = [
        column("KIND", 9, false),
        column("CANDIDATE", 40, false),
        column("RUNS", 4, true),
        column("MERGED", 6, true),
        column("BEST", 4, true),
        column("READY", 5, true),
        column("FAILED", 6, true),
        column("INTR", 4, true),
        column("PARENT", 6, true),
        column("P(OK)", 5, true),
        column("COST", 9, true),
        column("TIME", 8, true),
        column("TURNS", 5, true),
        column("SCORE", 5, true),
    ];
    let rows: Vec<Vec<Cell>> = stats
        .iter()
        .map(|s| {
            vec![
                Cell::plain(s.kind.as_str()),
                Cell::plain(s.label()),
                Cell::plain(s.runs.to_string()),
                Cell::plain(s.merged.to_string()),
                Cell::plain(s.judged_best.to_string()),
                Cell::plain(s.ready.to_string()),
                Cell::plain(s.failed.to_string()),
                Cell::plain(s.interrupted.to_string()),
                Cell::plain(s.delegated.to_string()),
                Cell::plain(format!("{:.2}", s.expected())),
                Cell::plain(render::cost_text(s.mean_cost_usd)),
                Cell::plain(attempts::duration_text(s.mean_duration_ms)),
                Cell::plain(s.mean_turns.map_or("-".into(), |t| format!("{t:.1}"))),
                Cell::plain(s.mean_score.map_or("-".into(), |t| format!("{t:.0}"))),
            ]
        })
        .collect();
    let mut text = render::table(&columns, &rows, style);
    text.push_str(
        "\nP(OK) is the posterior mean success the router samples around: merged and judged \
         best count 1, ready its judge score (or 0.5), failed 0; interrupted not at all. Costs, \
         times, turns and scores are means. PARENT counts the runs whose harness a delegating \
         parent chose (by spawn), recorded as the rest are.\n",
    );
    text
}

/// `by fleet advise PROMPT`: inside a harness, as its branch through the
/// engine running it (its task's fleet entry or the harnesses it may run,
/// and its budget); outside, with the configuration's fleet or the
/// inventory default and what this machine has of each harness.
pub fn advise(
    env: &Env,
    target: &Target,
    prompt: &str,
    kind: Option<TaskKind>,
    seed: Option<u64>,
    fleet: Option<&Fleet>,
    as_json: bool,
) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(local_only());
    }
    if let Some(delegate) = crate::commands::harness_delegate(as_json)? {
        return match delegate.advise(prompt, kind) {
            Ok(advice) => show_advice(env, &advice, as_json),
            Err(error) => crate::commands::fail(as_json, &error),
        };
    }
    // No [fleet]: the inventory default, as --auto would route.
    let (fleet, excluded) = match fleet {
        Some(fleet) => (fleet.clone(), Vec::new()),
        None => {
            let cwd = std::env::current_dir()?;
            let isolated = crate::defaults::config_at(&cwd, &|name| std::env::var(name).ok())
                .map_err(Failure::Message)?
                .is_some_and(|c| c.defaults.isolated == Some(true));
            let built = inventory_default(isolated)?;
            (built.fleet(), built.excluded)
        }
    };
    let yard = open()?;
    let options = TaskOptions::default();
    // What this machine has of each harness, installing nothing.
    let gate = crate::harness_cmd::route_gate(&fleet, &options, true);
    let scope = AdviceScope {
        fleet: Some(&fleet),
        seed,
        harnesses: gate.as_deref(),
        ..AdviceScope::default()
    };
    let mut advice = yard.advise(prompt, &options, kind, scope)?;
    for left_out in excluded {
        advice.candidates.push(Advised {
            candidate: left_out.candidate,
            available: false,
            reason: Some(left_out.reason),
            stats: None,
            expected: 0.5,
        });
    }
    show_advice(env, &advice, as_json)
}

fn show_advice(env: &Env, advice: &Advice, as_json: bool) -> Outcome {
    match as_json {
        true => print(&json::text(
            &serde_json::to_value(advice).unwrap_or_default(),
        )),
        false => print(&advice_text(advice, style(env))),
    }
}

/// The advice as a person reads it: the kind, a table of candidates, the
/// router's pick, the caller's own harness and budget, and the notes.
pub fn advice_text(advice: &Advice, style: Style) -> String {
    let matched = match advice.matched.is_empty() {
        true => String::new(),
        false => format!(": {}", advice.matched.join(", ")),
    };
    let among = match advice.entry.as_str() {
        ENVELOPE_ENTRY => "the harnesses this branch may run".to_owned(),
        entry => branchyard::fleet_entry_label(entry),
    };
    let mut text = format!(
        "by: {} ({}{matched}) among {among}, seed {}\n",
        advice.kind, advice.kind_source, advice.seed
    );
    let column = |header, max, right| Column { header, max, right };
    let columns = [
        column("CANDIDATE", 40, false),
        column("RUNS", 4, true),
        column("P(OK)", 5, true),
        column("COST", 9, true),
        column("TIME", 8, true),
        column("TURNS", 5, true),
        column("AVAILABLE", 48, false),
    ];
    let rows: Vec<Vec<Cell>> = advice
        .candidates
        .iter()
        .map(|a| {
            let s = a.stats.as_ref();
            vec![
                Cell::plain(a.candidate.label()),
                Cell::plain(s.map_or("0".to_owned(), |s| s.runs.to_string())),
                Cell::plain(format!("{:.2}", a.expected)),
                Cell::plain(render::cost_text(s.and_then(|s| s.mean_cost_usd))),
                Cell::plain(attempts::duration_text(s.and_then(|s| s.mean_duration_ms))),
                Cell::plain(
                    s.and_then(|s| s.mean_turns)
                        .map_or("-".to_owned(), |t| format!("{t:.1}")),
                ),
                match &a.reason {
                    None => Cell::plain("yes"),
                    Some(why) => Cell::toned(format!("no: {why}"), Tone::Yellow),
                },
            ]
        })
        .collect();
    text.push_str(&render::table(&columns, &rows, style));
    match &advice.pick {
        Some(pick) => text.push_str(&format!(
            "by: pick: {} — {}\n",
            pick.candidate.label(),
            pick.reason
        )),
        None => text.push_str("by: pick: none can run here now\n"),
    }
    if let Some(own) = &advice.own {
        text.push_str(&format!("by: you: {}\n", own.label()));
    }
    if let Some(left) = advice.remaining_usd {
        let tree = advice
            .tree_remaining_usd
            .map(|t| format!(" (${t:.2} under the cost limits above you)"))
            .unwrap_or_default();
        text.push_str(&format!(
            "by: budget: ${left:.2} left to give children{tree}\n"
        ));
    }
    for note in &advice.notes {
        text.push_str(&format!("by: note: {note}\n"));
    }
    text
}

/// `by fleet route PROMPT`.
pub fn route(
    target: &Target,
    prompt: &str,
    how: &RouteOptions,
    fleet: Option<&Fleet>,
    as_json: bool,
) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(local_only());
    }
    // No [fleet]: the inventory default, as --auto would route.
    let (fleet, excluded) = match fleet {
        Some(fleet) => (fleet.clone(), Vec::new()),
        None => {
            let cwd = std::env::current_dir()?;
            let isolated = crate::defaults::config_at(&cwd, &|name| std::env::var(name).ok())
                .map_err(Failure::Message)?
                .is_some_and(|c| c.defaults.isolated == Some(true));
            let built = inventory_default(isolated)?;
            (built.fleet(), built.excluded)
        }
    };
    let yard = open()?;
    let options = TaskOptions::default();
    let how = RouteOptions {
        seed: Some(how.seed.unwrap_or_else(branchyard::fleet_seed)),
        // What this machine has of each harness, installing nothing.
        harnesses: crate::harness_cmd::route_gate(&fleet, &options, true),
        ..how.clone()
    };
    let mut route = yard.route(prompt, &options, &fleet, &how, how.attempts)?;
    route.excluded.extend(excluded);
    match as_json {
        true => print(&json::text(
            &serde_json::to_value(&route).unwrap_or_default(),
        )),
        false => print(&route_text(&route, &fleet).replace("by: routed as", "would route as")),
    }
}
