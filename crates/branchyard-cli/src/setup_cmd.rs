//! `by setup`: one idempotent command from a fresh repository to a working
//! `by run`. It composes the engines that already exist and adds no logic
//! of its own to them:
//!
//! 1. **detect**: the repository, the harnesses on `PATH` with their
//!    versions and login state, and the check command the files suggest
//!    ([`Facts::gather`] over the same [`HostProbe`] `by init` uses);
//! 2. **config**: when there is no `branchyard.toml`, the `project` topic
//!    with its defaults ([`branchyard_setup::next`]); on a terminal only the
//!    questions whose answer cannot be detected are asked;
//! 3. **hooks**: the `plugin` topic's per-harness install for every harness
//!    on `PATH` that has a manifest ([`crate::init::plugin_response`], the
//!    path `by init plugin --all-detected` takes), one plan per harness so a
//!    file that differs holds back that harness alone; then a model-free
//!    check that each harness's hook command answers;
//! 4. **logins**: the command to run for each harness whose login is not
//!    verified. Setup never starts a login flow;
//! 5. **summary**: what was detected, written and installed, what only the
//!    person can do, and the next command.
//!
//! Without a terminal and without `--yes` it behaves as `--dry-run`. A
//! second run writes nothing and says so. It exits 0 when it is done, even
//! with things left for the person; non-zero only on a real error.
//!
//! The same module draws the status card bare `by` prints ([`card`]) and
//! `by init plugin --status` ([`plugin_status`]). See `docs/setup.md`.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use branchyard::glance;
use branchyard_controls::catalog;
use branchyard_setup::config::{self, WorkspaceConfig};
use branchyard_setup::install::{self, Manifest};
use branchyard_setup::plan::{FileAction, Plan};
use branchyard_setup::probe::{harness_label, unapproved_tools, Facts, HarnessFact};
use branchyard_setup::Topic;
use serde_json::{json, Value};

use crate::args::SetupFlags;
use crate::commands::{print, Env, Failure, Outcome};
use crate::render::{table, Cell, Column, Tone};
use crate::setup_io::{self, which, CachedProbe, HostProbe, HostValidator, RandomEntropy};

/// The task `Next:` suggests; a prompt a newcomer can replace at once.
const EXAMPLE_TASK: &str = "fix the flaky test";
/// How long the canned hook check waits for `by hook` (which gives up
/// after two seconds itself).
const HOOK_WAIT: Duration = Duration::from_secs(6);
/// Notes and long lines wrap at this width.
const WRAP: usize = 96;
/// The width of the label column in the summary.
const LABEL: usize = 10;

// ---------------------------------------------------------------- the report

/// How far one step got.
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    /// Nothing to write: already as planned.
    Current,
    /// To be written, and not yet.
    Pending,
    /// Written by this run.
    Done,
    /// Cannot be written, and why.
    Refused(String),
}

/// What happens to `branchyard.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ConfigState {
    /// `--no-config`.
    Skipped,
    /// It exists and is left as it is.
    Exists,
    /// To be written.
    Pending,
    /// Written by this run.
    Done,
}

/// The project defaults step.
#[derive(Clone)]
struct ConfigStep {
    /// Harnesses run in remote actors rather than on the host.
    substrate: bool,
    /// Harnesses run on the configured HTTP or SSH server.
    server: bool,
    path: String,
    state: ConfigState,
    /// `harness codex · check "cargo test" · local, no isolation`.
    summary: String,
    plan: Option<Plan>,
    /// The default harness the file names (or would name).
    harness: Option<String>,
    /// The check command the file names (or would name).
    check: Option<String>,
    /// The `[workspace]` the plan writes, when it has scripts.
    scripts: Option<WorkspaceConfig>,
    /// The scripts were trusted by this run.
    trusted: bool,
}

/// One place a harness's install writes: a file, a directory of files, or
/// a merge into the person's own file.
#[derive(Clone)]
struct Target {
    path: String,
    how: String,
    action: &'static str,
    files: usize,
}

/// A harness's hook command, run once with a canned input.
#[derive(Clone, Debug, PartialEq, Eq)]
enum HookCheck {
    /// It answered; the detail says how.
    Verified(String),
    /// It will not speak, and why.
    Silent(String),
}

/// One harness's install.
#[derive(Clone)]
struct HookStep {
    id: String,
    label: String,
    provides: Vec<String>,
    notes: Vec<String>,
    state: State,
    plan: Option<Plan>,
    targets: Vec<Target>,
    /// Files this run writes (or would).
    writes: usize,
    check: Option<HookCheck>,
}

/// One thing only the person can do.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Todo {
    /// `login`, `hooks`, `note`, `workspace` or `install`.
    kind: &'static str,
    harness: Option<String>,
    text: String,
    command: Option<String>,
}

/// Everything `by setup` found and did, from which both renderings are made.
struct Report {
    facts: Facts,
    /// The repository root.
    root: PathBuf,
    /// Nothing was written (asked for, or no terminal to ask on).
    dry_run: bool,
    /// The person asked for `--dry-run` rather than having no terminal.
    asked_dry: bool,
    /// A terminal, for the next command to be `by setup` rather than `--yes`.
    interactive: bool,
    config: ConfigStep,
    hooks: Vec<HookStep>,
    /// The paths written, as the plans name them.
    written: Vec<String>,
    unchanged: Vec<String>,
    todo: Vec<Todo>,
    next: String,
    readiness: Option<branchyard::up::Plan>,
}

// ------------------------------------------------------------------- command

/// `by setup`.
pub fn main(env: &Env, args: &SetupFlags) -> Outcome {
    if let Some(branch) = crate::workspace_cmd::in_harness() {
        return Err(Failure::Message(format!(
            "by setup is a person's decision, made outside any branch (this is {branch}).\n\
             Next: run it from your own terminal"
        )));
    }
    crate::onboarding::directory(env, args)?;
    let cwd = std::env::current_dir()?;
    let interactive = env.stdin_tty && env.stdout_tty && !args.json;
    let writing = args.yes && !args.dry_run;
    let asking = !args.yes && !args.dry_run && interactive;
    let dry_run = !writing && !asking;
    if !args.no_config && !setup_io::project_root(&cwd).join(".git").exists() {
        return Err(Failure::Message(format!(
            "{} is not inside a git repository, and branchyard.toml belongs to one.\n\
             Next: cd into a repository (or git init here) and run by setup; \
             by setup --no-config installs hooks alone",
            cwd.display()
        )));
    }
    if interactive {
        eprintln!("Looking at this machine and repository...");
    }
    let probe = CachedProbe::new(HostProbe::new(&cwd));
    let facts = Facts::gather(&probe);
    let root = PathBuf::from(&facts.root);
    if !args.json {
        print(&detected_text(&facts))?;
    }

    let answers = crate::onboarding::answers(env, args, &facts)?;
    let config = plan_config(args, asking, &facts, &probe, answers)?;
    let hooks = plan_hooks(args, &facts, &probe)?;
    if !args.harnesses.is_empty() {
        // Named on purpose: a refusal is an error, not a line to read later.
        if let Some(step) = hooks.iter().find(|h| matches!(h.state, State::Refused(_))) {
            if let State::Refused(why) = &step.state {
                return Err(Failure::Message(format!(
                    "{}: {why}\nNext: by init plugin {} --dry-run",
                    step.label, step.id
                )));
            }
        }
    }
    let writes = config
        .plan
        .iter()
        .flat_map(|p| &p.files)
        .filter(|f| f.writes())
        .count()
        + hooks.iter().map(|h| h.writes).sum::<usize>();
    let mut report = Report {
        facts,
        root,
        dry_run,
        asked_dry: args.dry_run,
        interactive,
        config,
        hooks,
        written: Vec::new(),
        unchanged: Vec::new(),
        todo: Vec::new(),
        next: String::new(),
        readiness: None,
    };

    let scripts = report.config.scripts.is_some() && report.config.state == ConfigState::Pending;
    let mut trust = false;
    if asking && writes > 0 {
        print(&format!("\n{}", changes_text(&report, true)))?;
        let go = cliclack::confirm(format!("Write {writes} file(s)?"))
            .initial_value(true)
            .interact()
            .map_err(crate::wizard::cancelled)?;
        if !go {
            cliclack::outro_cancel("Nothing was written.").map_err(Failure::Io)?;
            return Ok(());
        }
        if scripts {
            trust = cliclack::confirm(
                "Trust the [workspace] scripts above now? They run as you in every new \
                 worktree; `by workspace untrust` takes it back.",
            )
            .initial_value(false)
            .interact()
            .map_err(crate::wizard::cancelled)?;
        }
    } else if writing {
        // The person saw what the defaults write and asked for them.
        trust = scripts;
    }

    if !dry_run {
        apply(&mut report)?;
        if trust {
            trust_scripts(&mut report.config, &report.root)?;
        }
        let root = report.root.clone();
        for step in &mut report.hooks {
            if matches!(step.state, State::Done | State::Current) {
                step.check = Some(check_hook(&step.id, &root));
            }
        }
    }
    finish(&mut report);
    if (args.up || args.guided) && !dry_run {
        report.readiness = Some(readiness(env, args, &report.root)?);
        report.next = match report.readiness.as_ref().is_some_and(|plan| plan.ready()) {
            true => run_command(
                report.config.check.as_deref(),
                default_harness(&report).as_deref(),
            ),
            false => "by setup --up".into(),
        };
    }
    let ready = report.readiness.as_ref().is_none_or(|plan| plan.ready());
    if args.json {
        print(&format!("{}\n", crate::json::text(&report_json(&report))))?;
    } else {
        print(&format!("\n{}", card_text(&report)))?;
        if let Some(plan) = &report.readiness {
            print(&crate::up_cmd::text(plan, env.style()))?;
        }
    }
    if ready {
        Ok(())
    } else {
        Err(Failure::Reported)
    }
}

