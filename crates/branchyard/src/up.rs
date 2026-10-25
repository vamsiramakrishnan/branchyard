//! Bringing a repository's configured backends up: [`Yard::up_plan`],
//! [`Yard::up_apply`] and [`Yard::up`], the one operation behind `by up`
//! (`docs/up.md`). From the configuration ([`crate::config`]) it works out
//! what the repository needs and what state each piece is in:
//!
//! - the store, `.branchyard/`, which opening the yard made;
//! - each harness the configuration names (`[defaults] harness`, the
//!   `[fleet]` candidates and judges, the `[knowledge]` distiller, or the
//!   default harness when none is named): installed, and logged in;
//! - each secret `[secrets]` and the `[models]` backends name: its
//!   variable set or its file present, by name, never by value;
//! - the model gateway `[models]` describes;
//! - the connector gateway `[connectors]` describes, or the bundles under
//!   the repository need: listening, or to be started;
//! - the provider `[defaults] provider` names: a build that runs it, a
//!   recipe that may run;
//! - the `[workspace]` scripts: trusted.
//!
//! Each is a [`Step`] in one of four states. `ready` needs nothing.
//! `planned` is something `up` does itself: install a harness, start the
//! gateway. `needs_person` is a decision or a secret only a person
//! supplies: a login, a key, a trust decision, or an install the
//! `[harnesses]` policy makes them confirm. `blocked` is something the
//! configuration or this machine cannot do, with what to change.
//!
//! What `up` reads and does on the machine goes through [`Machine`], so
//! the plan is computed the same way everywhere and tested with a fake;
//! [`LocalMachine`] is this machine. Installing follows the `[harnesses]`
//! policy as `by harnesses install` does: only your user file can allow
//! it, and a harness running on a branch never calls this.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use branchyard_controls::catalog;
use branchyard_support::best_effort;
use branchyard_support::time::now_ms;
use serde::{Deserialize, Serialize};

use crate::config::{self, Connectors, HarnessInstall, ProjectConfig, ProviderKind};
use crate::configure;
use crate::connectors::{gateway, packager};
use crate::inventory::{
    self, DetectOptions, HarnessEvent, HarnessLog, HarnessState, InstallAction, InstallMode,
    InstallPlan, InstallPolicy, Inventory, InventoryCache, LoginState, Permission, RunResult,
};
use crate::models::KeySource;
use crate::{doctor, harness, projection, Error, Yard};

/// The harness a task runs on when the configuration names none, as
/// [`crate::TaskOptions::harness`] documents.
pub const DEFAULT_HARNESS: &str = "claude-code";

/// How far along one piece is. Ordered by how much it needs, so the worst
/// of a plan is its maximum.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Nothing to do.
    Ready,
    /// `up` does this itself when applied.
    Planned,
    /// A person's decision or secret: a login, a key, a trust decision,
    /// or an install to confirm.
    NeedsPerson,
    /// This configuration or machine cannot do it; the step says what to
    /// change.
    Blocked,
}

impl State {
    /// `ready`, `planned`, `needs_person` or `blocked`.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Ready => "ready",
            State::Planned => "planned",
            State::NeedsPerson => "needs_person",
            State::Blocked => "blocked",
        }
    }
}

/// What `up` does for a step when it is applied.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Run this install command, from the catalog, then detect the harness
    /// again.
    Install {
        /// The command and what it pins.
        plan: InstallPlan,
    },
    /// Start the repository's connector gateway in the background.
    StartGateway,
}

/// One piece of the repository's setup and its state.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// A stable dotted name, such as `harness.codex`, `secret.OPENAI_API_KEY`
    /// or `connectors`.
    pub name: String,
    /// How far along it is.
    pub state: State,
    /// What was found, in one line.
    pub detail: String,
    /// For a step that is not ready: what to do, in one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo: Option<String>,
    /// What applying the plan does for a planned step, or may do for one
    /// that needs the person's yes ([`Plan::approve`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

/// `text` as one line: runs of whitespace, newlines included, become one
/// space.
fn one_line(text: impl Into<String>) -> String {
    text.into().split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Step {
    /// A step that needs nothing.
    pub fn ready(name: impl Into<String>, detail: impl Into<String>) -> Step {
        Step {
            name: name.into(),
            state: State::Ready,
            detail: one_line(detail),
            todo: None,
            action: None,
        }
    }

    /// A step `up` does itself.
    pub fn planned(name: impl Into<String>, detail: impl Into<String>, action: Action) -> Step {
        Step {
            name: name.into(),
            state: State::Planned,
            detail: one_line(detail),
            todo: None,
            action: Some(action),
        }
    }

    /// A step for a person, with what to do; `action` when their yes lets
    /// `up` do it.
    pub fn needs_person(
        name: impl Into<String>,
        detail: impl Into<String>,
        todo: impl Into<String>,
        action: Option<Action>,
    ) -> Step {
        Step {
            name: name.into(),
            state: State::NeedsPerson,
            detail: one_line(detail),
            todo: Some(one_line(todo)),
            action,
        }
    }

    /// A step this configuration or machine cannot do, with what to
    /// change.
    pub fn blocked(
        name: impl Into<String>,
        detail: impl Into<String>,
        todo: impl Into<String>,
    ) -> Step {
        Step {
            name: name.into(),
            state: State::Blocked,
            detail: one_line(detail),
            todo: Some(one_line(todo)),
            action: None,
        }
    }
}

/// The steps, in report order; what [`Yard::up_plan`] computes and
/// [`Yard::up_apply`] returns, with the applied steps in their new state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// The steps, in report order.
    pub steps: Vec<Step>,
}

impl Plan {
    /// Every step is ready.
    pub fn ready(&self) -> bool {
        self.steps.iter().all(|s| s.state == State::Ready)
    }

    /// The worst state of any step; `ready` for none.
    pub fn state(&self) -> State {
        self.steps
            .iter()
            .map(|s| s.state)
            .max()
            .unwrap_or(State::Ready)
    }

