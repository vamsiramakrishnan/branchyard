//! The seven readiness checks `by up`, `by doctor`, `by setup` and `by
//! substrate check` report (docs/up.md#readiness-checks), each kept
//! separate so that a harness found on `PATH` is never reported as able to
//! run a task.
//!
//! A status comes from a typed finding, never from a step's prose: the
//! harness inventory ([`HarnessFindings`]), the staged Substrate check
//! ([`SubstrateProbe`]), and the steps' names and states. `verified` means
//! this run established it (a version command answered, Control accepted
//! the token, a worker listing showed a free slot); `unverified` that a
//! source is present and nothing confirmed it; `unknown` that nothing was
//! asked, or the answer said nothing; `not_applicable` that the configured
//! target has no such thing; `failed` that it was checked and is missing,
//! refused or unreachable, with what to do.

use std::collections::BTreeMap;
use std::path::Path;

use branchyard_substrate::{ControlError, SubstrateProvider, WorkerCapacity};
use serde::{Deserialize, Serialize};

use super::{State, Step};
use crate::config::{ProjectConfig, ProviderKind};
use crate::configure;
use crate::inventory::{Evidence as Proof, HarnessState, LoginState};

/// The seven checks, in report order.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessCheckId {
    /// The `by` CLI and each needed native harness are installed.
    CliAndHarnessInstalled,
    /// The project and user configuration load and name what the target needs.
    ConfigurationValid,
    /// The operator's own credentials for the remote target (token, bridge
    /// key, TLS files) are present, and accepted when it was asked.
    OperatorCredentialsAvailable,
    /// The remote control plane answered.
    RemoteControlPlaneReachable,
    /// Somewhere to place a task has room.
    WorkerCapacityAvailable,
    /// The target supports what the configuration asks of it: a template's
    /// bridge contract, a sandbox build, a connector gateway.
    ExecutionCapabilitiesSupported,
    /// Each needed native harness is authenticated.
    HarnessAuthVerified,
}

impl ReadinessCheckId {
    /// The seven, in report order.
    pub const ALL: [ReadinessCheckId; 7] = [
        ReadinessCheckId::CliAndHarnessInstalled,
        ReadinessCheckId::ConfigurationValid,
        ReadinessCheckId::OperatorCredentialsAvailable,
        ReadinessCheckId::RemoteControlPlaneReachable,
        ReadinessCheckId::WorkerCapacityAvailable,
        ReadinessCheckId::ExecutionCapabilitiesSupported,
        ReadinessCheckId::HarnessAuthVerified,
    ];

    /// What the check is called in a table.
    pub fn label(self) -> &'static str {
        match self {
            ReadinessCheckId::CliAndHarnessInstalled => "CLI and native harness installed",
            ReadinessCheckId::ConfigurationValid => "configuration valid",
            ReadinessCheckId::OperatorCredentialsAvailable => "operator credentials available",
            ReadinessCheckId::RemoteControlPlaneReachable => "remote control plane reachable",
            ReadinessCheckId::WorkerCapacityAvailable => "worker capacity available",
            ReadinessCheckId::ExecutionCapabilitiesSupported => {
                "requested execution capabilities supported"
            }
            ReadinessCheckId::HarnessAuthVerified => "native harness authentication verified",
        }
    }
}

/// How far a check got. Ordered by how much it needs, so the worst of
/// several is their maximum.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The configured target has no such thing: no control plane for local
    /// execution.
    NotApplicable,
    /// This run established it.
    Verified,
    /// A source is present (a key variable, a credential file, an image that
    /// should hold the harness) and nothing confirmed it.
    Unverified,
    /// Nothing was asked, or the answer said nothing.
    Unknown,
    /// Checked, and missing, refused, unreachable or unsupported.
    Failed,
}

impl CheckStatus {
    /// `not_applicable`, `verified`, `unverified`, `unknown` or `failed`.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::NotApplicable => "not_applicable",
            CheckStatus::Verified => "verified",
            CheckStatus::Unverified => "unverified",
            CheckStatus::Unknown => "unknown",
            CheckStatus::Failed => "failed",
        }
    }
}

/// One of the seven checks: what was found and, unless verified or not
/// applicable, what to do.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessCheck {
    /// Which check.
    pub id: ReadinessCheckId,
    /// What it is called in a table.
    pub label: String,
    /// How far it got.
    pub status: CheckStatus,
    /// What was found, in one line.
    pub detail: String,
    /// What to do next, in one line, when there is something to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

impl ReadinessCheck {
    fn new(id: ReadinessCheckId, status: CheckStatus, detail: impl Into<String>) -> ReadinessCheck {
        ReadinessCheck {
            id,
            label: id.label().to_owned(),
            status,
            detail: super::one_line(detail),
            next: None,
        }
    }

    /// This run established it.
    pub fn verified(id: ReadinessCheckId, detail: impl Into<String>) -> ReadinessCheck {
        ReadinessCheck::new(id, CheckStatus::Verified, detail)
    }

    /// A source is present and nothing confirmed it.
    pub fn unverified(id: ReadinessCheckId, detail: impl Into<String>) -> ReadinessCheck {
        ReadinessCheck::new(id, CheckStatus::Unverified, detail)
    }

    /// Nothing was asked, or the answer said nothing.
    pub fn unknown(id: ReadinessCheckId, detail: impl Into<String>) -> ReadinessCheck {
        ReadinessCheck::new(id, CheckStatus::Unknown, detail)
    }