/// Check the configured execution machine; a server's harnesses live remotely.
fn readiness(env: &Env, args: &SetupFlags, root: &Path) -> Result<branchyard::up::Plan, Failure> {
    let config = branchyard::configure::configuration(root)?
        .map(|located| located.config)
        .unwrap_or_default();
    if config.remote.url.is_none() {
        return crate::up_cmd::run(env, false, args.yes, args.json);
    }
    let mut globals = crate::args::Globals::default();
    crate::defaults::apply_globals(&config, &mut globals);
    let remote = crate::remote::Remote::connect(&globals)?;
    let inventory = remote.client.inventory()?;
    let harness = config.defaults.harness.as_deref().unwrap_or("claude-code");
    Ok(remote_plan(&inventory, remote.repo.name(), harness))
}

/// Convert advertised worker evidence into setup's shared readiness shape.
fn remote_plan(
    inventory: &branchyard_client::api::InventoryReport,
    repository: &str,
    harness: &str,
) -> branchyard::up::Plan {
    use branchyard::inventory::LoginState;
    use branchyard::up::{Plan, Step};

    let mut steps = vec![Step::ready(
        "remote.connection",
        "server authenticated and inventory received",
    )];
    let installed = inventory
        .workers
        .iter()
        .filter(|worker| worker.repos.iter().any(|name| name == repository))
        .filter_map(|worker| worker.inventory.as_ref())
        .flat_map(|inventory| &inventory.harnesses)
        .find(|state| state.id == harness && state.on_path);
    match installed {
        Some(state) => {
            steps.push(Step::ready(
                format!("remote.harness.{harness}"),
                format!("{} on remote worker", state.path),
            ));
            if state.login.state == LoginState::LoggedIn {
                steps.push(Step::ready(
                    format!("remote.login.{harness}"),
                    "remote credential source detected; model authentication unverified",
                ));
            } else {
                steps.push(Step::needs_person(format!("remote.login.{harness}"), "remote model credentials not verified", format!("configure credentials on the worker with by harnesses login {harness}, then rerun by setup --up"), None));
            }
        }
        None => steps.push(Step::blocked(
            format!("remote.harness.{harness}"),
            "no serving worker advertises this harness on PATH",
            format!("install {harness} on the worker, then rerun by setup --up"),
        )),
    }
    Plan { steps }
}

// ----------------------------------------------------------------- detecting

/// The `Detected` block.
fn detected_text(facts: &Facts) -> String {
    let mut out = String::from("Detected\n");
    row(
        &mut out,
        "repository",
        &match &facts.git {
            Some(git) => match &git.default_branch {
                Some(branch) => format!("{} (default branch {branch})", git.root),
                None => git.root.clone(),
            },
            None => format!("{} (not a git repository)", facts.root),
        },
    );
    let found: Vec<String> = facts.installed().map(harness_phrase).collect();
    row(
        &mut out,
        "harnesses",
        &match found.is_empty() {
            true => "none on PATH".to_owned(),
            false => found.join("; "),
        },
    );
    if let Some(check) = &facts.suggested_check {
        row(&mut out, "check", check);
    }
    out
}

/// `Claude Code 2.1.3, login not verified`: the harness, its version and
/// its login in the words `by harnesses` uses.
fn harness_phrase(h: &HarnessFact) -> String {
    let named = match &h.version {
        Some(v) if !v.is_empty() => format!("{} {v}", harness_label(&h.harness)),
        _ => harness_label(&h.harness),
    };
    format!("{named}, {}", login_words(h))
}

/// A login in the words of the `by harnesses` table (`logged in`, `key set
/// (VAR)`, `credentials found`, `logged out`, `login not verified`,
/// `unknown`): what Branchyard saw, and no more.
fn login_words(h: &HarnessFact) -> String {
    let Some(login) = &h.login else {
        return "login not checked".into();
    };
    if !login.label.is_empty() {
        return login.label.clone();
    }
    // A probe without the label: the same words from what it did record.
    match (login.state.as_str(), login.evidence.as_str()) {
        ("logged_in", "verified") => "logged in".into(),
        ("logged_in", _) => "credentials found".into(),
        ("logged_out", "verified") => "logged out".into(),
        ("logged_out", _) => "login not verified".into(),
        _ => "unknown".into(),
    }
}

/// Where a harness's login comes from, as far as Branchyard could tell:
/// what the status command or the credential files said, else `unknown`.
fn auth_source(h: &HarnessFact) -> String {
    match &h.login {
        Some(login) if login.state == "logged_in" && !login.detail.is_empty() => {
            login.detail.clone()
        }
        _ => "unknown".into(),
    }
}

/// Whether the login is known: logged in, or logged out, on the harness's
/// own word.
fn login_verified(h: &HarnessFact) -> bool {
    h.login.as_ref().is_some_and(|l| l.evidence == "verified")
}

/// What was checked and found nothing: the detail of an unverified login.
fn checked(h: &HarnessFact) -> String {
    match &h.login {
        Some(login) if !login.detail.is_empty() => login.detail.clone(),
        _ => "no check ran".into(),
    }
}

// ------------------------------------------------------------------- config