    /// The steps `up` does when the plan is applied.
    pub fn planned(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|s| s.state == State::Planned)
    }

    /// The steps that need a person's yes before `up` does them: not
    /// ready, with an action.
    pub fn askable(&self) -> impl Iterator<Item = &Step> {
        self.steps
            .iter()
            .filter(|s| s.state == State::NeedsPerson && s.action.is_some())
    }

    /// The person said yes to the step `name`: it is planned now. `false`
    /// when there is no such step to approve.
    pub fn approve(&mut self, name: &str) -> bool {
        match self
            .steps
            .iter_mut()
            .find(|s| s.name == name && s.state == State::NeedsPerson && s.action.is_some())
        {
            Some(step) => {
                step.state = State::Planned;
                step.todo = None;
                true
            }
            None => false,
        }
    }

    /// The plan as `by up --json` prints it: `{"steps": [...], "state",
    /// "ready"}`.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "steps": self.steps,
            "state": self.state(),
            "ready": self.ready(),
        })
    }
}

/// How `up` may act.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Answer yes to the installs the `[harnesses]` policy would ask
    /// about (`by up --yes`).
    pub yes: bool,
    /// Whether a person can be asked: a terminal. Without one, an install
    /// the policy would ask about needs `yes`.
    pub interactive: bool,
}

/// Whether something a person must trust may run here, as the machine
/// answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// Defined, and trusted as it is now (or needing no trust).
    Ready,
    /// Not defined.
    Undefined,
    /// Defined, and not trusted as it is now.
    NeedsTrust,
    /// This process cannot read the trust decision; `by` can.
    Unknown,
}

/// What `up` reads and does on a machine. [`LocalMachine`] is this one;
/// a test uses a fake. Nothing here returns a secret's value.
pub trait Machine {
    /// The harnesses and tools found, fresh enough.
    fn inventory(&mut self) -> Result<Inventory, String>;
    /// Detect one harness again, after an install.
    fn detect(&mut self, harness: &str) -> Result<Inventory, String>;
    /// Whether the variable `name` is set and not empty.
    fn var_set(&self, name: &str) -> bool;
    /// Whether `path` is a file.
    fn file_present(&self, path: &Path) -> bool;
    /// The executable `program` on `PATH`, if any.
    fn which(&self, program: &str) -> Option<PathBuf>;
    /// Whether something accepts connections at `url`.
    fn listening(&self, url: &str) -> bool;
    /// Run an install command: its exit status and output.
    fn run(&mut self, command: &str) -> RunResult;
    /// Start the yard's connector gateway in the background (as `by
    /// gateway start` does); the URL it listens at.
    fn start_gateway(&mut self, yard: &Yard, config: &Connectors) -> Result<String, String>;
    /// Whether recipe `name` may run here.
    fn recipe(&self, yard: &Yard, name: &str) -> Readiness;
    /// Whether the `[workspace]` scripts may run here.
    fn workspace(&self, yard: &Yard) -> Readiness;
    /// Record an install in the harness log, when the machine keeps one.
    fn record(&mut self, event: &HarnessEvent) {
        let _ = event;
    }
}

/// This machine: the harnesses detected afresh (and kept for `by
/// harnesses`), this process's environment and `PATH`, and `by gateway
/// start` for the gateway. Trust
/// decisions (`by recipe trust`, `by workspace trust`) are `by`'s to read;
/// here they are [`Readiness::Unknown`], which the plan reports as the
/// person's to check.
pub struct LocalMachine {
    options: DetectOptions,
    cache: InventoryCache,
    by: Option<PathBuf>,
    log: HarnessLog,
}

impl LocalMachine {
    /// With the inventory cache and harness log under the user
    /// configuration directory, and the `by` this process is or finds
    /// beside it or on `PATH`.
    pub fn new() -> LocalMachine {
        let dir = config::user_file()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let (inventory_path, log_path) = inventory::user_paths(&dir);
        LocalMachine {
            options: DetectOptions::from_env(),
            cache: InventoryCache::new(inventory_path, inventory::DEFAULT_TTL),
            by: projection::found_by(),
            log: HarnessLog::new(log_path),
        }
    }

    /// Start the gateway with this `by`.
    pub fn with_by(mut self, by: PathBuf) -> LocalMachine {
        self.by = Some(by);
        self
    }
}

impl Default for LocalMachine {
    fn default() -> Self {
        LocalMachine::new()
    }
}

impl Machine for LocalMachine {
    fn inventory(&mut self) -> Result<Inventory, String> {
        // Looked at again, not read from the cache: `up` is about to act on
        // what it finds, and a login made since the last look counts.
        self.cache.local(&self.options, true)
    }

    fn detect(&mut self, _harness: &str) -> Result<Inventory, String> {
        // Everything again, so that the cache `by harnesses` and the next
        // `by up` read has the install in it.
        self.cache.local(&self.options, true)
    }

    fn var_set(&self, name: &str) -> bool {
        std::env::var_os(name).is_some_and(|v| !v.is_empty())
    }