    /// The configured target has no such thing.
    pub fn not_applicable(id: ReadinessCheckId, detail: impl Into<String>) -> ReadinessCheck {
        ReadinessCheck::new(id, CheckStatus::NotApplicable, detail)
    }

    /// Checked and not there, with what to do.
    pub fn failed(
        id: ReadinessCheckId,
        detail: impl Into<String>,
        next: impl Into<String>,
    ) -> ReadinessCheck {
        ReadinessCheck::new(id, CheckStatus::Failed, detail).next(next)
    }

    /// With what to do next.
    pub fn next(mut self, next: impl Into<String>) -> ReadinessCheck {
        self.next = Some(super::one_line(next));
        self
    }
}

/// What this machine's harness inventory established for one harness: an
/// installed check and an authentication check, each with its evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessFindings {
    /// Found, on `PATH`, new enough, and whether its version command answered.
    pub installed: ReadinessCheck,
    /// What its login detection found.
    pub auth: ReadinessCheck,
}

impl HarnessFindings {
    /// From what detection found of the harness: its executable, its
    /// version command's answer and its login, by name only.
    pub fn of(state: &HarnessState) -> HarnessFindings {
        use ReadinessCheckId::{CliAndHarnessInstalled as Installed, HarnessAuthVerified as Auth};
        let id = &state.id;
        let installed = if !state.on_path {
            ReadinessCheck::failed(
                Installed,
                format!("{id} is at {}, which is not on PATH", state.path),
                "add its directory to PATH, then run by harnesses --refresh",
            )
        } else if let Some((why, fix)) = state.too_old() {
            ReadinessCheck::failed(Installed, format!("{id} {why}"), fix)
        } else {
            match &state.version {
                Some(version) => ReadinessCheck::verified(
                    Installed,
                    format!(
                        "{id} at {}: its version command answered {version}",
                        state.path
                    ),
                ),
                None => ReadinessCheck::unverified(
                    Installed,
                    format!(
                        "{id} at {}: its version command gave no version ({})",
                        state.path,
                        state.version_note.as_deref().unwrap_or("no output")
                    ),
                )
                .next(format!(
                    "run {} --version yourself, then by harnesses --refresh",
                    state.path
                )),
            }
        };
        let login = &state.login;
        let hint = state.login_hint();
        let auth = if state.quota.as_ref().is_some_and(|q| q.limit_reached) {
            ReadinessCheck::failed(
                Auth,
                format!("{id}: its login has reached its usage limit"),
                "by usage shows when it resets",
            )
        } else if login.key_unused() {
            ReadinessCheck::failed(
                Auth,
                format!("{id}: {} ({})", login.label(), login.detail),
                format!("by harnesses login {id}"),
            )
        } else {
            match (login.state, login.evidence) {
                (LoginState::LoggedIn, Proof::Verified) => ReadinessCheck::verified(
                    Auth,
                    format!("{id}: its status command says logged in ({})", login.detail),
                ),
                (LoginState::LoggedIn, _) => ReadinessCheck::unverified(
                    Auth,
                    format!(
                        "{id}: {} ({}); no status command confirmed it, and only a model call would",
                        login.label(),
                        login.detail
                    ),
                ),
                (LoginState::LoggedOut, Proof::Verified) => ReadinessCheck::failed(
                    Auth,
                    format!("{id}: its status command says logged out ({})", login.detail),
                    hint,
                ),
                (LoginState::LoggedOut, _) => ReadinessCheck::unknown(
                    Auth,
                    format!(
                        "{id}: no credential file or key variable Branchyard knows was found ({})",
                        login.detail
                    ),
                )
                .next(hint),
                (LoginState::Unknown, _) => ReadinessCheck::unknown(
                    Auth,
                    match login.detail.is_empty() {
                        true => format!("{id}: login not checked"),
                        false => format!("{id}: login not checked ({})", login.detail),
                    },
                )
                .next(hint),
            }
        };
        HarnessFindings { installed, auth }
    }
}

/// What the read-only Substrate check found. Each stage runs only when the
/// one before it passed, so a variant names the first stage that did not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubstrateProbe {
    /// Substrate is the provider and there is no `[substrate]` section.
    Unconfigured,
    /// The section does not describe a connection Branchyard can make
    /// (scheme, atespace, template, TLS files); Control was not contacted.
    Invalid(String),
    /// A local operator credential (the bridge signing key, the API token
    /// file) is missing or unreadable; Control was not contacted.
    Credentials(String),
    /// Control did not answer.
    Unreachable(String),
    /// Control answered and refused the operator's credentials.
    Rejected(String),
    /// The template's bridge verifying key is not the configured key's.
    SignerMismatch(String),
    /// Control accepted the credentials; the template is missing or unusable.
    Template(String),
    /// Control accepted the credentials and the template was inspected.
    Inspected {
        /// The template's name.
        template: String,
        /// The image its actors run, which should hold the harness.
        image: String,
        /// One worker listing, when `--smoke` asked for it.
        workers: Option<Result<WorkerCapacity, ControlError>>,
    },
}