/// The project defaults: planned with the engine's own defaults, over the
/// few answers a terminal gives.
fn plan_config(
    args: &SetupFlags,
    asking: bool,
    facts: &Facts,
    probe: &CachedProbe<HostProbe>,
    guided_answers: BTreeMap<String, Value>,
) -> Result<ConfigStep, Failure> {
    let path = config::PROJECT_FILE.to_owned();
    let none = |state: ConfigState, summary: String| ConfigStep {
        substrate: false,
        server: false,
        path: path.clone(),
        state,
        summary,
        plan: None,
        harness: None,
        check: None,
        scripts: None,
        trusted: false,
    };
    if args.no_config {
        return Ok(none(ConfigState::Skipped, String::new()));
    }
    if facts.project_config {
        let current = facts
            .project_text
            .as_deref()
            .and_then(|text| config::parse(text).ok())
            .unwrap_or_default();
        let mut step = none(ConfigState::Exists, String::new());
        step.substrate = current.defaults.provider == Some(config::ProviderKind::Substrate);
        step.server = current.remote.url.is_some();
        step.harness = current.defaults.harness;
        step.check = current.defaults.check;
        return Ok(step);
    }
    let raw = match asking && !args.guided {
        true => ask_what_cannot_be_detected(facts)?,
        false => guided_answers,
    };
    let response = branchyard_setup::next(
        Topic::Project,
        probe,
        &raw,
        true,
        &mut RandomEntropy,
        &HostValidator,
    );
    let Some(plan) = response.plan else {
        return Err(Failure::Message(
            "the project defaults need answers setup could not default.\n\
             Next: by init project"
                .into(),
        ));
    };
    if !plan.valid {
        let why: Vec<String> = plan
            .files
            .iter()
            .filter(|f| !f.validation.ok)
            .map(|f| format!("{}: {}", f.path, f.validation.messages.join("; ")))
            .collect();
        return Err(Failure::Message(format!(
            "branchyard.toml would not load: {}\nNext: by init project",
            why.join("; ")
        )));
    }
    let answer = |id: &str| {
        response
            .answers
            .get(id)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let harness = answer("harness");
    let check = answer("check");
    let isolated = response
        .answers
        .get("isolated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let summary = [
        harness.as_ref().map(|h| format!("harness {h}")),
        Some(match &check {
            Some(check) => format!("check \"{check}\""),
            None => "no check".to_owned(),
        }),
        Some(if answer("provider").as_deref() == Some("substrate") {
            "remote Agent Substrate, private home".to_owned()
        } else {
            match isolated {
                true => "isolated".to_owned(),
                false => "local, no isolation".to_owned(),
            }
        }),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let scripts = plan
        .files
        .first()
        .and_then(|f| f.content.as_deref())
        .and_then(|text| config::parse(text).ok())
        .and_then(|c| c.workspace)
        .filter(WorkspaceConfig::has_scripts);
    let state = match plan.files.iter().any(|f| f.writes()) {
        true => ConfigState::Pending,
        false => ConfigState::Exists,
    };
    Ok(ConfigStep {
        substrate: answer("provider").as_deref() == Some("substrate"),
        server: answer("remote").as_deref() == Some("server"),
        path,
        state,
        summary,
        plan: Some(plan),
        harness,
        check,
        scripts,
        trusted: false,
    })
}

/// On a terminal, the questions whose answer cannot be detected: the
/// default harness when several are logged in, a check when the files
/// suggest none, and isolation. Everything else takes the engine's default.
fn ask_what_cannot_be_detected(facts: &Facts) -> Result<BTreeMap<String, Value>, Failure> {
    let mut raw = BTreeMap::new();
    let questions = branchyard_setup::questions(Topic::Project, facts, &BTreeMap::new());
    let find = |id: &str| questions.iter().find(|q| q.id == id).cloned();
    let mut ask = |q: &branchyard_setup::Question| -> Result<(), Failure> {
        let value = crate::wizard::ask(q).map_err(crate::wizard::cancelled)?;
        raw.insert(q.id.clone(), value);
        Ok(())
    };
    let usable: Vec<&str> = facts.logged_in().map(|h| h.harness.as_str()).collect();
    if usable.len() > 1 {
        if let Some(mut q) = find("harness") {
            let all: Vec<_> = q
                .choices
                .drain(..)
                .chain(q.more_choices.drain(..))
                .collect();
            q.choices = all
                .into_iter()
                .filter(|c| c.value.as_str().is_some_and(|v| usable.contains(&v)))
                .collect();
            q.default = match q.default.as_str().is_some_and(|d| usable.contains(&d)) {
                true => q.default.clone(),
                false => Value::from(usable[0]),
            };
            q.prompt =
                "Which of the harnesses you are logged in to should new branches use?".into();
            ask(&q)?;
        }
    }
    if facts.suggested_check.is_none() {
        if let Some(q) = find("check") {
            ask(&q)?;
        }
    }
    if let Some(q) = find("isolated") {
        ask(&q)?;
    }
    if let Some(q) = find("provider") {
        ask(&q)?;
    }
    if raw.get("provider").and_then(Value::as_str) == Some("substrate") {
        for id in [
            "harness",
            "substrate.endpoint",
            "substrate.router",
            "substrate.atespace",
            "substrate.template",
            "substrate.key",
            "substrate.api_token_file",
            "substrate.ca",
            "substrate.server_name",
            "substrate.egress",
            "secrets",
        ] {
            let questions = branchyard_setup::questions(Topic::Project, facts, &raw);
            if let Some(q) = questions.iter().find(|q| q.id == id) {
                let value = crate::wizard::ask(q).map_err(crate::wizard::cancelled)?;
                raw.insert(id.into(), value);
            }
        }
        let questions = branchyard_setup::questions(Topic::Project, facts, &raw);
        for q in questions.iter().filter(|q| q.id.starts_with("secret.")) {
            let value = crate::wizard::ask(q).map_err(crate::wizard::cancelled)?;
            raw.insert(q.id.clone(), value);
        }
    }
    Ok(raw)
}

// -------------------------------------------------------------------- hooks

/// Each harness's install, planned: those on `PATH` that have a manifest,
/// or the ones `--harness` names.
fn plan_hooks(
    args: &SetupFlags,
    facts: &Facts,
    probe: &CachedProbe<HostProbe>,
) -> Result<Vec<HookStep>, Failure> {
    if args.no_hooks {
        return Ok(Vec::new());
    }
    let manifests = install::manifests().map_err(Failure::Message)?;
    let ids: Vec<String> = match args.harnesses.is_empty() {
        true => facts.install_detected.clone(),
        false => args.harnesses.clone(),
    };
    let mut steps = Vec::new();
    for id in ids {
        let Some(manifest) = manifests.iter().find(|m| m.harness == id) else {
            let known: Vec<&str> = manifests.iter().map(|m| m.harness.as_str()).collect();
            return Err(Failure::Message(format!(
                "no install for harness {id:?}; by setup installs for: {}\n\
                 Next: by init plugin lists them with what each gets",
                known.join(", ")
            )));
        };
        steps.push(plan_hook(manifest, facts, probe));
    }
    Ok(steps)
}

/// One harness's plan, through the path `by init plugin` takes.
fn plan_hook(manifest: &Manifest, facts: &Facts, probe: &CachedProbe<HostProbe>) -> HookStep {
    let response =
        crate::init::plugin_response(probe, vec![manifest.harness.clone()], BTreeMap::new());
    let mut step = HookStep {
        id: manifest.harness.clone(),
        label: harness_label(&manifest.harness),
        provides: manifest.provides.clone(),
        notes: manifest.notes.iter().map(|n| n.text.clone()).collect(),
        state: State::Current,
        plan: None,
        targets: Vec::new(),
        writes: 0,
        check: None,
    };
    let Some(plan) = response.plan else {
        step.state = State::Refused("the install finished without a plan".into());
        return step;
    };
    if !plan.valid {
        let why: Vec<String> = plan
            .files
            .iter()
            .filter_map(|f| {
                f.refusal
                    .clone()
                    .or_else(|| (!f.validation.ok).then(|| f.validation.messages.join("; ")))
                    .map(|why| format!("{}: {why}", f.path))
            })
            .collect();
        step.state = State::Refused(format!("cannot be installed ({})", why.join("; ")));
        return step;
    }
    let differing = plan.overwrites();
    if !differing.is_empty() {
        let paths: Vec<&str> = differing.iter().map(|f| f.path.as_str()).collect();
        step.state = State::Refused(format!(
            "these files differ from the packaged copy and were not replaced: {}",
            paths.join(", ")
        ));
        return step;
    }
    step.writes = plan.files.iter().filter(|f| f.writes()).count();
    step.targets = targets(manifest, &plan, facts);
    step.state = match step.writes {
        0 => State::Current,
        _ => State::Pending,
    };
    step.plan = Some(plan);
    step
}

/// A plan's files grouped by the manifest entry that made them.
fn targets(manifest: &Manifest, plan: &Plan, facts: &Facts) -> Vec<Target> {
    let mut out = Vec::new();
    for entry in &manifest.files {
        let path = install::expand(&entry.to, facts);
        let files: Vec<_> = plan
            .files
            .iter()
            .filter(|f| f.path == path || f.path.starts_with(&format!("{path}/")))
            .collect();
        if files.is_empty() {
            continue;
        }
        let action = match files.iter().all(|f| f.action == FileAction::Unchanged) {
            true => "unchanged",
            false => match files.iter().all(|f| f.action == FileAction::Create) {
                true => "create",
                false => "update",
            },
        };
        out.push(Target {
            path,
            how: files[0].how.clone().unwrap_or_default(),
            action,
            files: files.len(),
        });
    }
    out
}

/// Write the config and each harness's files. A harness that cannot be
/// installed is left as its plan said.
fn apply(report: &mut Report) -> Result<(), Failure> {
    let root = report.root.clone();
    let mut written = Vec::new();
    let mut unchanged = Vec::new();
    if report.config.state == ConfigState::Pending {
        if let Some(plan) = report.config.plan.clone() {
            let applied = write_plan(&plan, &root)?;
            written.extend(applied.written);
            unchanged.extend(applied.unchanged);
            report.config.state = ConfigState::Done;
        }
    }
    for step in &mut report.hooks {
        let Some(plan) = step.plan.clone() else {
            continue;
        };
        match step.state {
            State::Pending => {
                let applied = write_plan(&plan, &root)?;
                written.extend(applied.written);
                unchanged.extend(applied.unchanged);
                step.state = State::Done;
            }
            State::Current => unchanged.extend(plan.files.iter().map(|f| f.path.clone())),
            State::Done | State::Refused(_) => {}
        }
    }
    report.written = written;
    report.unchanged = unchanged;
    Ok(())
}

/// One plan written under `root`, never replacing a file that differs.
fn write_plan(plan: &Plan, root: &Path) -> Result<branchyard_setup::protocol::Applied, Failure> {
    setup_io::apply(plan, root, false).map_err(|refusal| match refusal {
        setup_io::Refusal::Invalid(problems) | setup_io::Refusal::WouldOverwrite(problems) => {
            Failure::Message(format!(
                "nothing more was written: {}\nNext: by init plugin --all-detected --dry-run",
                problems.join("; ")
            ))
        }
        setup_io::Refusal::Io(message) => Failure::Message(message),
    })
}

/// Trust the `[workspace]` scripts setup itself just wrote: the person saw
/// them (the review, or `--yes` taking the defaults) and the summary says so.
fn trust_scripts(config: &mut ConfigStep, root: &Path) -> Result<(), Failure> {
    if config.state != ConfigState::Done || config.scripts.is_none() {
        return Ok(());
    }
    if let Some(resolved) = crate::workspace_cmd::resolve(root)? {
        if matches!(
            resolved.trust,
            crate::workspace_cmd::TrustState::Untrusted | crate::workspace_cmd::TrustState::Changed
        ) {
            crate::workspace_cmd::trust(&resolved)?;
            config.trusted = true;
        }
    }
    Ok(())
}

// --------------------------------------------------------------- hook check

/// The hook dialect a harness's install speaks: its own, or `generic` for
/// the plugins written in code.
fn dialect_of(harness: &str) -> &str {
    match harness {
        "pi" | "oh-my-pi" | "opencode" | "amp" => "generic",
        other => other,
    }
}

/// Run `by hook` as the harness would, once, with a canned input and no
/// model: `by` must be on `PATH` (the hook commands start with `command -v
/// by`), and it must answer in the harness's dialect without complaint.
/// Whether the harness itself runs the hook (a trust prompt, a restart) is
/// not something this can see; the notes say it.
fn check_hook(harness: &str, repo: &Path) -> HookCheck {
    let dialect = dialect_of(harness);
    let Ok(known) = crate::hook_cmd::dialect(Some(dialect)) else {
        return HookCheck::Silent(format!("by hook has no dialect for {harness}"));
    };
    let Some(by) = which("by") else {
        let here = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.display().to_string()))
            .unwrap_or_else(|| "the directory of by".into());
        return HookCheck::Silent(format!(
            "`by` is not on PATH, and every hook starts with `command -v by`, so the hooks \
             do nothing; add {here} to PATH"
        ));
    };
    let event = ["session-start", "user-prompt-submit"]
        .into_iter()
        .find(|event| !known.drops(event))
        .unwrap_or("user-prompt-submit");
    let session = format!("setup-check-{}", std::process::id());
    let input = json!({
        "cwd": repo,
        "session_id": session,
        "sessionId": session,
        "conversation_id": session,
        "conversationId": session,
        "workspace_roots": [repo],
        "workspacePaths": [repo],
    })
    .to_string();
    // The hook remembers what it told a session; this one is forgotten.
    let hooks_dir = glance::find_root(repo).map(|root| glance::state_dir(&root).join("hooks"));
    let had_dir = hooks_dir.as_ref().is_some_and(|dir| dir.exists());
    let outcome = run_hook(&by, dialect, event, repo, &input);
    if let Some(dir) = &hooks_dir {
        branchyard_support::cleanup_file(dir.join(format!("{session}.json")));
        let empty = std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none());
        if !had_dir && empty {
            branchyard_support::best_effort(
                "remove the hook check's directory",
                std::fs::remove_dir(dir),
            );
        }
    }
    match outcome {
        Err(why) => HookCheck::Silent(why),
        Ok((_, stderr)) if !stderr.trim().is_empty() => HookCheck::Silent(format!(
            "`by hook --harness {dialect} {event}` said: {}",
            stderr.lines().next().unwrap_or_default().trim()
        )),
        Ok((stdout, _)) if stdout.trim().is_empty() => HookCheck::Verified(format!(
            "answers {event} in the {dialect} dialect; nothing to report in this repository yet"
        )),
        Ok(_) => HookCheck::Verified(format!(
            "answers {event} in the {dialect} dialect, with news from this repository"
        )),
    }
}