    fn file_present(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn which(&self, program: &str) -> Option<PathBuf> {
        harness::find_on_path(program)
    }

    fn listening(&self, url: &str) -> bool {
        gateway::listening(url)
    }

    fn run(&mut self, command: &str) -> RunResult {
        inventory::run_local(command)
    }

    fn start_gateway(&mut self, yard: &Yard, _config: &Connectors) -> Result<String, String> {
        let by = self.by.clone().ok_or_else(|| {
            "no `by` to start the gateway with: this process is not `by` and found none beside \
             it or on PATH; `by gateway start` starts it"
                .to_owned()
        })?;
        let output = Command::new(&by)
            .args(["gateway", "start", "--json"])
            .current_dir(yard.root())
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("could not run {}: {e}", by.display()))?;
        if !output.status.success() {
            return Err(format!(
                "`by gateway start` failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("`by gateway start --json` printed no JSON: {e}"))?;
        let url = value
            .get("url")
            .and_then(|u| u.as_str())
            .map(str::to_owned)
            .ok_or_else(|| "`by gateway start --json` named no url".to_owned())?;
        // A supervisor that started but whose gateway never answered.
        if value.get("listening").and_then(|l| l.as_bool()) == Some(false) {
            return Err(format!(
                "the gateway's supervisor runs but nothing listens at {url} yet; see {}",
                value
                    .get("log")
                    .and_then(|l| l.as_str())
                    .unwrap_or(".branchyard/gateway/gateway.log")
            ));
        }
        Ok(url)
    }

    fn recipe(&self, _yard: &Yard, _name: &str) -> Readiness {
        Readiness::Unknown
    }

    fn workspace(&self, _yard: &Yard) -> Readiness {
        Readiness::Unknown
    }

    fn record(&mut self, event: &HarnessEvent) {
        best_effort(
            "record the install in the harness log",
            self.log.append(event),
        );
    }
}

impl Yard {
    /// What this repository needs for `config` and the state of each
    /// piece, read from `machine` and changed nowhere: the plan `by up
    /// --plan` prints. See the module comment for the steps.
    pub fn up_plan(
        &self,
        config: &ProjectConfig,
        options: &Options,
        machine: &mut dyn Machine,
    ) -> Result<Plan, Error> {
        plan(self, config, options, machine)
    }

    /// Do the planned steps of `plan` on `machine`: install harnesses,
    /// start the gateway. The plan comes back with each applied step in
    /// its new state (ready, or blocked with what went wrong) and a login
    /// step for each harness installed; the other steps are as they were.
    pub fn up_apply(
        &self,
        config: &ProjectConfig,
        plan: Plan,
        machine: &mut dyn Machine,
    ) -> Result<Plan, Error> {
        apply(self, config, plan, machine)
    }

    /// [`Yard::up_plan`], then [`Yard::up_apply`]: what `by up` does.
    pub fn up(
        &self,
        config: &ProjectConfig,
        options: &Options,
        machine: &mut dyn Machine,
    ) -> Result<Plan, Error> {
        let plan = self.up_plan(config, options, machine)?;
        self.up_apply(config, plan, machine)
    }
}

// The plan.

/// The harnesses `config` needs, sorted: `[defaults] harness`, the
/// `[fleet]` candidates, judges and goal judges, the `[knowledge]`
/// distiller; [`DEFAULT_HARNESS`] when it names none. A profile ID counts
/// as its harness.
pub fn needed_harnesses(config: &ProjectConfig) -> Vec<String> {
    let mut named = BTreeSet::new();
    if let Some(harness) = &config.defaults.harness {
        named.insert(harness.clone());
    }
    for entry in config.fleet.values() {
        named.extend(entry.candidates.iter().map(|c| c.harness.clone()));
        named.extend(entry.judge.iter().map(|j| j.harness.clone()));
        named.extend(entry.goal_judge.iter().map(|j| j.harness.clone()));
    }
    if let Some(distiller) = &config.knowledge.distiller {
        named.insert(distiller.harness.clone());
    }
    if named.is_empty() {
        named.insert(DEFAULT_HARNESS.to_owned());
    }
    named.into_iter().map(|id| harness_of(&id)).collect()
}

/// The harness behind `id`: itself, or the harness of the profile named
/// `id`.
fn harness_of(id: &str) -> String {
    if catalog::harness(id).is_some() {
        return id.to_owned();
    }
    branchyard_harness::profiles::PROFILES
        .iter()
        .find(|p| p.id == id)
        .map_or_else(|| id.to_owned(), |p| p.harness.to_owned())
}

/// The `[harnesses]` policy, as `by harnesses install` reads it.
fn install_policy(config: &ProjectConfig, options: &Options) -> InstallPolicy {
    let mode = config.harnesses.install.map(|m| match m {
        HarnessInstall::Never => InstallMode::Never,
        HarnessInstall::Ask => InstallMode::Ask,
        HarnessInstall::Auto => InstallMode::Auto,
    });
    InstallPolicy::new(mode, config.harnesses.allow.clone(), options.interactive)
}

fn plan(
    yard: &Yard,
    config: &ProjectConfig,
    options: &Options,
    machine: &mut dyn Machine,
) -> Result<Plan, Error> {
    let root = yard.root();
    let mut steps = vec![Step::ready(
        "store",
        format!("{}", root.join(".branchyard").display()),
    )];
    match machine.inventory() {
        Ok(mut inventory) => {
            // A key `by harnesses login ID --api-key` stored counts.
            inventory::count_stored_keys(
                &mut inventory,
                &inventory::stored_keys(root, &config.secrets),
            );
            let policy = install_policy(config, options);
            for harness in needed_harnesses(config) {
                steps.extend(harness_steps(&harness, &inventory, &policy, options));
            }
        }
        Err(why) => steps.push(Step::blocked(
            "harnesses",
            format!("could not detect the harnesses on this machine: {why}"),
            "run `by harnesses --refresh` and fix what it reports",
        )),
    }
    steps.extend(secret_steps(root, config, machine));
    if let Some(step) = models_step(config) {
        steps.push(step);
    }
    if let Some(step) = connectors_step(yard, &config.connectors, machine)? {
        steps.push(step);
    }
    if let Some(step) = provider_step(yard, config, machine) {
        steps.push(step);
    }
    if let Some(step) = workspace_step(yard, config, machine) {
        steps.push(step);
    }
    Ok(Plan { steps })
}

/// `harness.<id>` and, when it is installed, `login.<id>`.
fn harness_steps(
    harness: &str,
    inventory: &Inventory,
    policy: &InstallPolicy,
    options: &Options,
) -> Vec<Step> {
    let name = format!("harness.{harness}");
    if let Some(state) = inventory.get(harness) {
        let version = state
            .version
            .as_deref()
            .map_or_else(|| "version unknown".to_owned(), |v| format!("version {v}"));
        return vec![
            Step::ready(name, format!("{} ({version})", state.path)),
            login_step(harness, state),
        ];
    }
    let install = match inventory::plan(harness, InstallAction::Install, None, &inventory.tools) {
        Ok(plan) => plan,
        Err(why) => {
            let homepage = catalog::harness(harness)
                .and_then(|e| e.homepage.as_deref())
                .map_or_else(String::new, |h| format!("; see {h}"));
            return vec![Step::blocked(
                name,
                format!("not installed, and cannot be from here: {why}"),
                format!("install {harness} by hand{homepage}"),
            )];
        }
    };
    let command = install.command.clone();
    vec![
        match policy.permits(harness, options.yes, options.interactive) {
            Permission::Run => Step::planned(
                name,
                format!("not installed; `{command}` installs it"),
                Action::Install { plan: install },
            ),
            Permission::Ask => Step::needs_person(
                name,
                "not installed",
                format!("confirm the install, or `by up --yes`: it runs `{command}`"),
                Some(Action::Install { plan: install }),
            ),
            Permission::Refused(why) => Step::needs_person(
                name,
                format!("not installed: {why}"),
                format!("install it: `{command}`"),
                None,
            ),
        },
    ]
}

/// `login.<id>`: what the inventory knows of the harness's login, with
/// the catalog's login command or key variables when it has none.
fn login_step(harness: &str, state: &HarnessState) -> Step {
    let name = format!("login.{harness}");
    let detail = &state.login.detail;
    match state.login.state {
        LoginState::LoggedIn => Step::ready(name, format!("{} ({detail})", state.login.label())),
        // The words `by harnesses` and `by doctor` use: logged out only when
        // the harness's own status command said so; otherwise the login is
        // not verified, and the fix is the same one command.
        LoginState::LoggedOut => {
            let what = match state.login.evidence {
                inventory::Evidence::Verified => "logged out",
                _ => "login not verified",
            };
            Step::needs_person(
                name,
                format!("{what} ({detail})"),
                inventory::login_hint(harness),
                None,
            )
        }
        LoginState::Unknown => Step::ready(
            name,
            match detail.is_empty() {
                true => "login not checked".to_owned(),
                false => format!("login not verified ({detail})"),
            },
        ),
    }
}

/// `secret.<NAME>` for each `[secrets]` entry and each `[models]`
/// backend's key: where it is, and whether it is there, by name.
fn secret_steps(root: &Path, config: &ProjectConfig, machine: &dyn Machine) -> Vec<Step> {
    let mut names: BTreeSet<String> = config.secrets.keys().cloned().collect();
    names.extend(
        config
            .models
            .backends
            .values()
            .filter_map(|b| b.key.clone()),
    );
    names
        .into_iter()
        .map(|secret| {
            let name = format!("secret.{secret}");
            match configure::key_source(root, &config.secrets, &secret) {
                KeySource::Env(var) => match machine.var_set(&var) {
                    true => Step::ready(name, format!("${var} is set")),
                    false => Step::needs_person(
                        name,
                        format!("${var} is not set"),
                        format!(
                            "export {var} in the environment `by` runs in (its value never goes \
                             in branchyard.toml)"
                        ),
                        None,
                    ),
                },
                KeySource::File(path) => match machine.file_present(&path) {
                    true => Step::ready(name, format!("{} is present", path.display())),
                    false => Step::needs_person(
                        name,
                        format!("{} is missing", path.display()),
                        format!("create {} holding the secret, mode 0600", path.display()),
                        None,
                    ),
                },
            }
        })
        .collect()
}

/// `models`: the gateway `[models]` describes, when it names a backend.
fn models_step(config: &ProjectConfig) -> Option<Step> {
    if config.models.backends.is_empty() {
        return None;
    }
    Some(match configure::models_config(&config.models) {
        Ok(engine) => Step::ready(
            "models",
            format!(
                "model gateway with {} backend(s) and {} route(s); it starts with each turn \
                 (keys: the secret steps)",
                engine.backends.len(),
                engine.routes.len()
            ),
        ),
        Err(error) => Step::blocked(
            "models",
            error.to_string(),
            "fix [models] (docs/model-gateway.md); `by config validate` checks it",
        ),
    })
}

/// `connectors`: the gateway, when `[connectors]` is set or the
/// repository has bundles: listening, to be started, or without Anvil.
fn connectors_step(
    yard: &Yard,
    config: &Connectors,
    machine: &dyn Machine,
) -> Result<Option<Step>, Error> {
    let root = yard.root();
    let has_bundles = packager::discover(&configure::bundles(root, config))
        .map(|bundles| !bundles.is_empty())
        .unwrap_or(false);
    if config.is_empty() && !has_bundles {
        return Ok(None);
    }
    let located = configure::locate_gateway(yard, config)?;
    if let Some(at) = &located {
        if machine.listening(at.url()) {
            return Ok(Some(Step::ready(
                "connectors",
                format!("gateway at {} ({})", at.url(), at.source()),
            )));
        }
    }
    let detail = match &located {
        Some(at) => format!("nothing listens at {} ({})", at.url(), at.source()),
        None => "no gateway runs for this repository".to_owned(),
    };
    let anvil = configure::anvil_command(config)?;
    let program = anvil.first().cloned().unwrap_or_default();
    let present = match program.contains('/') {
        true => machine.file_present(Path::new(&program)),
        false => machine.which(&program).is_some(),
    };
    Ok(Some(match present {
        true => Step::planned("connectors", detail, Action::StartGateway),
        false => Step::blocked(
            "connectors",
            format!("{detail}; {program} (Anvil, the gateway) is not installed"),
            "install Anvil and put it on PATH, or set [connectors] anvil to its path \
             (docs/connectors.md)",
        ),
    }))
}

/// `provider`: what `[defaults] provider` names, when it is not local.
fn provider_step(yard: &Yard, config: &ProjectConfig, machine: &dyn Machine) -> Option<Step> {
    const NAME: &str = "provider";
    match config.defaults.provider {
        None | Some(ProviderKind::Local) => None,
        Some(ProviderKind::Microsandbox) => Some(match &config.microsandbox {
            None => Step::blocked(
                NAME,
                "[defaults] provider = \"microsandbox\", but there is no [microsandbox] section",
                "add [microsandbox] with an image to branchyard.toml (docs/providers.md)",
            ),
            Some(sandbox) => {
                let options = configure::sandbox_options(sandbox);
                match doctor::check_microsandbox(yard, &options) {
                    Ok(()) => Step::ready(
                        NAME,
                        format!(
                            "microsandbox with image {}: this build can run it",
                            options.image
                        ),
                    ),
                    Err(error) => Step::blocked(
                        NAME,
                        error.to_string(),
                        format!(
                            "rebuild with `{}` (needs KVM and libcap-ng) and give \
                             [microsandbox] an image (docs/modes.md#sandbox)",
                            branchyard_microsandbox::REBUILD
                        ),
                    ),
                }
            }
        }),
        Some(ProviderKind::Recipe) => Some(match config.defaults.recipe.as_deref() {
            None => Step::blocked(
                NAME,
                "[defaults] provider = \"recipe\", but [defaults] recipe names none",
                "set recipe under [defaults] to one of your [recipes.NAME]",
            ),
            Some(name) => match machine.recipe(yard, name) {
                Readiness::Ready => Step::ready(NAME, format!("recipe {name} may run")),
                Readiness::Undefined => Step::blocked(
                    NAME,
                    format!("[defaults] recipe = {name:?}, but there is no [recipes.{name}]"),
                    "define the recipe in branchyard.toml or your user file (docs/recipes.md)",
                ),
                Readiness::NeedsTrust => Step::needs_person(
                    NAME,
                    format!("recipe {name} has commands you have not trusted as they are now"),
                    format!("review `by recipe show {name}`, then `by recipe trust {name}`"),
                    None,
                ),
                Readiness::Unknown => Step::needs_person(
                    NAME,
                    format!("recipe {name}: whether it may run is your trust decision, which `by` reads"),
                    format!("`by recipe check {name}` says, and `by recipe trust {name}` decides"),
                    None,
                ),
            },
        }),
    }
}

/// `workspace`: the `[workspace]` scripts' trust, when there are any.
fn workspace_step(yard: &Yard, config: &ProjectConfig, machine: &dyn Machine) -> Option<Step> {
    const NAME: &str = "workspace";
    let workspace = config.workspace.as_ref()?;
    if !workspace.has_scripts() {
        return None;
    }
    Some(match machine.workspace(yard) {
        Readiness::Ready | Readiness::Undefined => {
            Step::ready(NAME, "the [workspace] scripts may run")
        }
        Readiness::NeedsTrust => Step::needs_person(
            NAME,
            "the [workspace] scripts are not trusted as they are now",
            "read them in branchyard.toml, then `by workspace trust`",
            None,
        ),
        Readiness::Unknown => Step::needs_person(
            NAME,
            "whether the [workspace] scripts may run is your trust decision, which `by` reads",
            "`by workspace trust` after reading them, if you have not",
            None,
        ),
    })
}

// Applying.

fn apply(
    yard: &Yard,
    config: &ProjectConfig,
    mut plan: Plan,
    machine: &mut dyn Machine,
) -> Result<Plan, Error> {
    let mut logins: Vec<(usize, Step)> = Vec::new();
    for (index, step) in plan.steps.iter_mut().enumerate() {
        if step.state != State::Planned {
            continue;
        }
        let Some(action) = step.action.clone() else {
            continue;
        };
        match action {
            Action::Install { plan: install } => {
                let installed = {
                    let cell = RefCell::new(&mut *machine);
                    inventory::install(
                        &install,
                        None,
                        &mut |command| cell.borrow_mut().run(command),
                        &mut |harness| cell.borrow_mut().detect(harness),
                    )
                };
                machine.record(&HarnessEvent {
                    at_ms: now_ms(),
                    on: "local".to_owned(),
                    harness: install.harness.clone(),
                    action: InstallAction::Install,
                    by: "by up".to_owned(),
                    command: Some(install.command.clone()),
                    outcome: match installed.verified {
                        true => "verified".to_owned(),
                        false => "failed".to_owned(),
                    },
                    version_before: None,
                    version_after: installed.after.as_ref().and_then(|a| a.version.clone()),
                    detail: installed.problem.clone(),
                });
                let name = step.name.clone();
                *step = match (&installed.problem, &installed.after) {
                    (None, Some(after)) => Step::ready(
                        name,
                        format!(
                            "installed with `{}`: {} ({})",
                            install.command,
                            after.path,
                            after.version.as_deref().map_or_else(
                                || "version unknown".to_owned(),
                                |v| format!("version {v}")
                            )
                        ),
                    ),
                    (problem, _) => Step::blocked(
                        name,
                        format!(
                            "`{}` did not install it: {}",
                            install.command,
                            problem.as_deref().unwrap_or("not found afterwards")
                        ),
                        format!(
                            "see the command's output (the end: {:?}) and install it by hand",
                            tail_line(&installed.output_tail)
                        ),
                    ),
                };
                if let Some(after) = &installed.after {
                    logins.push((index + 1, login_step(&install.harness, after)));
                }
            }
            Action::StartGateway => {
                let name = step.name.clone();
                *step = match machine.start_gateway(yard, &config.connectors) {
                    // Started is not up: the step is ready only once
                    // something answers at the URL.
                    Ok(url) if machine.listening(&url) => {
                        Step::ready(name, format!("gateway started at {url}"))
                    }
                    Ok(url) => Step::blocked(
                        name,
                        format!("the gateway was started but nothing listens at {url} yet"),
                        "see .branchyard/gateway/gateway.log, then `by gateway status`",
                    ),
                    Err(why) => Step::blocked(
                        name,
                        format!("the gateway did not start: {why}"),
                        "see .branchyard/gateway/gateway.log, then `by gateway start`",
                    ),
                };
            }
        }
    }
    for (offset, (index, step)) in logins.into_iter().enumerate() {
        plan.steps.insert(index + offset, step);
    }
    Ok(plan)
}

/// The last non-empty line of `text`, for a message.
fn tail_line(text: &str) -> &str {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{Evidence, Login};
    use std::collections::BTreeMap;

    /// A machine with what each test puts on it.
    struct Fake {
        inventory: Result<Inventory, String>,
        vars: BTreeSet<String>,
        files: BTreeSet<PathBuf>,
        programs: BTreeSet<String>,
        listening: BTreeSet<String>,
        /// What `detect` finds after an install, by harness.
        installed: BTreeMap<String, HarnessState>,
        gateway: Result<String, String>,
        /// Whether a gateway `start_gateway` reports started comes up.
        gateway_listens: bool,
        recipe: Readiness,
        workspace: Readiness,
        runs: Vec<String>,
        events: Vec<HarnessEvent>,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                inventory: Ok(inventory(&[], &[])),
                vars: BTreeSet::new(),
                files: BTreeSet::new(),
                programs: BTreeSet::new(),
                listening: BTreeSet::new(),
                installed: BTreeMap::new(),
                gateway: Err("not started".into()),
                gateway_listens: true,
                recipe: Readiness::Unknown,
                workspace: Readiness::Unknown,
                runs: Vec::new(),
                events: Vec::new(),
            }
        }
    }

    impl Machine for Fake {
        fn inventory(&mut self) -> Result<Inventory, String> {
            self.inventory.clone()
        }

        fn detect(&mut self, harness: &str) -> Result<Inventory, String> {
            let found = self.installed.get(harness).cloned();
            Ok(inventory_of(found.into_iter().collect(), &[]))
        }

        fn var_set(&self, name: &str) -> bool {
            self.vars.contains(name)
        }

        fn file_present(&self, path: &Path) -> bool {
            self.files.contains(path)
        }

        fn which(&self, program: &str) -> Option<PathBuf> {
            self.programs
                .contains(program)
                .then(|| PathBuf::from("/usr/bin").join(program))
        }

        fn listening(&self, url: &str) -> bool {
            self.listening.contains(url)
        }

        fn run(&mut self, command: &str) -> RunResult {
            self.runs.push(command.to_owned());
            Ok((Some(0), "added 1 package\n".to_owned()))
        }

        fn start_gateway(&mut self, _yard: &Yard, _config: &Connectors) -> Result<String, String> {
            let started = self.gateway.clone()?;
            if self.gateway_listens {
                self.listening.insert(started.clone());
            }
            Ok(started)
        }

        fn recipe(&self, _yard: &Yard, _name: &str) -> Readiness {
            self.recipe
        }

        fn workspace(&self, _yard: &Yard) -> Readiness {
            self.workspace
        }

        fn record(&mut self, event: &HarnessEvent) {
            self.events.push(event.clone());
        }
    }

    fn harness(id: &str, version: &str, login: LoginState) -> HarnessState {
        HarnessState {
            id: id.to_owned(),
            path: format!("/opt/bin/{id}"),
            on_path: true,
            version: Some(version.to_owned()),
            version_note: None,
            login: Login {
                state: login,
                evidence: Evidence::Verified,
                detail: match login {
                    LoginState::LoggedIn => "status: logged in".to_owned(),
                    LoginState::LoggedOut => "status: not logged in".to_owned(),
                    LoginState::Unknown => String::new(),
                },
                ..Login::default()
            },
            quota: None,
        }
    }

    fn inventory_of(harnesses: Vec<HarnessState>, tools: &[&str]) -> Inventory {
        Inventory {
            host: "fake".to_owned(),
            os: "linux".to_owned(),
            detected_at_ms: 0,
            checked: Vec::new(),
            harnesses,
            tools: tools
                .iter()
                .map(|t| ((*t).to_owned(), format!("/usr/bin/{t}")))
                .collect(),
        }
    }

    fn inventory(harnesses: &[HarnessState], tools: &[&str]) -> Inventory {
        inventory_of(harnesses.to_vec(), tools)
    }

    fn repository() -> tempfile::TempDir {
        let dir = tempfile::Builder::new().prefix("by-up-").tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    fn parsed(text: &str) -> ProjectConfig {
        config::parse(text).unwrap()
    }

    fn names(plan: &Plan) -> Vec<&str> {
        plan.steps.iter().map(|s| s.name.as_str()).collect()
    }

    fn step<'a>(plan: &'a Plan, name: &str) -> &'a Step {
        plan.steps
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no step {name} in {:?}", names(plan)))
    }