/// Check the saved Substrate connection read-only: the local credentials,
/// then one authenticated `GetActorTemplate`, then, with `smoke`, one
/// `ListWorkers` page. It creates no actor and calls no model.
pub fn probe_substrate(config: &ProjectConfig, smoke: bool) -> SubstrateProbe {
    let Some(section) = config.substrate.as_ref() else {
        return SubstrateProbe::Unconfigured;
    };
    let options = configure::substrate_options(section);
    let signer = match branchyard_bridge::Signer::read(&options.key) {
        Ok(signer) => signer,
        Err(error) => {
            return SubstrateProbe::Credentials(unreadable(
                "the bridge signing key",
                &options.key,
                &error,
            ))
        }
    };
    if let Some(problem) = options.api_token_file.as_deref().and_then(token_problem) {
        return SubstrateProbe::Credentials(problem);
    }
    let connection = match options.connection_config_with_signer(signer) {
        Ok(connection) => connection,
        Err(error) => return SubstrateProbe::Invalid(error.to_string()),
    };
    let provider = match SubstrateProvider::connect(connection) {
        Ok(provider) => provider,
        Err(error) => return SubstrateProbe::Invalid(error.to_string()),
    };
    match provider.probe_template() {
        Ok(inspection) => SubstrateProbe::Inspected {
            template: inspection.name,
            image: inspection.image,
            workers: smoke.then(|| provider.worker_capacity()),
        },
        Err(ControlError::Unreachable(why)) => SubstrateProbe::Unreachable(why),
        Err(ControlError::Rejected(why)) => SubstrateProbe::Rejected(why),
        Err(ControlError::SignerMismatch(why)) => SubstrateProbe::SignerMismatch(why),
        Err(ControlError::Answered(why)) => SubstrateProbe::Template(why),
    }
}

/// Why the API token file cannot be sent, by its path; never its contents.
fn token_problem(path: &Path) -> Option<String> {
    match std::fs::File::open(path).and_then(|file| file.metadata()) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => None,
        Ok(_) => Some(format!("the API token file {} is empty", path.display())),
        Err(error) => Some(unreadable("the API token file", path, &error)),
    }
}

/// `what` at `path` could not be used, in words rather than an OS error.
fn unreadable(what: &str, path: &Path, error: &std::io::Error) -> String {
    let shown = path.display();
    match error.kind() {
        std::io::ErrorKind::NotFound => format!("{what} {shown} does not exist"),
        std::io::ErrorKind::PermissionDenied => format!("{what} {shown} is not readable by you"),
        _ => format!("{what} {shown} cannot be used: {error}"),
    }
}

impl SubstrateProbe {
    /// The `provider` step: ready once Control authenticated and the
    /// template was inspected, otherwise blocked with what to change.
    pub fn step(&self) -> Step {
        const NAME: &str = "provider";
        match self {
            SubstrateProbe::Unconfigured => Step::blocked(
                NAME,
                "Substrate selected without [substrate] configuration",
                "configure [substrate] endpoint, router, template and key (docs/substrate.md)",
            ),
            SubstrateProbe::Inspected { template, .. } => Step::ready(
                NAME,
                format!(
                    "Substrate Control authenticated; template {template} inspected; actor \
                     execution and model access unverified"
                ),
            ),
            SubstrateProbe::Invalid(why)
            | SubstrateProbe::Credentials(why)
            | SubstrateProbe::Unreachable(why)
            | SubstrateProbe::Rejected(why)
            | SubstrateProbe::SignerMismatch(why)
            | SubstrateProbe::Template(why) => Step::blocked(NAME, why, self.remedy()),
        }
    }

    /// What to change for a stage that did not pass.
    fn remedy(&self) -> &'static str {
        match self {
            SubstrateProbe::Unconfigured => "configure [substrate] (docs/substrate.md)",
            SubstrateProbe::Invalid(_) => {
                "fix [substrate] endpoint, atespace, template and TLS files, then rerun `by up --plan` (docs/substrate.md)"
            }
            SubstrateProbe::Credentials(_) => {
                "point [substrate] key at a readable 0600 key and api_token_file at a nonempty token file, then rerun `by up --plan`"
            }
            SubstrateProbe::Unreachable(_) => {
                "check [substrate] endpoint, the network or port forward, and TLS server_name/CA; a guided GKE connection is refreshed by `by setup --guided`; then rerun `by up --plan`"
            }
            SubstrateProbe::Rejected(_) => {
                "refresh the token [substrate] api_token_file names (a guided GKE connection: `by setup --guided`), then rerun `by up --plan`"
            }
            SubstrateProbe::SignerMismatch(_) => {
                "point [substrate] key at the key the template was made with, or review `by substrate setup --plan`"
            }
            SubstrateProbe::Template(_) | SubstrateProbe::Inspected { .. } => {
                "review the template with `by substrate setup --plan` (docs/substrate-setup.md)"
            }
        }
    }
}

/// The seven checks for a Substrate target from `probe` alone, as `by
/// doctor` and `by substrate check` report them; the remote harness's
/// credential sources are read from `machine` under `root`.
pub fn substrate_checks(
    root: &Path,
    config: &ProjectConfig,
    machine: &dyn super::Machine,
    probe: SubstrateProbe,
) -> Vec<ReadinessCheck> {
    let mut steps = super::remote_harness_readiness(root, config, machine);
    steps.push(probe.step());
    let findings = Findings {
        substrate: Some(probe),
        ..Findings::default()
    };
    readiness_checks(config, &steps, &findings)
}

/// What the checks are computed from besides the steps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Findings {
    /// What this machine's inventory established for each needed harness it
    /// found, by harness ID.
    pub harnesses: BTreeMap<String, HarnessFindings>,
    /// What the Substrate check found, when Substrate is the provider.
    pub substrate: Option<SubstrateProbe>,
}

