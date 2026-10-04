//! Routing and judging from the command line: what `by run --auto` and
//! `by fan --auto` print about the router's choice, `by judge`, and `by
//! fleet stats|route`. The decisions are the SDK's (`Yard::run_routed`,
//! `Yard::judge`); this renders them. See docs/fleet.md.

use std::sync::Arc;

use branchyard::{
    classify, fleet_stats, recorded_route, CandidateStats, Fleet, Judge, JudgeOptions, JudgeSpec,
    Judgement, Route, RouteOptions, Routed, TaskKind, TaskOptions, Yard,
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

/// The router's choice, for stderr before the branches start.
pub fn route_text(route: &Route) -> String {
    let matched = match route.matched.is_empty() {
        true => String::new(),
        false => format!(": {}", route.matched.join(", ")),
    };
    let mut text = format!(
        "by: routed as {} ({}{matched}) by [fleet.{}], seed {}\n",
        route.kind, route.kind_source, route.entry, route.seed
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
    let route = yard.route(
        prompt,
        options,
        fleet,
        &how,
        match fan {
            true => attempts,
            false => Some(1),
        },
    )?;
    eprint!("{}", route_text(&route));
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
#[allow(clippy::map_unwrap_or)] // ratchet: branchyard-cli
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
        .map(|d| d.kind)
        .unwrap_or_else(|| classify(&branch.info().prompt).kind);
    Ok(fleet.entry(kind).and_then(|(_, e)| e.judge.clone()))
}

/// Judge `names` and record the scores.
pub fn judge_names(
    yard: &Yard,
    names: &[String],
    spec: Option<JudgeSpec>,
    rubric: Option<String>,
    fleet: Option<&Fleet>,
) -> Result<Judgement, Failure> {
    let spec = chosen_judge(yard, names, spec, fleet)?;
    let judge: Option<Arc<dyn Judge>> = spec
        .as_ref()
        .map(|s| branchyard::harness_judge(s, &TaskOptions::default()));
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
    let judgement = judge_names(&yard, &names, spec, args.rubric.clone(), fleet)?;
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
         times, turns and scores are means.\n",
    );
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
    let fleet = fleet.ok_or_else(|| {
        Failure::Message(
            "branchyard.toml has no [fleet] table; add [fleet.default] (see docs/fleet.md)".into(),
        )
    })?;
    let yard = open()?;
    let options = TaskOptions::default();
    let how = RouteOptions {
        seed: Some(how.seed.unwrap_or_else(branchyard::fleet_seed)),
        // What this machine has of each harness, installing nothing.
        harnesses: crate::harness_cmd::route_gate(fleet, &options, true),
        ..how.clone()
    };
    let route = yard.route(prompt, &options, fleet, &how, how.attempts)?;
    match as_json {
        true => print(&json::text(
            &serde_json::to_value(&route).unwrap_or_default(),
        )),
        false => print(&route_text(&route).replace("by: routed as", "would route as")),
    }
}