    #[test]
    fn nothing_configured_needs_the_default_harness_and_is_ready_when_it_is_there() {
        let dir = repository();
        let yard = Yard::open(dir.path()).unwrap();
        let mut machine = Fake::new();
        machine.inventory = Ok(inventory(
            &[harness("claude-code", "2.1.0", LoginState::LoggedIn)],
            &["npm"],
        ));
        let plan = yard
            .up_plan(&ProjectConfig::default(), &Options::default(), &mut machine)
            .unwrap();
        assert_eq!(
            names(&plan),
            ["store", "harness.claude-code", "login.claude-code"]
        );
        assert!(plan.ready(), "{plan:?}");
        assert_eq!(plan.state(), State::Ready);
        assert!(step(&plan, "harness.claude-code")
            .detail
            .contains("version 2.1.0"));
        assert_eq!(plan.json()["ready"], true);
        assert_eq!(plan.json()["steps"][0]["state"], "ready");
    }

    #[test]
    fn the_needed_harnesses_come_from_every_table_that_names_one() {
        let config = parsed(
            r#"
[defaults]
harness = "codex"
[fleet.feature]
candidates = [{ harness = "gemini-cli" }, { harness = "codex" }]
judge = { harness = "claude-code" }
[knowledge]
distiller = { harness = "qwen-code" }
"#,
        );
        assert_eq!(
            needed_harnesses(&config),
            ["claude-code", "codex", "gemini-cli", "qwen-code"]
        );
        assert_eq!(
            needed_harnesses(&ProjectConfig::default()),
            [DEFAULT_HARNESS]
        );
    }

