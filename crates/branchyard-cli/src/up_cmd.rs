//! `by up [--plan] [--smoke] [--yes] [--json]`: bring this repository's configured
//! backends up, the engine's [`branchyard::up`] over this machine, with
//! what `by` knows that the engine's own machine does not: the trust
//! decisions `by recipe trust` and `by workspace trust` record, and `by
//! gateway start`. See docs/up.md.
//!
//! The plan is printed as a table like `by doctor`'s, with what to do
//! about each step that is not ready, then applied unless `--plan`. On a
//! terminal, an install the `[harnesses]` policy leaves to you is asked
//! about first; `--yes` answers them. Exit status: 0 when every step is
//! ready, 1 otherwise. Nothing here prints a secret's value.
//!
//! Below the steps, the seven readiness checks ([`branchyard::up::readiness_checks`])
//! say what this run established and what it did not. `--smoke` adds one
//! read-only Substrate worker listing; it creates nothing and calls no model.

use std::path::{Path, PathBuf};

use branchyard::inventory::{HarnessEvent, Inventory, RunResult};
use branchyard::up::{Action, CheckStatus, LocalMachine, Machine, Options, Plan, Readiness, State};
use branchyard::Yard;
use branchyard_setup::config::Connectors;

use crate::args::Globals;
use crate::commands::{self, print, Env, Failure, Outcome};
use crate::render::{table, Cell, Column, Style, Tone};
use crate::{gateway_cmd, json, recipe_cmd, workspace_cmd};

/// This machine, as `by` sees it.
struct Here {
    inner: LocalMachine,
}

impl Machine for Here {
    fn inventory(&mut self) -> Result<Inventory, String> {
        self.inner.inventory()
    }

    fn detect(&mut self, harness: &str) -> Result<Inventory, String> {
        self.inner.detect(harness)
    }

    fn var_set(&self, name: &str) -> bool {
        self.inner.var_set(name)
    }

    fn file_present(&self, path: &Path) -> bool {
        self.inner.file_present(path)
    }

    fn which(&self, program: &str) -> Option<PathBuf> {
        self.inner.which(program)
    }

    fn listening(&self, url: &str) -> bool {
        self.inner.listening(url)
    }

    fn run(&mut self, command: &str) -> RunResult {
        self.inner.run(command)
    }

    fn start_gateway(&mut self, yard: &Yard, config: &Connectors) -> Result<String, String> {
        match gateway_cmd::start_background(yard, config).map_err(|e| e.to_string())? {
            // The supervisor runs, but its gateway never answered within
            // the start's wait: not up. The engine checks the rest listen.
            gateway_cmd::Started::Spawned {
                listening: false,
                url,
                log,
                ..
            } => Err(format!(
                "the gateway's supervisor runs but nothing listens at {url} yet; see {}",
                log.display()
            )),
            started => Ok(started.url().to_owned()),
        }
    }

    fn microsandbox_host(&self) -> Result<(), String> {
        self.inner.microsandbox_host()
    }

    fn recipe(&self, yard: &Yard, name: &str) -> Readiness {
        let root = std::fs::canonicalize(yard.root()).unwrap_or_else(|_| yard.root().to_path_buf());
        match recipe_cmd::readiness(&root, name) {
            Ok(recipe_cmd::Readiness::Runs) => Readiness::Ready,
            Ok(recipe_cmd::Readiness::Undefined) => Readiness::Undefined,
            Ok(recipe_cmd::Readiness::Untrusted | recipe_cmd::Readiness::Changed) => {
                Readiness::NeedsTrust
            }
            Err(_) => Readiness::Unknown,
        }
    }

    fn workspace(&self, yard: &Yard) -> Readiness {
        match workspace_cmd::resolve(yard.root()) {
            Ok(Some(resolved)) => match resolved.trust {
                workspace_cmd::TrustState::NotNeeded | workspace_cmd::TrustState::Trusted => {
                    Readiness::Ready
                }
                workspace_cmd::TrustState::Untrusted | workspace_cmd::TrustState::Changed => {
                    Readiness::NeedsTrust
                }
            },
            Ok(None) => Readiness::Undefined,
            Err(_) => Readiness::Unknown,
        }
    }

    fn record(&mut self, event: &HarnessEvent) {
        self.inner.record(event);
    }
}