/// The seven checks for `config`, from the `steps` a plan found and the
/// `findings` beside them, in [`ReadinessCheckId::ALL`] order.
pub fn readiness_checks(
    config: &ProjectConfig,
    steps: &[Step],
    findings: &Findings,
) -> Vec<ReadinessCheck> {
    let provider = config.defaults.provider.unwrap_or(ProviderKind::Local);
    let provider_step = steps.iter().find(|s| s.name == "provider");
    let mut checks = vec![configuration(config, steps, provider_step)];
    match (provider, &findings.substrate) {
        (ProviderKind::Substrate, probe) => {
            let probe = probe.as_ref().unwrap_or(&SubstrateProbe::Unconfigured);
            checks.extend(substrate(probe, config));
            checks.push(remote_auth(steps));
        }
        (local, _) => {
            checks.extend(on_this_machine(local, provider_step, steps));
            let needed = super::needed_harnesses(config);
            checks.push(harnesses(&needed, steps, findings));
            checks.push(combine(
                ReadinessCheckId::HarnessAuthVerified,
                needed.iter().map(|id| match findings.harnesses.get(id) {
                    Some(found) => found.auth.clone(),
                    None => ReadinessCheck::unknown(
                        ReadinessCheckId::HarnessAuthVerified,
                        format!("{id}: not installed here, so its login was not checked"),
                    )
                    .next(format!(
                        "by up installs {id} when the [harnesses] policy allows it"
                    )),
                }),
            ));
        }
    }
    checks.sort_by_key(|check| check.id);
    checks
}

/// The worst of several findings for one check, their details joined.
fn combine(
    id: ReadinessCheckId,
    found: impl IntoIterator<Item = ReadinessCheck>,
) -> ReadinessCheck {
    let found: Vec<ReadinessCheck> = found.into_iter().collect();
    let Some(worst) = found.iter().max_by_key(|check| check.status) else {
        return ReadinessCheck::unknown(id, "nothing to check");
    };
    ReadinessCheck {
        id,
        label: id.label().to_owned(),
        status: worst.status,
        detail: found
            .iter()
            .map(|check| check.detail.as_str())
            .collect::<Vec<_>>()
            .join("; "),
        next: worst.next.clone(),
    }
}

/// The configuration loaded (the caller would have stopped otherwise); a
/// step that blocks on a section the configuration lacks fails it.
fn configuration(
    config: &ProjectConfig,
    steps: &[Step],
    provider_step: Option<&Step>,
) -> ReadinessCheck {
    let id = ReadinessCheckId::ConfigurationValid;
    let blocked = |step: &&Step| step.state == State::Blocked;
    if let Some(models) = steps.iter().filter(blocked).find(|s| s.name == "models") {
        return ReadinessCheck::failed(id, &models.detail, models.todo.clone().unwrap_or_default());
    }
    let provider = config.defaults.provider.unwrap_or(ProviderKind::Local);
    let section_missing = match provider {
        ProviderKind::Local => false,
        ProviderKind::Substrate => config.substrate.is_none(),
        ProviderKind::Microsandbox => config.microsandbox.is_none(),
        // Both ways a recipe step blocks are configuration: none named, or none by that name.
        ProviderKind::Recipe => provider_step.is_some_and(|s| s.state == State::Blocked),
    };
    match (section_missing, provider_step) {
        (true, Some(step)) => {
            ReadinessCheck::failed(id, &step.detail, step.todo.clone().unwrap_or_default())
        }
        (true, None) => ReadinessCheck::failed(
            id,
            "the provider's configuration section is missing",
            "by setup --guided",
        ),
        (false, _) => ReadinessCheck::verified(
            id,
            "the configuration loaded and names what its provider needs",
        ),
    }
}

/// Credentials, control plane, capacity and capabilities for a provider
/// that runs tasks on this machine.
fn on_this_machine(
    provider: ProviderKind,
    provider_step: Option<&Step>,
    steps: &[Step],
) -> [ReadinessCheck; 4] {
    use ReadinessCheckId::*;
    let credentials = ReadinessCheck::not_applicable(
        OperatorCredentialsAvailable,
        "tasks run on this machine as you; no remote operator credential is used",
    );
    let control = ReadinessCheck::not_applicable(
        RemoteControlPlaneReachable,
        "no remote control plane is configured: tasks run on this machine",
    );
    let not_checked = |id| {
        ReadinessCheck::unknown(id, "not checked until the provider check passes")
            .next("by up --plan")
    };
    match (provider, provider_step) {
        (ProviderKind::Microsandbox, Some(step)) if step.state == State::Ready => [
            credentials,
            control,
            ReadinessCheck::unverified(
                WorkerCapacityAvailable,
                "this host's KVM access was checked; no microVM was started",
            ),
            ReadinessCheck::verified(ExecutionCapabilitiesSupported, &step.detail),
        ],
        (ProviderKind::Microsandbox, Some(step)) => [
            credentials,
            control,
            not_checked(WorkerCapacityAvailable),
            ReadinessCheck::failed(
                ExecutionCapabilitiesSupported,
                &step.detail,
                step.todo.clone().unwrap_or_default(),
            ),
        ],
        (ProviderKind::Recipe, step) => [
            credentials,
            control,
            ReadinessCheck::unknown(
                WorkerCapacityAvailable,
                "a recipe's own transport decides where tasks run",
            )
            .next("by recipe check NAME runs the recipe's doctor"),
            match step {
                Some(step) if step.state == State::Ready => {
                    ReadinessCheck::verified(ExecutionCapabilitiesSupported, &step.detail)
                }
                Some(step) if step.state == State::NeedsPerson => {
                    let check =
                        ReadinessCheck::unverified(ExecutionCapabilitiesSupported, &step.detail);
                    match &step.todo {
                        Some(todo) => check.next(todo),
                        None => check,
                    }
                }
                _ => not_checked(ExecutionCapabilitiesSupported),
            },
        ],
        _ => [
            credentials,
            control,
            ReadinessCheck::not_applicable(
                WorkerCapacityAvailable,
                "tasks run as processes on this machine",
            ),
            connectors(steps),
        ],
    }
}