    #[test]
    fn a_missing_harness_is_planned_installed_or_left_to_the_person_by_the_policy() {
        let dir = repository();
        let yard = Yard::open(dir.path()).unwrap();
        let config = parsed("[defaults]\nharness = \"codex\"\n");
        let mut machine = Fake::new();
        machine.inventory = Ok(inventory(&[], &["npm"]));
        // Unset: ask on a terminal, never elsewhere.
        let plan = yard
            .up_plan(&config, &Options::default(), &mut machine)
            .unwrap();
        let codex = step(&plan, "harness.codex");
        assert_eq!(codex.state, State::NeedsPerson);
        assert!(codex.action.is_none(), "{codex:?}");
        assert!(codex.todo.as_deref().unwrap_or_default().contains("npm"));
        // On a terminal: the person's yes.
        let interactive = Options {
            yes: false,
            interactive: true,
        };
        let mut plan = yard.up_plan(&config, &interactive, &mut machine).unwrap();
        let codex = step(&plan, "harness.codex");
        assert_eq!(codex.state, State::NeedsPerson);
        assert!(matches!(codex.action, Some(Action::Install { .. })));
        assert_eq!(plan.askable().count(), 1);
        assert!(plan.approve("harness.codex"));
        assert!(!plan.approve("harness.codex"), "approved once");
        assert_eq!(step(&plan, "harness.codex").state, State::Planned);
        // `--yes` on a terminal: planned. Off a terminal an unset policy
        // is `never`, as for `by harnesses install`.
        let yes = Options {
            yes: true,
            interactive: true,
        };
        let plan = yard.up_plan(&config, &yes, &mut machine).unwrap();
        assert_eq!(step(&plan, "harness.codex").state, State::Planned);
        assert_eq!(plan.state(), State::Planned);
        let yes_off_terminal = Options {
            yes: true,
            interactive: false,
        };
        let plan = yard
            .up_plan(&config, &yes_off_terminal, &mut machine)
            .unwrap();
        assert_eq!(step(&plan, "harness.codex").state, State::NeedsPerson);
        // `install = "auto"` in the user file: planned anywhere.
        let auto = parsed("[defaults]\nharness = \"codex\"\n[harnesses]\ninstall = \"auto\"\n");
        let plan = yard
            .up_plan(&auto, &Options::default(), &mut machine)
            .unwrap();
        assert_eq!(step(&plan, "harness.codex").state, State::Planned);
        // Applied: the fake install puts codex there at the pinned version,
        // logged out.
        let pinned = match &step(&plan, "harness.codex").action {
            Some(Action::Install { plan }) => plan.pinned.clone(),
            other => panic!("not an install: {other:?}"),
        };
        machine.installed.insert(
            "codex".into(),
            harness(
                "codex",
                pinned.as_deref().unwrap_or("0.150.0"),
                LoginState::LoggedOut,
            ),
        );
        let applied = yard.up_apply(&config, plan, &mut machine).unwrap();
        assert_eq!(machine.runs.len(), 1);
        assert!(machine.runs[0].contains("npm"), "{:?}", machine.runs);
        assert_eq!(names(&applied), ["store", "harness.codex", "login.codex"]);
        assert_eq!(step(&applied, "harness.codex").state, State::Ready);
        let login = step(&applied, "login.codex");
        assert_eq!(login.state, State::NeedsPerson);
        assert!(login.todo.as_deref().unwrap_or_default().contains("codex"));
        assert_eq!(machine.events.len(), 1);
        assert_eq!(machine.events[0].outcome, "verified");
        assert_eq!(machine.events[0].by, "by up");
        assert!(!applied.ready());
    }