pub fn main(
    env: &Env,
    globals: &Globals,
    plan_only: bool,
    smoke: bool,
    yes: bool,
    as_json: bool,
) -> Outcome {
    if globals.remote.is_some() {
        return Err(crate::errors::Report::usage(
            "by up brings up the repository on this machine; it takes no --remote (a server \
             brings up its own: docs/server.md)",
        )
        .next("run by up without --remote (unset BRANCHYARD_REMOTE)")
        .into());
    }
    if let Some(branch) = workspace_cmd::in_harness() {
        return Err(crate::errors::Report::new(
            "denied",
            format!(
                "a harness running on a branch ({branch}) cannot bring the repository up; that \
                 is a person's command"
            ),
        )
        .next("run by up in your own terminal, outside any branch")
        .into());
    }
    let plan = run(env, plan_only, smoke, yes, as_json)?;
    report(env, &plan, as_json)
}

/// Compute and optionally apply readiness without writing its final report.
/// Setup embeds this result in its own single structured response.
pub(crate) fn run(
    env: &Env,
    plan_only: bool,
    smoke: bool,
    yes: bool,
    as_json: bool,
) -> Result<Plan, Failure> {
    let yard = commands::open().map_err(|failure| match failure {
        // The fix, and where: not "`.` is not inside a git work tree".
        Failure::Sdk(branchyard::Error::NotARepository(_)) => crate::errors::Report::new(
            "not_a_repository",
            crate::init::not_a_repository(&std::env::current_dir().unwrap_or_default()),
        )
        .into(),
        other => other,
    })?;
    let config = yard.configured().unwrap_or_default();
    let interactive = env.stdin_tty && env.stderr_tty && !as_json;
    let options = Options {
        yes,
        interactive,
        smoke,
    };
    let mut machine = Here {
        inner: LocalMachine::new(),
    };
    let progress = (env.stderr_tty && !as_json).then(cliclack::spinner);
    if let Some(progress) = &progress {
        let listing =
            smoke && config.defaults.provider == Some(branchyard::config::ProviderKind::Substrate);
        progress.start(match listing {
            true => {
                "Checking configured backends and credential sources; listing Substrate workers"
            }
            false => "Checking configured backends and credential sources",
        });
    }
    let planned = yard.up_plan(&config, &options, &mut machine);
    if let Some(progress) = &progress {
        match &planned {
            Ok(_) => progress.stop("Readiness checked"),
            Err(_) => progress.error("Readiness check failed; run by doctor"),
        }
    }
    let mut plan = planned?;
    if plan_only {
        return Ok(plan);
    }
    // On a terminal, each install the policy leaves to you is asked about.
    if interactive && !yes {
        let askable: Vec<(String, String)> = plan
            .askable()
            .filter_map(|step| match &step.action {
                Some(Action::Install { plan }) => Some((step.name.clone(), plan.command.clone())),
                _ => None,
            })
            .collect();
        for (name, command) in askable {
            let harness = name.trim_start_matches("harness.");
            let approved = cliclack::confirm(format!("Install {harness} by running `{command}`?"))
                .initial_value(false)
                .interact()
                .map_err(Failure::Io)?;
            if approved {
                plan.approve(&name);
            }
        }
    }
    for step in plan.planned() {
        match &step.action {
            Some(Action::Install { plan }) => eprintln!("by: {}", plan.command),
            Some(Action::StartGateway) => eprintln!("by: starting the connector gateway"),
            None => {}
        }
    }
    let progress =
        (env.stderr_tty && !as_json && plan.planned().next().is_some()).then(cliclack::spinner);
    if let Some(progress) = &progress {
        progress.start("Applying planned changes; checking each result");
    }
    let result = yard.up_apply(&config, plan, &mut machine);
    if let Some(progress) = &progress {
        match &result {
            Ok(applied) if applied.ready() => progress.stop("Configured backends are ready"),
            Ok(_) => progress
                .cancel("Some steps need attention; follow the remedies below, then rerun by up"),
            Err(_) => progress.error("Setup stopped; run by doctor, then rerun by up"),
        }
    }
    result.map_err(Failure::from)
}

/// Print the plan; exit 0 only when every step is ready.
fn report(env: &Env, plan: &Plan, as_json: bool) -> Outcome {
    match as_json {
        true => print(&json::text(&plan.json()))?,
        false => print(&text(plan, env.style()))?,
    }
    match plan.ready() {
        true => Ok(()),
        false => Err(Failure::Reported),
    }
}