/// Local execution supports what it is asked unless the connector gateway
/// the configuration names cannot run.
fn connectors(steps: &[Step]) -> ReadinessCheck {
    let id = ReadinessCheckId::ExecutionCapabilitiesSupported;
    match steps.iter().find(|s| s.name == "connectors") {
        Some(step) if step.state == State::Blocked => {
            ReadinessCheck::failed(id, &step.detail, step.todo.clone().unwrap_or_default())
        }
        Some(step) if step.state == State::Planned => {
            ReadinessCheck::unverified(id, format!("connector gateway: {}", step.detail))
                .next("by up starts it")
        }
        Some(step) => ReadinessCheck::verified(id, format!("connector gateway: {}", step.detail)),
        None => ReadinessCheck::verified(
            id,
            "local worktree execution; no connector gateway asked for",
        ),
    }
}

/// Each needed harness on this machine: what its step says when it is not
/// installed, what detection established when it is.
fn harnesses(needed: &[String], steps: &[Step], findings: &Findings) -> ReadinessCheck {
    let id = ReadinessCheckId::CliAndHarnessInstalled;
    if let Some(detection) = steps.iter().find(|s| s.name == "harnesses") {
        return ReadinessCheck::failed(
            id,
            &detection.detail,
            detection.todo.clone().unwrap_or_default(),
        );
    }
    combine(
        id,
        needed.iter().map(|harness| {
            let step = steps
                .iter()
                .find(|s| s.name == format!("harness.{harness}"));
            match (findings.harnesses.get(harness), step) {
                (Some(found), _) => found.installed.clone(),
                (None, Some(step)) => {
                    let check = ReadinessCheck::failed(
                        id,
                        format!("{harness}: {}", step.detail),
                        step.todo.clone().unwrap_or_else(|| "by up".to_owned()),
                    );
                    match step.state {
                        // `by up` installs it when applied.
                        State::Planned => {
                            ReadinessCheck::unknown(id, format!("{harness}: {}", step.detail))
                                .next("by up")
                        }
                        _ => check,
                    }
                }
                (None, None) => ReadinessCheck::unknown(id, format!("{harness}: not detected"))
                    .next("by harnesses --refresh"),
            }
        }),
    )
}

/// The remote harness's credential sources, as `login.<id>` steps found
/// them for an actor's private home.
fn remote_auth(steps: &[Step]) -> ReadinessCheck {
    let id = ReadinessCheckId::HarnessAuthVerified;
    combine(
        id,
        steps
            .iter()
            .filter(|s| s.name.starts_with("login."))
            .map(|step| match step.state {
                State::Ready => ReadinessCheck::unverified(
                    id,
                    format!(
                        "{}: {}; no model was called",
                        step.name.trim_start_matches("login."),
                        step.detail
                    ),
                ),
                _ => ReadinessCheck::failed(
                    id,
                    format!(
                        "{}: {}",
                        step.name.trim_start_matches("login."),
                        step.detail
                    ),
                    step.todo.clone().unwrap_or_default(),
                ),
            }),
    )
}