    #[test]
    fn an_install_that_leaves_nothing_behind_is_blocked_with_the_output() {
        let dir = repository();
        let yard = Yard::open(dir.path()).unwrap();
        let config = parsed("[defaults]\nharness = \"codex\"\n[harnesses]\ninstall = \"auto\"\n");
        let mut machine = Fake::new();
        machine.inventory = Ok(inventory(&[], &["npm"]));
        let applied = yard.up(&config, &Options::default(), &mut machine).unwrap();
        let codex = step(&applied, "harness.codex");
        assert_eq!(codex.state, State::Blocked);
        assert!(codex.detail.contains("still not found"), "{codex:?}");
        assert!(codex
            .todo
            .as_deref()
            .unwrap_or_default()
            .contains("added 1 package"));
        assert_eq!(machine.events[0].outcome, "failed");
    }

    #[test]
    fn a_harness_no_tool_can_install_is_blocked_and_never_run() {
        let dir = repository();
        let yard = Yard::open(dir.path()).unwrap();
        let config = parsed("[defaults]\nharness = \"codex\"\n[harnesses]\ninstall = \"auto\"\n");
        let mut machine = Fake::new();
        machine.inventory = Ok(inventory(&[], &[]));
        let applied = yard.up(&config, &Options::default(), &mut machine).unwrap();
        assert_eq!(step(&applied, "harness.codex").state, State::Blocked);
        assert!(machine.runs.is_empty());
    }