fn tone(state: State) -> Tone {
    match state {
        State::Ready => Tone::Green,
        State::Planned => Tone::Bold,
        State::NeedsPerson => Tone::Yellow,
        State::Blocked => Tone::Red,
    }
}

fn check_tone(status: CheckStatus) -> Tone {
    match status {
        CheckStatus::NotApplicable => Tone::Dim,
        CheckStatus::Verified => Tone::Green,
        CheckStatus::Unverified | CheckStatus::Unknown => Tone::Yellow,
        CheckStatus::Failed => Tone::Red,
    }
}

/// The seven readiness checks as a table under a heading; empty for none.
pub(crate) fn checks_table(checks: &[branchyard::up::ReadinessCheck], style: Style) -> String {
    if checks.is_empty() {
        return String::new();
    }
    let columns = [
        Column {
            header: "STATUS",
            max: 14,
            right: false,
        },
        Column {
            header: "CHECK",
            max: 44,
            right: false,
        },
        Column {
            header: "DETAIL",
            max: usize::MAX,
            right: false,
        },
    ];
    let rows: Vec<Vec<Cell>> = checks
        .iter()
        .map(|c| {
            vec![
                Cell::toned(c.status.as_str(), check_tone(c.status)),
                Cell::plain(&c.label),
                Cell::plain(&c.detail),
            ]
        })
        .collect();
    format!("\nReadiness checks:\n{}", table(&columns, &rows, style))
}

/// A table of the steps, what to do about each that is not ready, and a
/// summary line.
pub(crate) fn text(plan: &Plan, style: Style) -> String {
    let columns = [
        Column {
            header: "STATE",
            max: 12,
            right: false,
        },
        Column {
            header: "STEP",
            max: 28,
            right: false,
        },
        Column {
            header: "DETAIL",
            max: usize::MAX,
            right: false,
        },
    ];
    let rows: Vec<Vec<Cell>> = plan
        .steps
        .iter()
        .map(|s| {
            vec![
                Cell::toned(s.state.as_str(), tone(s.state)),
                Cell::plain(&s.name),
                Cell::plain(&s.detail),
            ]
        })
        .collect();
    let mut out = table(&columns, &rows, style);
    out.push_str(&checks_table(&plan.checks, style));
    let todo: Vec<&branchyard::up::Step> = plan.steps.iter().filter(|s| s.todo.is_some()).collect();
    if !todo.is_empty() {
        out.push_str("\nTo do:\n");
        for step in &todo {
            out.push_str(&format!(
                "  {}: {}\n",
                step.name,
                step.todo.as_deref().unwrap_or_default()
            ));
        }
    }
    let to_verify: Vec<(&str, &str)> = plan
        .checks
        .iter()
        .filter_map(|c| Some((c.label.as_str(), c.next.as_deref()?)))
        .filter(|(_, next)| !todo.iter().any(|s| s.todo.as_deref() == Some(*next)))
        .collect();
    if !to_verify.is_empty() {
        out.push_str("\nTo verify:\n");
        for (label, next) in to_verify {
            out.push_str(&format!("  {label}: {next}\n"));
        }
    }
    let count = |state: State| plan.steps.iter().filter(|s| s.state == state).count();
    let (person, blocked) = (count(State::NeedsPerson), count(State::Blocked));
    let summary = match (plan.ready(), person, blocked) {
        (true, _, _) => "ready: every step is in place".to_owned(),
        (false, 0, 0) => format!("{} step(s) planned", count(State::Planned)),
        (false, person, 0) => format!("{person} step(s) need you"),
        (false, 0, blocked) => format!("{blocked} step(s) blocked"),
        (false, person, blocked) => format!("{person} step(s) need you, {blocked} blocked"),
    };
    out.push('\n');
    out.push_str(&summary);
    out.push('\n');
    if !plan.checks.is_empty() {
        let counts: Vec<String> = [
            CheckStatus::Verified,
            CheckStatus::Unverified,
            CheckStatus::Unknown,
            CheckStatus::Failed,
            CheckStatus::NotApplicable,
        ]
        .into_iter()
        .filter_map(|status| {
            let n = plan.checks.iter().filter(|c| c.status == status).count();
            (n > 0).then(|| format!("{n} {}", status.as_str().replace('_', " ")))
        })
        .collect();
        out.push_str(&format!("checks: {}\n", counts.join(", ")));
    }
    out
}