/// `by hook --harness DIALECT EVENT` with `input` on its standard input:
/// what it printed and what it said on stderr.
fn run_hook(
    by: &Path,
    dialect: &str,
    event: &str,
    repo: &Path,
    input: &str,
) -> Result<(String, String), String> {
    let mut child = Process::new(by)
        .args(["hook", "--harness", dialect, event])
        .current_dir(repo)
        .env_remove(branchyard::ENV_BRANCH)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", by.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        branchyard_support::best_effort(
            "write the hook check's input",
            stdin.write_all(input.as_bytes()),
        );
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let read = |pipe: Option<&mut dyn std::io::Read>| {
                    let mut text = String::new();
                    if let Some(pipe) = pipe {
                        branchyard_support::best_effort(
                            "read the hook check's output",
                            pipe.read_to_string(&mut text),
                        );
                    }
                    text
                };
                let stdout = read(child.stdout.as_mut().map(|p| p as &mut dyn std::io::Read));
                let stderr = read(child.stderr.as_mut().map(|p| p as &mut dyn std::io::Read));
                return match status.success() {
                    true => Ok((stdout, stderr)),
                    false => Err(format!("`by hook` exited with {status}")),
                };
            }
            Ok(None) if start.elapsed() < HOOK_WAIT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                branchyard_support::best_effort("stop the hook check", child.kill());
                branchyard_support::best_effort("reap the hook check", child.wait());
                return Err(format!(
                    "`by hook` did not answer within {} s",
                    HOOK_WAIT.as_secs()
                ));
            }
        }
    }
}

// ----------------------------------------------------------- todo and next