    #[test]
    fn secrets_are_checked_by_name_from_their_variable_or_file() {
        let dir = repository();
        let yard = Yard::open(dir.path()).unwrap();
        let config = parsed(
            r#"
[secrets]
OPENAI_API_KEY = "MY_OPENAI"
GITHUB_TOKEN = "@keys/github"
[models.backends.zai]
api = "openai"
url = "https://api.z.ai/v1"
key = "ZAI_KEY"
"#,
        );
        let mut machine = Fake::new();
        machine.inventory = Ok(inventory(
            &[harness("claude-code", "2.1.0", LoginState::LoggedIn)],
            &[],
        ));
        machine.vars.insert("MY_OPENAI".into());
        let plan = yard
            .up_plan(&config, &Options::default(), &mut machine)
            .unwrap();
        assert_eq!(step(&plan, "secret.OPENAI_API_KEY").state, State::Ready);
        assert_eq!(
            step(&plan, "secret.OPENAI_API_KEY").detail,
            "$MY_OPENAI is set"
        );
        let github = step(&plan, "secret.GITHUB_TOKEN");
        assert_eq!(github.state, State::NeedsPerson);
        assert!(github.detail.contains("keys/github"), "{github:?}");
        let zai = step(&plan, "secret.ZAI_KEY");
        assert_eq!(zai.state, State::NeedsPerson);
        assert_eq!(zai.detail, "$ZAI_KEY is not set");
        assert!(step(&plan, "models").detail.contains("1 backend"));
        // Nothing prints a value.
        let text = serde_json::to_string(&plan.json()).unwrap();
        assert!(!text.contains("secret-value"));
        machine.vars.insert("ZAI_KEY".into());
        machine
            .files
            .insert(dir.path().canonicalize().unwrap().join("keys/github"));
        let plan = yard
            .up_plan(&config, &Options::default(), &mut machine)
            .unwrap();
        assert!(plan.ready(), "{plan:?}");
    }

    #[test]
    fn the_connector_gateway_is_started_when_anvil_is_there_and_blocked_when_not() {
        let dir = repository();
        let yard = Yard::open(dir.path()).unwrap();
        let config = parsed("[connectors]\ngateway = \"http://127.0.0.1:7/mcp\"\n");
        let mut machine = Fake::new();
        machine.inventory = Ok(inventory(
            &[harness("claude-code", "2.1.0", LoginState::LoggedIn)],
            &[],
        ));
        let plan = yard
            .up_plan(&config, &Options::default(), &mut machine)
            .unwrap();
        let gateway = step(&plan, "connectors");
        assert_eq!(gateway.state, State::Blocked);
        assert!(gateway.detail.contains("anvil"), "{gateway:?}");
        machine.programs.insert("anvil".into());
        let plan = yard
            .up_plan(&config, &Options::default(), &mut machine)
            .unwrap();
        let gateway = step(&plan, "connectors");
        assert_eq!(gateway.state, State::Planned);
        assert_eq!(gateway.action, Some(Action::StartGateway));
        machine.gateway = Ok("http://127.0.0.1:7/mcp".to_owned());
        let applied = yard.up_apply(&config, plan, &mut machine).unwrap();
        assert_eq!(step(&applied, "connectors").state, State::Ready);
        assert!(applied.ready(), "{applied:?}");
        // Already listening: nothing to do.
        machine.listening.insert("http://127.0.0.1:7/mcp".into());
        let plan = yard
            .up_plan(&config, &Options::default(), &mut machine)
            .unwrap();
        assert_eq!(step(&plan, "connectors").state, State::Ready);
        assert!(step(&plan, "connectors").detail.contains("pinned"));
        // A start that fails is blocked with its reason.
        machine.listening.clear();
        machine.gateway = Err("anvil exited with status 3".to_owned());
        let applied = yard.up(&config, &Options::default(), &mut machine).unwrap();
        let gateway = step(&applied, "connectors");
        assert_eq!(gateway.state, State::Blocked);
        assert!(gateway.detail.contains("status 3"));
        // A start that reports a URL nothing answers at is not ready either.
        machine.gateway = Ok("http://127.0.0.1:7/mcp".to_owned());
        machine.gateway_listens = false;
        let applied = yard.up(&config, &Options::default(), &mut machine).unwrap();
        let gateway = step(&applied, "connectors");
        assert_eq!(gateway.state, State::Blocked);
        assert!(gateway.detail.contains("nothing listens"), "{gateway:?}");
        assert!(!applied.ready());
    }