/// Installed, credentials, control plane, capacity and capabilities from
/// the staged Substrate check.
fn substrate(probe: &SubstrateProbe, config: &ProjectConfig) -> [ReadinessCheck; 5] {
    use ReadinessCheckId::*;
    let endpoint = config
        .substrate
        .as_ref()
        .map_or("the configured endpoint", |s| s.endpoint.as_str());
    let harness = config.defaults.harness.as_deref().map_or("agy", |id| {
        if id == "antigravity" {
            "agy"
        } else {
            id
        }
    });
    let actor_smoke = format!(
        "by substrate check --smoke --harness {harness} (creates and deletes one disposable actor; no model call)"
    );
    let remedy = probe.remedy();
    let not_contacted = |id, why: &str| ReadinessCheck::unknown(id, format!("not checked: {why}"));
    let after_template = |id| {
        ReadinessCheck::unknown(id, "not checked until the template inspection passes").next(remedy)
    };
    let installed_unknown = || {
        ReadinessCheck::unknown(
            CliAndHarnessInstalled,
            "the harness comes from the template's image, which was not inspected",
        )
        .next(remedy)
    };
    match probe {
        SubstrateProbe::Unconfigured | SubstrateProbe::Invalid(_) => [
            installed_unknown(),
            not_contacted(
                OperatorCredentialsAvailable,
                "the [substrate] configuration is not usable",
            ),
            not_contacted(
                RemoteControlPlaneReachable,
                "the [substrate] configuration is not usable",
            ),
            after_template(WorkerCapacityAvailable),
            after_template(ExecutionCapabilitiesSupported),
        ],
        SubstrateProbe::Credentials(why) => [
            installed_unknown(),
            ReadinessCheck::failed(OperatorCredentialsAvailable, why, remedy),
            not_contacted(
                RemoteControlPlaneReachable,
                "Control is not contacted without usable operator credentials",
            ),
            after_template(WorkerCapacityAvailable),
            after_template(ExecutionCapabilitiesSupported),
        ],
        SubstrateProbe::Unreachable(why) => [
            installed_unknown(),
            ReadinessCheck::unverified(
                OperatorCredentialsAvailable,
                "the bridge key and token file are readable here; Control did not answer, so it has not accepted them",
            ),
            ReadinessCheck::failed(
                RemoteControlPlaneReachable,
                format!("Control at {endpoint} did not answer: {why}"),
                remedy,
            ),
            after_template(WorkerCapacityAvailable),
            after_template(ExecutionCapabilitiesSupported),
        ],
        SubstrateProbe::Rejected(why) => [
            installed_unknown(),
            ReadinessCheck::failed(
                OperatorCredentialsAvailable,
                format!("Control refused the operator credentials: {why}"),
                remedy,
            ),
            ReadinessCheck::verified(
                RemoteControlPlaneReachable,
                format!("Control at {endpoint} answered"),
            ),
            after_template(WorkerCapacityAvailable),
            after_template(ExecutionCapabilitiesSupported),
        ],
        SubstrateProbe::SignerMismatch(why) => [
            installed_unknown(),
            ReadinessCheck::failed(OperatorCredentialsAvailable, why, remedy),
            ReadinessCheck::verified(
                RemoteControlPlaneReachable,
                format!("Control at {endpoint} answered and accepted the token"),
            ),
            after_template(WorkerCapacityAvailable),
            after_template(ExecutionCapabilitiesSupported),
        ],
        SubstrateProbe::Template(why) => [
            installed_unknown(),
            ReadinessCheck::verified(
                OperatorCredentialsAvailable,
                "Control accepted the operator token",
            ),
            ReadinessCheck::verified(
                RemoteControlPlaneReachable,
                format!("Control at {endpoint} answered"),
            ),
            after_template(WorkerCapacityAvailable),
            ReadinessCheck::failed(ExecutionCapabilitiesSupported, why, remedy),
        ],
        SubstrateProbe::Inspected {
            template,
            image,
            workers,
        } => [
            ReadinessCheck::unverified(
                CliAndHarnessInstalled,
                format!(
                    "by is installed here; {harness} comes from template {template}'s image {image}, where it has not been run"
                ),
            )
            .next(&actor_smoke),
            ReadinessCheck::verified(
                OperatorCredentialsAvailable,
                "Control accepted the operator token, and the template's verifying key matches the bridge key",
            ),
            ReadinessCheck::verified(
                RemoteControlPlaneReachable,
                format!("Control at {endpoint} authenticated the request"),
            ),
            capacity(workers.as_ref(), &actor_smoke),
            ReadinessCheck::verified(
                ExecutionCapabilitiesSupported,
                format!("template {template} declares the bridge execution contract"),
            ),
        ],
    }
}