/// What only the person can do, and the command to run next.
fn finish(report: &mut Report) {
    let facts = &report.facts;
    let mut todo = Vec::new();
    if !report.config.substrate && !report.config.server && facts.installed().next().is_none() {
        todo.push(Todo {
            kind: "install",
            harness: None,
            text: "No coding harness is on PATH. Install one; `by harnesses --all` lists them \
                   with their install commands."
                .into(),
            command: Some("by harnesses install claude-code".into()),
        });
    }
    if report.config.substrate {
        todo.push(Todo { kind: "note", harness: report.config.harness.clone(),
            text: "Remote actors use the configured image and private-home secrets. Run by up to check Control authentication, template readiness and credential sources; model access remains unverified.".into(),
            command: Some("by up".into()) });
    }
    if report.config.server {
        todo.push(Todo {
            kind: "note",
            harness: report.config.harness.clone(),
            text: "Tasks use the configured server's harnesses and credentials. Run by setup --up to authenticate and inspect its serving workers; model access remains unverified.".into(),
            command: Some("by setup --up".into()),
        });
    }
    let default_harness = default_harness(report);
    for h in facts
        .installed()
        .filter(|_| !report.config.substrate && !report.config.server)
    {
        let label = harness_label(&h.harness);
        let login = format!("by harnesses login {}", h.harness);
        let how = match catalog::harness(&h.harness).and_then(|e| e.login.clone()) {
            Some(own) => format!("Sign in with `{login}` (it runs `{own}`)."),
            None => match branchyard::inventory::key_variables(&h.harness).join(" or ") {
                vars if vars.is_empty() => format!("`{login}` says what to run."),
                vars => format!("`{login} --api-key` stores a key ({vars})."),
            },
        };
        let key_unused = h
            .login
            .as_ref()
            .is_some_and(|l| l.detail.contains(branchyard::inventory::AGY_KEY_UNUSED));
        let text = match h.login.as_ref().map(|l| l.state.as_str()) {
            // A key agy will not read: logged in for Branchyard's isolated
            // runs, but a headless run in your own home asks for a sign-in.
            Some("logged_in") if key_unused => format!(
                "{label}: GEMINI_API_KEY is set, but agy reads it only with modelProvider gemini \
                 in its settings, so a run in your own home asks for a Google sign-in and fails. \
                 `{login}` shows the change and writes it."
            ),
            Some("logged_in") => continue,
            Some("logged_out") if login_verified(h) => format!("{label} is logged out. {how}"),
            // Nothing to go on at all (no status command, file or variable
            // the catalog knows): unknown, in the listing's word.
            Some("unknown") => format!("{label}: login unknown ({}). {how}", checked(h)),
            // Never "logged out" for a guess: an environment can authenticate
            // a harness without any of the files Branchyard looks for.
            _ => format!(
                "{label}: login not verified (checked: {}). {how}",
                checked(h)
            ),
        };
        let for_default = default_harness.as_deref() == Some(h.harness.as_str());
        let item = Todo {
            kind: "login",
            harness: Some(h.harness.clone()),
            text,
            command: Some(login),
        };
        // The default harness's login comes first: it is what `by run` uses.
        match for_default {
            true => todo.insert(0, item),
            false => todo.push(item),
        }
    }
    // Scripts the first `by run` would be refused for: those setup was
    // about to write (dry run), or those in the file now and not trusted.
    if report.dry_run && report.config.scripts.is_some() {
        todo.push(Todo {
            kind: "workspace",
            harness: None,
            text: "The [workspace] scripts above run only once trusted, and the first `by run` \
                   is refused until then. With --yes, setup trusts the ones it writes."
                .into(),
            command: Some("by workspace trust".into()),
        });
    } else if !report.dry_run && report.config.state != ConfigState::Skipped {
        if let Ok(Some(resolved)) = crate::workspace_cmd::resolve(&report.root) {
            if matches!(
                resolved.trust,
                crate::workspace_cmd::TrustState::Untrusted
                    | crate::workspace_cmd::TrustState::Changed
            ) {
                todo.push(Todo {
                    kind: "workspace",
                    harness: None,
                    text: "The [workspace] scripts in branchyard.toml are not trusted, and the \
                           first `by run` is refused until they are. `by workspace show` lists \
                           them."
                        .into(),
                    command: Some("by workspace trust".into()),
                });
            }
        }
    }
    let mut seen: Vec<String> = Vec::new();
    let mut silent: Vec<(String, Vec<String>)> = Vec::new();
    for step in &report.hooks {
        match (&step.state, &step.check) {
            (State::Refused(why), _) => todo.push(Todo {
                kind: "hooks",
                harness: Some(step.id.clone()),
                text: format!("{}: hooks not installed: {why}", step.label),
                command: Some(format!("by init plugin {} --dry-run", step.id)),
            }),
            // One line for harnesses that are silent for the same reason.
            (_, Some(HookCheck::Silent(why))) => match silent.iter_mut().find(|(w, _)| w == why) {
                Some((_, labels)) => labels.push(step.label.clone()),
                None => silent.push((why.clone(), vec![step.label.clone()])),
            },
            _ => {}
        }
        if matches!(step.state, State::Refused(_)) {
            continue;
        }
        for note in &step.notes {
            if !seen.contains(note) {
                seen.push(note.clone());
                todo.push(Todo {
                    kind: "note",
                    harness: Some(step.id.clone()),
                    text: format!("{}: {note}", step.label),
                    command: None,
                });
            }
        }
    }
    for (why, labels) in silent {
        todo.push(Todo {
            kind: "hooks",
            harness: None,
            text: format!("The hooks of {} are silent: {why}", labels.join(", ")),
            command: None,
        });
    }
    report.todo = todo;
    report.next = next_command(report);
}

/// The default harness the file names, or would: else the one the engine
/// would suggest.
fn default_harness(report: &Report) -> Option<String> {
    report
        .config
        .harness
        .clone()
        .or_else(|| match report.config.state {
            ConfigState::Skipped => None,
            _ => Some(report.facts.preferred_harness()),
        })
        .filter(|h| report.facts.installed().any(|f| &f.harness == h))
}

/// `by run "fix the flaky test" --check "cargo test"`, with the check the
/// file names or the files suggest.
fn run_command(check: Option<&str>, harness: Option<&str>) -> String {
    let mut command = format!("by run \"{EXAMPLE_TASK}\"");
    if let Some(check) = check {
        let quoted = match check.chars().any(|c| matches!(c, '"' | '\\' | '$' | '`')) {
            true => branchyard_recipe::quote(check),
            false => format!("\"{check}\""),
        };
        command.push_str(&format!(" --check {quoted}"));
    }
    if harness.is_some_and(unapproved_tools) {
        command.push_str(" --allow-unapproved-tools");
    }
    command
}

fn next_command(report: &Report) -> String {
    let writes_pending = report.dry_run
        && (report.config.state == ConfigState::Pending
            || report.hooks.iter().any(|h| h.state == State::Pending));
    if writes_pending {
        return match report.interactive {
            true => "by setup".into(),
            false => "by setup --yes".into(),
        };
    }
    if report.config.server {
        return "by setup --up".into();
    } else if report.config.substrate {
        return "by up".into();
    }
    if report.facts.installed().next().is_none() {
        return "by harnesses install claude-code".into();
    }
    let check = report
        .config
        .check
        .clone()
        .or_else(|| report.facts.suggested_check.clone());
    run_command(check.as_deref(), default_harness(report).as_deref())
}

// ---------------------------------------------------------------- rendering

/// `label` padded, then `text`, continuation lines indented under it.
fn row(out: &mut String, label: &str, text: &str) {
    let mut lines = text.lines();
    out.push_str(&format!(
        "  {label:<LABEL$}  {}\n",
        lines.next().unwrap_or_default()
    ));
    for line in lines {
        out.push_str(&format!("  {:<LABEL$}  {line}\n", ""));
    }
}

/// `text` wrapped at [`WRAP`] columns, the first line after `first`, the
/// rest indented by `indent`.
fn wrapped(first: &str, indent: &str, text: &str) -> String {
    let mut out = String::new();
    let mut line = first.to_owned();
    let mut fresh = true;
    for word in text.split_whitespace() {
        if !fresh && line.chars().count() + 1 + word.chars().count() > WRAP {
            out.push_str(&line);
            out.push('\n');
            line = indent.to_owned();
            fresh = true;
        }
        if !fresh {
            line.push(' ');
        }
        line.push_str(word);
        fresh = false;
    }
    out.push_str(&line);
    out.push('\n');
    out
}

/// A path with the home directory written `~`.
fn tilde(path: &str, home: Option<&str>) -> String {
    match home {
        Some(home) if !home.is_empty() => match path.strip_prefix(home.trim_end_matches('/')) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_owned(),
        },
        _ => path.to_owned(),
    }
}