    #[test]
    fn no_connectors_configured_and_no_bundles_is_no_gateway_step() {
        let dir = repository();
        let yard = Yard::open(dir.path()).unwrap();
        let mut machine = Fake::new();
        machine.inventory = Ok(inventory(
            &[harness("claude-code", "2.1.0", LoginState::LoggedIn)],
            &[],
        ));
        let plan = yard
            .up_plan(&ProjectConfig::default(), &Options::default(), &mut machine)
            .unwrap();
        assert!(!names(&plan).contains(&"connectors"));
    }

    #[test]
    fn a_recipe_or_workspace_trust_decision_is_the_persons() {
        let dir = repository();
        let yard = Yard::open(dir.path()).unwrap();
        let config = parsed(
            r#"
[defaults]
provider = "recipe"
recipe = "devbox"
[recipes.devbox]
create = "echo created"
[workspace]
setup = "npm ci"
"#,
        );
        let mut machine = Fake::new();
        machine.inventory = Ok(inventory(
            &[harness("claude-code", "2.1.0", LoginState::LoggedIn)],
            &[],
        ));
        let plan = yard
            .up_plan(&config, &Options::default(), &mut machine)
            .unwrap();
        assert_eq!(step(&plan, "provider").state, State::NeedsPerson);
        assert!(step(&plan, "provider")
            .todo
            .as_deref()
            .unwrap_or_default()
            .contains("by recipe check devbox"));
        assert_eq!(step(&plan, "workspace").state, State::NeedsPerson);
        machine.recipe = Readiness::Ready;
        machine.workspace = Readiness::Ready;
        let plan = yard
            .up_plan(&config, &Options::default(), &mut machine)
            .unwrap();
        assert!(plan.ready(), "{plan:?}");
        machine.recipe = Readiness::NeedsTrust;
        machine.workspace = Readiness::NeedsTrust;
        let plan = yard
            .up_plan(&config, &Options::default(), &mut machine)
            .unwrap();
        assert!(step(&plan, "provider")
            .todo
            .as_deref()
            .unwrap_or_default()
            .contains("by recipe trust devbox"));
        assert!(step(&plan, "workspace")
            .todo
            .as_deref()
            .unwrap_or_default()
            .contains("by workspace trust"));
        machine.recipe = Readiness::Undefined;
        let plan = yard
            .up_plan(&config, &Options::default(), &mut machine)
            .unwrap();
        assert_eq!(step(&plan, "provider").state, State::Blocked);
    }

    #[test]
    fn a_logged_out_harness_names_its_login_and_a_detection_failure_blocks() {
        let dir = repository();
        let yard = Yard::open(dir.path()).unwrap();
        let mut machine = Fake::new();
        machine.inventory = Ok(inventory(
            &[harness("claude-code", "2.1.0", LoginState::LoggedOut)],
            &[],
        ));
        let plan = yard
            .up_plan(&ProjectConfig::default(), &Options::default(), &mut machine)
            .unwrap();
        let login = step(&plan, "login.claude-code");
        assert_eq!(login.state, State::NeedsPerson);
        assert!(login.detail.contains("not logged in"));
        machine.inventory = Err("sh: not found".into());
        let plan = yard
            .up_plan(&ProjectConfig::default(), &Options::default(), &mut machine)
            .unwrap();
        assert_eq!(step(&plan, "harnesses").state, State::Blocked);
        assert_eq!(plan.state(), State::Blocked);
    }

    #[test]
    fn a_sandbox_provider_this_build_cannot_run_is_blocked() {
        let dir = repository();
        let yard = Yard::open(dir.path()).unwrap();
        let config =
            parsed("[defaults]\nprovider = \"microsandbox\"\n[microsandbox]\nimage = \"img:1\"\n");
        let mut machine = Fake::new();
        machine.inventory = Ok(inventory(
            &[harness("claude-code", "2.1.0", LoginState::LoggedIn)],
            &[],
        ));
        let plan = yard
            .up_plan(&config, &Options::default(), &mut machine)
            .unwrap();
        let provider = step(&plan, "provider");
        match cfg!(feature = "microsandbox") {
            true => assert_eq!(provider.state, State::Ready),
            false => assert_eq!(provider.state, State::Blocked),
        }
        // The parser refuses the file; a built configuration reaches the
        // same step.
        let without = ProjectConfig {
            defaults: config::Defaults {
                provider: Some(ProviderKind::Microsandbox),
                ..config::Defaults::default()
            },
            ..ProjectConfig::default()
        };
        let plan = yard
            .up_plan(&without, &Options::default(), &mut machine)
            .unwrap();
        assert_eq!(step(&plan, "provider").state, State::Blocked);
    }

    #[test]
    fn steps_serialize_with_their_state_names() {
        let step = Step::needs_person(
            "harness.codex",
            "not installed",
            "confirm",
            Some(Action::StartGateway),
        );
        let value = serde_json::to_value(&step).unwrap();
        assert_eq!(value["state"], "needs_person");
        assert_eq!(value["action"]["kind"], "start_gateway");
        let back: Step = serde_json::from_value(value).unwrap();
        assert_eq!(back, step);
        assert_eq!(State::Blocked.as_str(), "blocked");
        assert!(State::Ready < State::Planned && State::Planned < State::NeedsPerson);
    }
}