/// Worker capacity from the `--smoke` listing, if it ran.
fn capacity(
    workers: Option<&Result<WorkerCapacity, ControlError>>,
    actor_smoke: &str,
) -> ReadinessCheck {
    let id = ReadinessCheckId::WorkerCapacityAvailable;
    let page = |more: bool| {
        if more {
            " (first page of workers only)"
        } else {
            ""
        }
    };
    match workers {
        None => ReadinessCheck::unknown(id, "workers were not listed").next(
            "by up --plan --smoke lists them (one read-only ListWorkers call)",
        ),
        Some(Err(error)) => {
            let why = match error {
                ControlError::Rejected(_) => "Control refused to list workers to this credential",
                ControlError::Unreachable(_) => "the worker listing got no answer",
                ControlError::Answered(_) | ControlError::SignerMismatch(_) => {
                    "Control did not list workers"
                }
            };
            ReadinessCheck::unknown(id, format!("{why}: {error}")).next(actor_smoke)
        }
        Some(Ok(found)) if found.active == 0 => ReadinessCheck::failed(
            id,
            format!(
                "Control lists {} worker(s), none of them schedulable{}",
                found.listed,
                page(found.more)
            ),
            "start or scale the Substrate worker pool, then rerun by up --plan --smoke",
        ),
        Some(Ok(found)) if found.free_actor_slots > 0 => ReadinessCheck::verified(
            id,
            format!(
                "{} schedulable worker(s) with {} free actor slot(s); CPU and memory placement not checked{}",
                found.active,
                found.free_actor_slots,
                page(found.more)
            ),
        ),
        Some(Ok(found)) if found.reporting == found.active => ReadinessCheck::failed(
            id,
            format!(
                "all {} schedulable worker(s) are at their actor capacity{}",
                found.active,
                page(found.more)
            ),
            "wait for running actors to finish or scale the worker pool, then rerun by up --plan --smoke",
        ),
        Some(Ok(found)) => ReadinessCheck::unverified(
            id,
            format!(
                "{} schedulable worker(s), {} without a reported actor capacity, so placement is not checked{}",
                found.active,
                found.active - found.reporting,
                page(found.more)
            ),
        )
        .next(actor_smoke),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Login;

    fn state(id: &str) -> HarnessState {
        HarnessState {
            id: id.into(),
            path: format!("/bin/{id}"),
            on_path: true,
            version: Some("1.0.0".into()),
            ..Default::default()
        }
    }

    fn login(state: LoginState, evidence: Proof, detail: &str) -> Login {
        Login {
            state,
            evidence,
            detail: detail.into(),
            ..Default::default()
        }
    }

    fn statuses(checks: &[ReadinessCheck]) -> Vec<CheckStatus> {
        checks.iter().map(|c| c.status).collect()
    }

    #[test]
    fn installed_is_verified_only_when_the_version_command_answered() {
        let found = HarnessFindings::of(&state("codex"));
        assert_eq!(found.installed.status, CheckStatus::Verified);
        assert!(found
            .installed
            .detail
            .contains("version command answered 1.0.0"));

        let silent = HarnessState {
            version: None,
            version_note: Some("timed out".into()),
            ..state("codex")
        };
        let found = HarnessFindings::of(&silent);
        assert_eq!(found.installed.status, CheckStatus::Unverified);
        assert!(found.installed.detail.contains("timed out"));
        assert!(found.installed.next.is_some());

        let off_path = HarnessState {
            on_path: false,
            ..state("codex")
        };
        let found = HarnessFindings::of(&off_path);
        assert_eq!(found.installed.status, CheckStatus::Failed);
        assert!(found.installed.next.as_deref().unwrap().contains("PATH"));
    }

    #[test]
    fn each_login_state_maps_to_its_own_auth_status() {
        let auth = |login: Login| {
            HarnessFindings::of(&HarnessState {
                login,
                ..state("codex")
            })
            .auth
        };
        let verified = auth(login(LoginState::LoggedIn, Proof::Verified, "Logged in"));
        assert_eq!(verified.status, CheckStatus::Verified);

        let likely = auth(Login {
            key: Some("OPENAI_API_KEY".into()),
            ..login(LoginState::LoggedIn, Proof::Likely, "OPENAI_API_KEY is set")
        });
        assert_eq!(likely.status, CheckStatus::Unverified);
        assert!(
            likely.detail.contains("only a model call would"),
            "{}",
            likely.detail
        );

        let out = auth(login(
            LoginState::LoggedOut,
            Proof::Verified,
            "Not logged in",
        ));
        assert_eq!(out.status, CheckStatus::Failed);
        assert!(out.next.is_some());

        let nothing = auth(login(
            LoginState::LoggedOut,
            Proof::Likely,
            "no credentials",
        ));
        assert_eq!(nothing.status, CheckStatus::Unknown);
        assert!(nothing.next.is_some());

        let unchecked = auth(login(LoginState::Unknown, Proof::None, ""));
        assert_eq!(unchecked.status, CheckStatus::Unknown);
        assert_eq!(unchecked.detail, "codex: login not checked");

        let limited = HarnessFindings::of(&HarnessState {
            quota: Some(crate::inventory::Quota {
                limit_reached: true,
                ..Default::default()
            }),
            ..state("codex")
        });
        assert_eq!(limited.auth.status, CheckStatus::Failed);
    }

    fn substrate_config() -> ProjectConfig {
        crate::config::parse(
            r#"
[defaults]
harness = "antigravity"
provider = "substrate"
[substrate]
endpoint = "https://control.example"
router = "https://router.example"
atespace = "yard"
template = "agy"
key = "/nonexistent/bridge.key"
egress = { mode = "deny" }
"#,
        )
        .unwrap()
    }

    fn substrate_checks(probe: SubstrateProbe) -> Vec<ReadinessCheck> {
        let steps = vec![
            probe.step(),
            Step::ready(
                "login.antigravity",
                "GEMINI_API_KEY credential source available for remote private home; model authentication unverified",
            ),
        ];
        let findings = Findings {
            substrate: Some(probe),
            ..Default::default()
        };
        readiness_checks(&substrate_config(), &steps, &findings)
    }

    #[test]
    fn substrate_stages_land_on_their_own_checks() {
        use CheckStatus::*;
        let inspected = |workers| SubstrateProbe::Inspected {
            template: "agy".into(),
            image: "registry.example/agy@sha256:00".into(),
            workers,
        };
        let cases: Vec<(SubstrateProbe, [CheckStatus; 7])> = vec![
            (
                SubstrateProbe::Credentials("the API token file /t does not exist".into()),
                [
                    Unknown, Verified, Failed, Unknown, Unknown, Unknown, Unverified,
                ],
            ),
            (
                SubstrateProbe::Unreachable("tcp connect error".into()),
                [
                    Unknown, Verified, Unverified, Failed, Unknown, Unknown, Unverified,
                ],
            ),
            (
                SubstrateProbe::Rejected("missing or incorrect API bearer token".into()),
                [
                    Unknown, Verified, Failed, Verified, Unknown, Unknown, Unverified,
                ],
            ),
            (
                SubstrateProbe::SignerMismatch("keys differ".into()),
                [
                    Unknown, Verified, Failed, Verified, Unknown, Unknown, Unverified,
                ],
            ),
            (
                SubstrateProbe::Template("actors/absent".into()),
                [
                    Unknown, Verified, Verified, Verified, Unknown, Failed, Unverified,
                ],
            ),
            (
                inspected(None),
                [
                    Unverified, Verified, Verified, Verified, Unknown, Verified, Unverified,
                ],
            ),
            (
                inspected(Some(Ok(WorkerCapacity {
                    listed: 2,
                    active: 2,
                    reporting: 2,
                    free_actor_slots: 3,
                    more: false,
                }))),
                [
                    Unverified, Verified, Verified, Verified, Verified, Verified, Unverified,
                ],
            ),
            (
                inspected(Some(Ok(WorkerCapacity {
                    listed: 1,
                    active: 0,
                    reporting: 0,
                    free_actor_slots: 0,
                    more: false,
                }))),
                [
                    Unverified, Verified, Verified, Verified, Failed, Verified, Unverified,
                ],
            ),
            (
                inspected(Some(Ok(WorkerCapacity {
                    listed: 1,
                    active: 1,
                    reporting: 1,
                    free_actor_slots: 0,
                    more: false,
                }))),
                [
                    Unverified, Verified, Verified, Verified, Failed, Verified, Unverified,
                ],
            ),
            (
                inspected(Some(Ok(WorkerCapacity {
                    listed: 1,
                    active: 1,
                    reporting: 0,
                    free_actor_slots: 0,
                    more: false,
                }))),
                [
                    Unverified, Verified, Verified, Verified, Unverified, Verified, Unverified,
                ],
            ),
            (
                inspected(Some(Err(ControlError::Rejected("denied".into())))),
                [
                    Unverified, Verified, Verified, Verified, Unknown, Verified, Unverified,
                ],
            ),
        ];
        for (probe, expected) in cases {
            let shown = format!("{probe:?}");
            let checks = substrate_checks(probe);
            let ids: Vec<ReadinessCheckId> = checks.iter().map(|c| c.id).collect();
            assert_eq!(ids, ReadinessCheckId::ALL, "{shown}");
            assert_eq!(statuses(&checks), expected, "{shown}");
            for check in &checks {
                if check.status == Failed {
                    assert!(check.next.is_some(), "{shown}: {check:?}");
                }
                assert!(!check.detail.contains("ready to execute"), "{check:?}");
            }
        }
    }

    #[test]
    fn worker_capacity_and_installed_point_at_the_explicit_actor_smoke() {
        let checks = substrate_checks(SubstrateProbe::Inspected {
            template: "agy".into(),
            image: "registry.example/agy@sha256:00".into(),
            workers: None,
        });
        let installed = &checks[0];
        assert!(
            installed.detail.contains("has not been run"),
            "{installed:?}"
        );
        let next = installed.next.as_deref().unwrap();
        assert!(
            next.starts_with("by substrate check --smoke --harness agy"),
            "{next}"
        );
        assert!(
            next.contains("creates and deletes one disposable actor"),
            "{next}"
        );
        let capacity = &checks[4];
        assert_eq!(capacity.status, CheckStatus::Unknown);
        assert!(capacity.next.as_deref().unwrap().contains("--smoke"));
    }

    #[test]
    fn a_missing_substrate_section_fails_configuration_and_leaves_the_rest_unknown() {
        let mut config = substrate_config();
        config.substrate = None;
        let probe = probe_substrate(&config, true);
        assert_eq!(probe, SubstrateProbe::Unconfigured);
        let findings = Findings {
            substrate: Some(probe.clone()),
            ..Default::default()
        };
        let checks = readiness_checks(&config, &[probe.step()], &findings);
        assert_eq!(checks[1].status, CheckStatus::Failed);
        assert_eq!(checks[2].status, CheckStatus::Unknown);
        assert_eq!(checks[3].status, CheckStatus::Unknown);
    }

    #[test]
    fn an_unreadable_bridge_key_or_token_file_is_a_credential_failure_before_any_call() {
        let scratch = branchyard_testkit::Scratch::new("readiness-credentials");
        let config = substrate_config();
        assert!(matches!(
            probe_substrate(&config, false),
            SubstrateProbe::Credentials(why) if why.contains("bridge signing key")
        ));
        let key = scratch.path().join("bridge.key");
        branchyard_bridge::Signer::write(&key).unwrap();
        let mut config = substrate_config();
        if let Some(section) = config.substrate.as_mut() {
            section.key = key.display().to_string();
            section.api_token_file =
                Some(scratch.path().join("absent.token").display().to_string());
        }
        assert!(matches!(
            probe_substrate(&config, false),
            SubstrateProbe::Credentials(why) if why.contains("does not exist")
        ));
        let empty = scratch.path().join("empty.token");
        std::fs::write(&empty, "").unwrap();
        if let Some(section) = config.substrate.as_mut() {
            section.api_token_file = Some(empty.display().to_string());
        }
        assert!(matches!(
            probe_substrate(&config, false),
            SubstrateProbe::Credentials(why) if why.contains("is empty")
        ));
    }

    #[test]
    fn several_harnesses_report_the_worst_with_every_detail() {
        let a = ReadinessCheck::verified(ReadinessCheckId::HarnessAuthVerified, "a: ok");
        let b = ReadinessCheck::unknown(ReadinessCheckId::HarnessAuthVerified, "b: not checked")
            .next("by harnesses login b");
        let both = combine(ReadinessCheckId::HarnessAuthVerified, [a, b]);
        assert_eq!(both.status, CheckStatus::Unknown);
        assert_eq!(both.detail, "a: ok; b: not checked");
        assert_eq!(both.next.as_deref(), Some("by harnesses login b"));
    }

    #[test]
    fn statuses_serialize_in_snake_case() {
        let check =
            ReadinessCheck::not_applicable(ReadinessCheckId::RemoteControlPlaneReachable, "local");
        let value = serde_json::to_value(&check).unwrap();
        assert_eq!(value["id"], "remote_control_plane_reachable");
        assert_eq!(value["status"], "not_applicable");
        assert_eq!(value["label"], "remote control plane reachable");
        assert!(value.get("next").is_none());
        for status in [
            CheckStatus::NotApplicable,
            CheckStatus::Verified,
            CheckStatus::Unverified,
            CheckStatus::Unknown,
            CheckStatus::Failed,
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), status.as_str());
        }
    }
}