fn plural(n: usize, word: &str) -> String {
    match n {
        1 => format!("1 {word}"),
        n => format!("{n} {word}s"),
    }
}

/// The config and hooks steps. With `files`, each file the install touches.
fn changes_text(report: &Report, files: bool) -> String {
    let home = report.facts.home.as_deref();
    let verb = |state: &State| match (state, report.dry_run) {
        (State::Done, _) => "installed",
        (State::Current, _) => "already current",
        (State::Pending, true) => "would install",
        (State::Pending, false) => "will install",
        (State::Refused(_), _) => "not installed",
    };
    let mut out = String::new();
    let c = &report.config;
    match c.state {
        ConfigState::Skipped => row(&mut out, "config", "skipped (--no-config)"),
        ConfigState::Exists => row(
            &mut out,
            "config",
            &format!("{} exists; left as it is", c.path),
        ),
        ConfigState::Pending | ConfigState::Done => {
            let tense = match (&c.state, report.dry_run) {
                (ConfigState::Done, _) => "wrote",
                (_, true) => "would write",
                (_, false) => "will write",
            };
            row(
                &mut out,
                "config",
                &format!("{tense} {}: {}", c.path, c.summary),
            );
        }
    }
    if let Some(scripts) = &c.scripts {
        let heading = match (c.trusted, &c.state) {
            (true, _) => {
                "[workspace] scripts, trusted by this run (`by workspace untrust` takes it back):"
            }
            (false, ConfigState::Done) => "[workspace] scripts, not trusted yet:",
            _ => "[workspace] scripts, which run as you in each new worktree once trusted:",
        };
        let mut text = format!("{heading}\n");
        for line in crate::workspace_cmd::scripts(scripts).lines() {
            text.push_str(&format!("{line}\n"));
        }
        row(&mut out, "", text.trim_end());
    }
    if report.hooks.is_empty() {
        row(
            &mut out,
            "hooks",
            "no harness on PATH has an install, or --no-hooks",
        );
    }
    for (i, step) in report.hooks.iter().enumerate() {
        let label = match i {
            0 => "hooks",
            _ => "",
        };
        let state = match &step.state {
            State::Pending | State::Done => {
                format!("{} {}", verb(&step.state), plural(step.writes, "file"))
            }
            State::Refused(_) => "not installed (see below)".to_owned(),
            other => verb(other).to_owned(),
        };
        let provides = step.provides.join(", ");
        let check = match &step.check {
            Some(HookCheck::Verified(_)) => "  hooks: verified".to_owned(),
            Some(HookCheck::Silent(_)) => "  hooks: silent (see below)".to_owned(),
            None => String::new(),
        };
        row(
            &mut out,
            label,
            &format!("{:<12} {state} ({provides}){check}", step.label),
        );
        if files || matches!(step.state, State::Refused(_)) {
            for t in &step.targets {
                let dir = match t.files {
                    1 => String::new(),
                    n => format!("  ({n} files)"),
                };
                let how = match t.how.as_str() {
                    "" | "write" => String::new(),
                    how => format!("  {how}"),
                };
                row(
                    &mut out,
                    "",
                    &format!("  {} {}{dir}{how}", t.action, tilde(&t.path, home)),
                );
            }
        }
    }
    out
}

/// The summary card: the steps, the default harness's login, what only the
/// person can do, and the next command.
fn card_text(report: &Report) -> String {
    let mut out = String::from("Summary\n");
    out.push_str(&changes_text(report, report.dry_run));
    if let Some(harness) = default_harness(report) {
        if let Some(h) = report.facts.installed().find(|h| h.harness == harness) {
            let verdict = match (
                h.login.as_ref().map(|l| l.state.as_str()),
                login_verified(h),
            ) {
                (Some("logged_in"), _) | (Some("logged_out"), true) => login_words(h),
                _ => format!("not verified (checked: {})", checked(h)),
            };
            let mut line = String::new();
            row(
                &mut line,
                "login",
                &format!("{harness}: {verdict}. Auth source: {}.", auth_source(h)),
            );
            out.push_str(&line);
        }
    }
    let nothing = report.written.is_empty() && !report.dry_run;
    if nothing {
        let existed = report.config.state == ConfigState::Exists
            || report.hooks.iter().any(|h| h.state == State::Current);
        out.push_str(match existed {
            true => "\nNothing to write: already set up.\n",
            false => "\nNothing to write.\n",
        });
    }
    if report.dry_run {
        out.push_str(&match report.asked_dry {
            true => "\nNothing was written (--dry-run).\n".to_owned(),
            false => "\nNothing was written: no terminal to ask on. Rerun with --yes to write \
                      the above.\n"
                .to_owned(),
        });
    }
    if !report.todo.is_empty() {
        out.push_str("\nOnly you can do:\n");
        let guessed = report.todo.iter().any(|t| {
            t.kind == "login"
                && report
                    .facts
                    .installed()
                    .any(|h| Some(&h.harness) == t.harness.as_ref() && !login_verified(h))
        });
        if guessed {
            out.push_str(&wrapped(
                "  ",
                "  ",
                "A login that is not verified is a guess: Branchyard looks for login files and \
                 key variables only, and your environment can authenticate a harness without \
                 them. Then `by run` uses it, and spends your money.",
            ));
        }
        // After a run that wrote nothing the hook notes are not news; they
        // stay one command away.
        let quiet = nothing;
        let mut skipped = 0;
        for item in &report.todo {
            if quiet && item.kind == "note" {
                skipped += 1;
                continue;
            }
            out.push_str(&wrapped("  - ", "    ", &item.text));
            if let (Some(command), "workspace" | "hooks") = (&item.command, item.kind) {
                out.push_str(&format!("    Run: {command}\n"));
            }
        }
        if skipped > 0 {
            out.push_str(&format!(
                "  - {} to finish in your harnesses: by init plugin --status lists them.\n",
                plural(skipped, "one-time step")
            ));
        }
    }
    out.push_str(&format!("\nNext: {}\n", report.next));
    out
}

// --------------------------------------------------------------------- json

/// The one object `--json` prints.
fn report_json(report: &Report) -> Value {
    let facts = &report.facts;
    let harnesses: Vec<Value> = facts
        .installed()
        .map(|h| {
            json!({
                "harness": h.harness,
                "label": harness_label(&h.harness),
                "version": h.version,
                "login": h.login.as_ref().map(|l| l.state.clone()),
                "evidence": h.login.as_ref().map(|l| l.evidence.clone()),
                "auth_source": auth_source(h),
            })
        })
        .collect();
    let default = default_harness(report).and_then(|id| {
        facts.installed().find(|h| h.harness == id).map(|h| {
            json!({
                "harness": h.harness,
                "login": h.login.as_ref().map(|l| l.state.clone()),
                "evidence": h.login.as_ref().map(|l| l.evidence.clone()),
                "verified": login_verified(h),
                "auth_source": auth_source(h),
            })
        })
    });
    let installed: Vec<Value> = report
        .hooks
        .iter()
        .map(|step| {
            let (state, why) = match &step.state {
                State::Current => ("current", None),
                State::Pending => ("would_install", None),
                State::Done => ("installed", None),
                State::Refused(why) => ("refused", Some(why.clone())),
            };
            let (hooks, detail) = match &step.check {
                Some(HookCheck::Verified(d)) => (Some("verified"), Some(d.clone())),
                Some(HookCheck::Silent(d)) => (Some("silent"), Some(d.clone())),
                None => (None, None),
            };
            json!({
                "harness": step.id,
                "label": step.label,
                "provides": step.provides,
                "state": state,
                "reason": why,
                "files": step.writes,
                "hooks": hooks,
                "hooks_detail": detail,
            })
        })
        .collect();
    let todo: Vec<Value> = report
        .todo
        .iter()
        .map(|t| {
            json!({
                "kind": t.kind,
                "harness": t.harness,
                "text": t.text,
                "command": t.command,
            })
        })
        .collect();
    let c = &report.config;
    let state = match c.state {
        ConfigState::Skipped => "skipped",
        ConfigState::Exists => "exists",
        ConfigState::Pending => "would_write",
        ConfigState::Done => "written",
    };
    let planned: Vec<String> = c
        .plan
        .iter()
        .chain(report.hooks.iter().filter_map(|h| h.plan.as_ref()))
        .flat_map(|p| &p.files)
        .filter(|f| f.writes())
        .map(|f| f.path.clone())
        .collect();
    let mut object = json!({
        "dry_run": report.dry_run,
        "detected": {
            "repository": {
                "root": facts.root,
                "git": facts.git.is_some(),
                "default_branch": facts.git.as_ref().and_then(|g| g.default_branch.clone()),
            },
            "harnesses": harnesses,
            "default_harness": default,
            "check": facts.suggested_check,
        },
        "config": {
            "path": c.path,
            "state": state,
            "summary": c.summary,
            "workspace_trusted": c.trusted,
        },
        "written": report.written,
        "unchanged": report.unchanged,
        "installed": installed,
        "todo": todo,
        "next": report.next,
    });
    if report.dry_run {
        object["would_write"] = json!(planned);
    }
    if let Some(plan) = &report.readiness {
        object["readiness"] = plan.json();
        object["ready"] = Value::Bool(plan.ready());
    }
    object
}

// ------------------------------------------------------------------- status

/// What `by init plugin --status` knows of one harness's install.
struct Installed {
    manifest: Manifest,
    detected: bool,
    /// `current`, `stale`, `missing` or `conflict`.
    state: &'static str,
    files: usize,
}

/// `by init plugin --status [ID...] [--json]`: per harness, whether its
/// install is current, stale or missing, whether `by` is on `PATH` for its
/// hooks, and the one-time steps still on the person's list. Writes nothing.
pub fn plugin_status(env: &Env, only: &[String], as_json: bool) -> Outcome {
    let cwd = std::env::current_dir()?;
    let probe = HostProbe::without_versions(&cwd);
    let facts = {
        use branchyard_setup::Probe as _;
        Facts {
            home: probe.home(),
            install_detected: install::detect(&probe),
            ..Facts::default()
        }
    };
    let manifests = install::manifests().map_err(Failure::Message)?;
    if let Some(unknown) = only
        .iter()
        .find(|id| !manifests.iter().any(|m| &m.harness == *id))
    {
        let known: Vec<&str> = manifests.iter().map(|m| m.harness.as_str()).collect();
        return Err(Failure::Message(format!(
            "no install for harness {unknown:?}; by init plugin installs for: {}",
            known.join(", ")
        )));
    }
    let all: Vec<Installed> = manifests
        .into_iter()
        .map(|manifest| {
            let files = install::planned_files(std::slice::from_ref(&manifest), &facts, &probe);
            let state = if files.iter().any(|f| f.refusal.is_some()) {
                "conflict"
            } else if files.iter().all(|f| f.action == FileAction::Unchanged) {
                "current"
            } else if files.iter().all(|f| f.action == FileAction::Create) {
                "missing"
            } else {
                "stale"
            };
            Installed {
                detected: facts.install_detected.contains(&manifest.harness),
                state,
                files: files.len(),
                manifest,
            }
        })
        .collect();
    let shown: Vec<&Installed> = all
        .iter()
        .filter(|i| match only.is_empty() {
            true => i.detected || i.state != "missing",
            false => only.contains(&i.manifest.harness),
        })
        .collect();
    let hidden = all.len() - shown.len();
    let by = which("by");
    // Only what is installed has steps left to do.
    let notes = |i: &Installed| match i.state {
        "current" | "stale" => i.manifest.notes.iter().map(|n| n.text.clone()).collect(),
        _ => Vec::new(),
    };
    if as_json {
        let rows: Vec<Value> = shown
            .iter()
            .map(|i| {
                json!({
                    "harness": i.manifest.harness,
                    "label": harness_label(&i.manifest.harness),
                    "detected": i.detected,
                    "state": i.state,
                    "files": i.files,
                    "provides": i.manifest.provides,
                    "notes": notes(i),
                })
            })
            .collect();
        return print(&format!(
            "{}\n",
            crate::json::text(&json!({
                "by_on_path": by.is_some(),
                "by": by.as_ref().map(|p| p.display().to_string()),
                "harnesses": rows,
                "hidden": hidden,
            }))
        ));
    }
    let mut out = String::new();
    match &by {
        Some(path) => out.push_str(&format!("by on PATH: {}\n", path.display())),
        None => out.push_str(
            "by is not on PATH: every hook starts with `command -v by`, so none of them speaks \
             until it is.\n",
        ),
    }
    out.push('\n');
    let columns = [
        Column {
            header: "HARNESS",
            max: 20,
            right: false,
        },
        Column {
            header: "ON PATH",
            max: 8,
            right: false,
        },
        Column {
            header: "INSTALL",
            max: 10,
            right: false,
        },
        Column {
            header: "FILES",
            max: 6,
            right: true,
        },
        Column {
            header: "NEXT",
            max: 60,
            right: false,
        },
    ];
    let rows: Vec<Vec<Cell>> = shown
        .iter()
        .map(|i| {
            let (tone, next) = match i.state {
                "current" => (Tone::Green, String::new()),
                "missing" => (
                    Tone::Yellow,
                    format!("by init plugin {}", i.manifest.harness),
                ),
                "stale" => (
                    Tone::Yellow,
                    format!("by init plugin {} --dry-run", i.manifest.harness),
                ),
                _ => (
                    Tone::Red,
                    format!(
                        "by init plugin {} --dry-run (shows what blocks it)",
                        i.manifest.harness
                    ),
                ),
            };
            vec![
                Cell::plain(&i.manifest.harness),
                Cell::plain(match i.detected {
                    true => "yes",
                    false => "no",
                }),
                Cell::toned(i.state, tone),
                Cell::plain(i.files.to_string()),
                Cell::plain(next),
            ]
        })
        .collect();
    out.push_str(&table(&columns, &rows, env.style()));
    let mut steps = Vec::new();
    for i in &shown {
        for note in notes(i) {
            if !steps.iter().any(|(_, n): &(String, String)| *n == note) {
                steps.push((harness_label(&i.manifest.harness), note));
            }
        }
    }
    if !steps.is_empty() {
        out.push_str(
            "\nStill on your list (Branchyard cannot do these, or see whether you did):\n",
        );
        for (label, note) in steps {
            out.push_str(&wrapped("  - ", "    ", &format!("{label}: {note}")));
        }
    }
    if hidden > 0 {
        out.push_str(&format!(
            "\n{hidden} more harnesses are not on this machine; by init plugin lists them.\n"
        ));
    }
    out.push_str("\nNext: by setup (or by init plugin --all-detected) installs what is missing\n");
    print(&out)
}

// --------------------------------------------------------------------- card

/// Bare `by`: whether this repository is set up, its branches by state, and
/// the one next command. Under twelve lines; `by --help` lists every command.
pub fn card() -> Outcome {
    let cwd = std::env::current_dir()?;
    let root = setup_io::project_root(&cwd);
    if !root.join(".git").exists() {
        return print(&format!(
            "Branchyard: {} is not inside a git repository.\n\
             Next: cd <repo> && by setup\n\
             All commands: by --help\n",
            cwd.display()
        ));
    }
    let configured = root.join(config::PROJECT_FILE).is_file();
    let yard = root.join(".branchyard").is_dir();
    let yes = |b: bool| if b { "yes" } else { "no" };
    let mut out = String::new();
    row(&mut out, "repository", &root.display().to_string());
    row(
        &mut out,
        "set up",
        &format!(
            "{}: branchyard.toml {} · .branchyard/ {}",
            if configured { "yes" } else { "not yet" },
            yes(configured),
            yes(yard)
        ),
    );
    let env = |name: &str| std::env::var(name).ok();
    let server = env("BRANCHYARD_REMOTE")
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            crate::defaults::config_at(&cwd, &env)
                .ok()
                .flatten()
                .and_then(|c| c.remote.url)
        });
    let next = match (&server, glance::read(&root, None)) {
        (Some(url), _) => {
            row(&mut out, "server", &format!("{url} (its branches: by ls)"));
            "by ls".to_owned()
        }
        (None, Ok(Some(look))) => {
            let counts = look.counts();
            let parts: Vec<String> = [
                (counts.running, "running"),
                (counts.waiting, "waiting"),
                (counts.asking, "asking"),
                (counts.blocked, "blocked"),
                (counts.ready, "ready"),
                (counts.failed, "failed"),
            ]
            .into_iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, word)| format!("{n} {word}"))
            .collect();
            let total = look.branches.len();
            let shown = match (total, parts.is_empty()) {
                (0, _) => "none yet".to_owned(),
                (n, true) => format!("{} (none needs you)", plural(n, "branch")),
                (n, false) => format!("{} · {}", plural(n, "branch"), parts.join(" · ")),
            };
            row(&mut out, "branches", &shown);
            // The first branch that wants a person, most urgent first.
            let wants = look
                .asking
                .first()
                .map(|m| m.from.clone())
                .or_else(|| {
                    first_with(&look, |s| {
                        matches!(
                            s,
                            branchyard::BranchStatus::Blocked { .. }
                                | branchyard::BranchStatus::AwaitingPlanApproval
                        )
                    })
                })
                .or_else(|| first_with(&look, |s| matches!(s, branchyard::BranchStatus::Ready)))
                .or_else(|| {
                    first_with(&look, |s| {
                        matches!(
                            s,
                            branchyard::BranchStatus::Failed { .. }
                                | branchyard::BranchStatus::BudgetExceeded { .. }
                                | branchyard::BranchStatus::Interrupted
                        )
                    })
                });
            match (total, wants) {
                (0, _) if !configured => "by setup".to_owned(),
                (0, _) => format!("by run \"{EXAMPLE_TASK}\""),
                (_, Some(name)) => format!("by show {name}"),
                // Branches do not make a repository set up: without a
                // branchyard.toml the next step is still the setup.
                (_, None) if !configured => "by setup".to_owned(),
                (_, None) => "by ls".to_owned(),
            }
        }
        (None, Ok(None)) => {
            row(&mut out, "branches", "none yet");
            match configured {
                true => format!("by run \"{EXAMPLE_TASK}\""),
                false => "by setup".to_owned(),
            }
        }
        (None, Err(error)) => {
            row(&mut out, "branches", &format!("unreadable: {error}"));
            "by doctor".to_owned()
        }
    };
    out.push_str(&format!("\nNext: {next}\nAll commands: by --help\n"));
    print(&format!("Branchyard\n{out}"))
}

/// The first branch whose status satisfies `wanted`.
fn first_with(
    look: &glance::Glance,
    wanted: impl Fn(&branchyard::BranchStatus) -> bool,
) -> Option<String> {
    look.branches
        .iter()
        .find(|b| wanted(&b.status))
        .map(|b| b.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard_setup::probe::HarnessLogin;

    fn fact(state: &str, evidence: &str, detail: &str) -> HarnessFact {
        HarnessFact {
            harness: "claude-code".into(),
            profile: "claude-code".into(),
            installed: true,
            version: Some("2.1.0".into()),
            qualification: None,
            login: Some(HarnessLogin {
                state: state.into(),
                evidence: evidence.into(),
                detail: detail.into(),
                label: String::new(),
            }),
        }
    }

    #[test]
    fn a_guess_is_never_called_logged_out() {
        let guess = fact("logged_out", "likely", "no ~/.claude/.credentials.json");
        assert_eq!(login_words(&guess), "login not verified");
        assert!(!login_verified(&guess));
        assert_eq!(auth_source(&guess), "unknown");
        assert_eq!(checked(&guess), "no ~/.claude/.credentials.json");
        let said = fact(
            "logged_out",
            "verified",
            "codex login status: Not logged in",
        );
        assert_eq!(login_words(&said), "logged out");
        let known = fact("logged_in", "verified", "~/.claude/.credentials.json");
        assert_eq!(login_words(&known), "logged in");
        assert_eq!(auth_source(&known), "~/.claude/.credentials.json");
        let unknown = fact("unknown", "none", "");
        assert_eq!(login_words(&unknown), "unknown");
        assert_eq!(checked(&unknown), "no check ran");
    }

    #[test]
    fn long_text_wraps_under_its_marker() {
        let text = "word ".repeat(40);
        let out = wrapped("  - ", "    ", &text);
        assert!(out.lines().count() > 1);
        assert!(out.lines().all(|l| l.chars().count() <= WRAP), "{out}");
        assert!(out.lines().next().unwrap().starts_with("  - word"));
        assert!(out.lines().nth(1).unwrap().starts_with("    word"));
    }

    #[test]
    fn the_home_directory_is_a_tilde() {
        assert_eq!(
            tilde("/home/me/.codex/hooks.json", Some("/home/me")),
            "~/.codex/hooks.json"
        );
        assert_eq!(tilde("/home/meme/x", Some("/home/me")), "/home/meme/x");
        assert_eq!(
            tilde("branchyard.toml", Some("/home/me")),
            "branchyard.toml"
        );
    }

    #[test]
    fn the_next_run_names_the_check_safely() {
        assert_eq!(
            run_command(Some("cargo test"), None),
            "by run \"fix the flaky test\" --check \"cargo test\""
        );
        assert_eq!(run_command(None, None), "by run \"fix the flaky test\"");
        assert_eq!(
            run_command(None, Some("antigravity")),
            "by run \"fix the flaky test\" --allow-unapproved-tools"
        );
        // A check with quotes or a `$` survives a shell reading the line.
        let line = run_command(Some("sh -c 'x $Y'"), None);
        assert_eq!(
            config::split_words(&line).unwrap(),
            ["by", "run", "fix the flaky test", "--check", "sh -c 'x $Y'"]
        );
    }

    #[test]
    fn pi_and_its_kin_speak_the_generic_dialect() {
        for id in ["pi", "oh-my-pi", "opencode", "amp"] {
            assert_eq!(dialect_of(id), "generic");
        }
        assert_eq!(dialect_of("codex"), "codex");
        // Every harness that has an install has a dialect to be checked in.
        for manifest in install::manifests().unwrap() {
            assert!(
                crate::hook_cmd::dialect(Some(dialect_of(&manifest.harness))).is_ok(),
                "{}",
                manifest.harness
            );
        }
    }

    #[test]
    fn remote_readiness_requires_the_serving_workers_harness_and_credentials() {
        use branchyard::inventory::{HarnessState, Inventory, Login, LoginState};
        use branchyard_client::api::{InventoryReport, WorkerInventory};

        let mut report = InventoryReport {
            workers: vec![WorkerInventory {
                id: "worker".into(),
                host: "host".into(),
                labels: Vec::new(),
                repos: vec!["project".into()],
                seen_ms_ago: 0,
                this: true,
                inventory: Some(Inventory {
                    harnesses: vec![HarnessState {
                        id: "antigravity".into(),
                        path: "/usr/local/bin/agy".into(),
                        on_path: true,
                        login: Login {
                            state: LoginState::LoggedIn,
                            ..Login::default()
                        },
                        ..HarnessState::default()
                    }],
                    ..Inventory::default()
                }),
            }],
        };
        let plan = remote_plan(&report, "project", "antigravity");
        assert!(plan.ready());
        assert!(plan
            .steps
            .iter()
            .any(|step| step.detail.contains("model authentication unverified")));
        assert!(!remote_plan(&report, "another-project", "antigravity").ready());
        report.workers[0].inventory.as_mut().unwrap().harnesses[0]
            .login
            .state = LoginState::Unknown;
        let plan = remote_plan(&report, "project", "antigravity");
        assert!(!plan.ready());
        assert_eq!(
            plan.steps.last().unwrap().state,
            branchyard::up::State::NeedsPerson
        );
        assert!(!remote_plan(
            &InventoryReport {
                workers: Vec::new()
            },
            "project",
            "antigravity"
        )
        .ready());
    }
}
