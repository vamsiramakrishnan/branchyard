//! Operator execution presets for workspace sessions
//! (docs/server.md#execution-presets): what discovery offers a caller, how
//! ready each environment looks from this server, what a session start's
//! `preset` resolves to, and the projection that keeps a preset's configured
//! values out of what its sessions and their branches show.
//!
//! A preset grants nothing. It resolves to the request fields a session
//! start already has (`provider`, provisioned secrets, `require_labels`),
//! which then pass the same checks as any request. Readiness informs a
//! person and never refuses a start.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex};

use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use axum::Json;
use branchyard_client::api::Operation;
use branchyard_client::workspace_api::{
    PresetCredentials, PresetOrigin, ReadinessScope, ReadyState, StartSessionRequest,
};
use branchyard_support::LockExt as _;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{json, Value};

use super::auth;
use crate::api::{App, Caller};
use crate::config::{substrate_options, ExecutionPreset};
use crate::console_store::{Collection, Record};
use crate::error::ApiError;

/// How long an API-host check is reused before it runs again, so a viewer
/// cannot make the server reread its keys on every request and a repaired
/// file is noticed within a minute.
const CHECK_TTL_MS: u64 = 60_000;

/// Why a readiness value is what it is. Each reason fixes the state, the
/// scope, the text and the action, so no error message, path, endpoint or
/// secret name can reach a viewer through readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    /// A local preset's sessions run as processes of a server or worker.
    LocalProcesses,
    /// `SubstrateOptions::connection_config` loads on this server.
    SubstrateReadable,
    /// It does not.
    SubstrateUnreadable,
    /// This build runs Microsandbox.
    MicrosandboxBuilt,
    /// This build does not.
    MicrosandboxMissing,
    /// Discovery never contacts a remote environment.
    ContactedAtStart,
    /// The harness's executable is on this server.
    ExecutableFound,
    /// It is not.
    ExecutableMissing,
    /// A Substrate template installs the harness.
    TemplateProvidesHarness,
    /// A sandbox image installs the harness.
    ImageProvidesHarness,
    /// The preset supplies the harness's key; a session verifies it.
    KeyConfigured,
    /// The preset supplies no key for the harness.
    NoKey,
}

/// What an operator can do about a readiness value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    CheckEnvironment,
    BuildWithMicrosandbox,
    InstallHarness,
    AddKey,
}

impl Reason {
    /// The state, scope, text and action this reason fixes.
    fn facts(self) -> (ReadyState, ReadinessScope, &'static str, Option<Action>) {
        use ReadinessScope::{ApiHost, Environment};
        use ReadyState::{NotReady, Ready, Unknown};
        match self {
            Reason::LocalProcesses => (
                Ready,
                ApiHost,
                "Sessions run as processes on a Branchyard server or worker, with the harnesses \
                 installed there.",
                None,
            ),
            Reason::SubstrateReadable => (
                Ready,
                ApiHost,
                "This server can read the Substrate settings and credential files.",
                None,
            ),
            Reason::SubstrateUnreadable => (
                NotReady,
                ApiHost,
                "This server cannot read the Substrate settings for this environment.",
                Some(Action::CheckEnvironment),
            ),
            Reason::MicrosandboxBuilt => (
                Ready,
                ApiHost,
                "This server's build can run Microsandbox environments.",
                None,
            ),
            Reason::MicrosandboxMissing => (
                NotReady,
                ApiHost,
                "This server's build cannot run Microsandbox environments; a worker built with \
                 them may.",
                Some(Action::BuildWithMicrosandbox),
            ),
            Reason::ContactedAtStart => (
                Unknown,
                Environment,
                "The cluster is contacted when a session starts, not before.",
                None,
            ),
            Reason::ExecutableFound => (
                Ready,
                ApiHost,
                "This server found the harness's executable.",
                None,
            ),
            Reason::ExecutableMissing => (
                NotReady,
                ApiHost,
                "This server did not find the harness's executable; another worker may have it.",
                Some(Action::InstallHarness),
            ),
            Reason::TemplateProvidesHarness => (
                Unknown,
                Environment,
                "The Substrate template provides the harness; discovery does not contact the \
                 cluster.",
                None,
            ),
            Reason::ImageProvidesHarness => (
                Unknown,
                Environment,
                "The sandbox image provides the harness; discovery does not start a sandbox.",
                None,
            ),
            Reason::KeyConfigured => (
                Unknown,
                Environment,
                "Credentials are configured on the server; they are verified when a session \
                 starts.",
                None,
            ),
            Reason::NoKey => (
                Unknown,
                Environment,
                "This environment supplies no key for the harness, and sessions do not use the \
                 server's own login.",
                Some(Action::AddKey),
            ),
        }
    }
}

impl Action {
    fn text(self) -> &'static str {
        match self {
            Action::CheckEnvironment => {
                "An operator should check this environment in the server configuration and run \
                 `by serve --check`."
            }
            Action::BuildWithMicrosandbox => {
                "An operator should run these sessions on workers built with Microsandbox support."
            }
            Action::InstallHarness => {
                "An operator can install the harness on this server or configure its command."
            }
            Action::AddKey => "An operator can add the harness's key to this preset's secrets.",
        }
    }
}

/// One readiness value as discovery reports it: `{state, scope, reason,
/// action}`, and `checked_ms_ago` for a cached check.
#[derive(Clone, Copy, Debug)]
struct Readiness {
    reason: Reason,
    checked_ms_ago: Option<u64>,
}

impl Readiness {
    fn fixed(reason: Reason) -> Readiness {
        Readiness {
            reason,
            checked_ms_ago: None,
        }
    }

    fn checked(reason: Reason, age_ms: u64) -> Readiness {
        Readiness {
            reason,
            checked_ms_ago: Some(age_ms),
        }
    }
}

impl Serialize for Readiness {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Wire {
            state: ReadyState,
            scope: ReadinessScope,
            reason: &'static str,
            action: Option<&'static str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            checked_ms_ago: Option<u64>,
        }
        let (state, scope, reason, action) = self.reason.facts();
        Wire {
            state,
            scope,
            reason,
            action: action.map(Action::text),
            checked_ms_ago: self.checked_ms_ago,
        }
        .serialize(serializer)
    }
}

/// API-host checks by what they check, each with the time it ran.
type Checks<T> = Mutex<HashMap<String, (u64, T)>>;

/// The harnesses this server can run, keyed by the configured commands.
static INSTALLED: LazyLock<Checks<Vec<Installed>>> = LazyLock::new(Default::default);

/// Whether a Substrate environment's settings load, keyed by its options.
static READABLE: LazyLock<Checks<bool>> = LazyLock::new(Default::default);

/// `check`'s value for `key` and its age in milliseconds, reused for up to
/// [`CHECK_TTL_MS`].
fn cached<T: Clone>(
    checks: &Checks<T>,
    key: String,
    check: impl FnOnce() -> Result<T, ApiError>,
) -> Result<(T, u64), ApiError> {
    if let Some((at, value)) = checks.lock_recovering("execution preset checks").get(&key) {
        let age = branchyard_support::time::now_ms().saturating_sub(*at);
        if age < CHECK_TTL_MS {
            return Ok((value.clone(), age));
        }
    }
    let value = check()?;
    checks
        .lock_recovering("execution preset checks")
        .insert(key, (branchyard_support::time::now_ms(), value.clone()));
    Ok((value, 0))
}

/// A harness profile on this server: the ID a request names it by (a
/// default profile's harness name, else its profile ID), and whether its
/// executable is found.
#[derive(Clone, Debug)]
struct Installed {
    id: String,
    profile: String,
    available: bool,
}

/// The PATH check of `GET /v1/harnesses`: a configured command decides for
/// its harness. Cached, with the age of the check.
fn installed(app: &App) -> Result<(Vec<Installed>, u64), ApiError> {
    let key = format!("{:?}", app.config.harness_commands);
    cached(&INSTALLED, key, || {
        let mut found = Vec::new();
        for info in branchyard::harnesses() {
            let id = match info.default {
                true => info.harness.clone(),
                false => info.profile.clone(),
            };
            let available = match app.command(None, &[Some(&id)])? {
                Some(command) => branchyard::check_harness_command(&id, &command).is_ok(),
                None => info.available,
            };
            found.push(Installed {
                id,
                profile: info.profile,
                available,
            });
        }
        Ok(found)
    })
}

/// Where a preset's sessions run, as readiness and projection need it.
enum Place {
    Local,
    Microsandbox,
    Substrate(Box<branchyard::SubstrateOptions>),
}

impl Place {
    fn of(provider: &branchyard::Provider) -> Place {
        match substrate_options(provider) {
            Some(options) => Place::Substrate(Box::new(options)),
            None if provider.label() == "microsandbox" => Place::Microsandbox,
            None => Place::Local,
        }
    }

    /// Whether the environment's settings are usable, as this server sees
    /// them. Reads the bridge key and PEM files of a Substrate environment
    /// and opens no socket.
    fn environment(&self) -> Result<Readiness, ApiError> {
        Ok(match self {
            Place::Local => Readiness::fixed(Reason::LocalProcesses),
            Place::Microsandbox => Readiness::fixed(match branchyard::MICROSANDBOX_BUILT {
                true => Reason::MicrosandboxBuilt,
                false => Reason::MicrosandboxMissing,
            }),
            Place::Substrate(options) => {
                let key = serde_json::to_string(options)
                    .map_err(|error| ApiError::internal(error.to_string()))?;
                let (readable, age) =
                    cached(&READABLE, key, || Ok(options.connection_config().is_ok()))?;
                let reason = match readable {
                    true => Reason::SubstrateReadable,
                    false => Reason::SubstrateUnreadable,
                };
                Readiness::checked(reason, age)
            }
        })
    }

    fn reachability(&self) -> Option<Readiness> {
        match self {
            Place::Substrate(_) => Some(Readiness::fixed(Reason::ContactedAtStart)),
            Place::Local | Place::Microsandbox => None,
        }
    }

    fn executable(&self, installed: &(Vec<Installed>, u64), harness: &str) -> Readiness {
        match self {
            Place::Local => {
                let (found, age) = installed;
                let available = found
                    .iter()
                    .any(|i| (i.id == harness || i.profile == harness) && i.available);
                let reason = match available {
                    true => Reason::ExecutableFound,
                    false => Reason::ExecutableMissing,
                };
                Readiness::checked(reason, *age)
            }
            Place::Microsandbox => Readiness::fixed(Reason::ImageProvidesHarness),
            Place::Substrate(_) => Readiness::fixed(Reason::TemplateProvidesHarness),
        }
    }
}

/// What a harness's profile can do in a console session, whose home is its
/// own.
struct Abilities {
    tool_approvals: bool,
    effort: bool,
}

fn abilities(harness: &str) -> Abilities {
    let profile = branchyard_harness::profiles::default_for(harness)
        .or_else(|| branchyard_harness::profiles::by_id(harness));
    Abilities {
        tool_approvals: branchyard::harness_profile(harness).is_ok_and(|p| p.tool_approvals),
        effort: profile
            .is_some_and(|p| branchyard_provision::takes_effort(p.harness, p.protocol, true)),
    }
}

/// Whether `preset` supplies a secret `harness` reads as its key.
fn credentials(preset: &ExecutionPreset, harness: &str) -> PresetCredentials {
    let keys = branchyard::inventory::harness_of(harness)
        .map(branchyard::inventory::key_variables)
        .unwrap_or_default();
    match preset.secrets.iter().any(|secret| keys.contains(secret)) {
        true => PresetCredentials::Configured,
        false => PresetCredentials::None,
    }
}

/// The presets offered to `tenant`: the configured ones that list it, or,
/// only when none are configured at all, the implicit `local`, whose
/// harnesses are the ones this server finds.
fn offered(app: &App, tenant: &str) -> Result<Vec<(ExecutionPreset, PresetOrigin)>, ApiError> {
    if app.config.execution_presets.is_empty() {
        let (installed, _) = installed(app)?;
        let harnesses = installed
            .iter()
            .filter(|i| i.available)
            .map(|i| json!({ "harness": i.id }))
            .collect::<Vec<_>>();
        let local = serde_json::from_value(json!({
            "id": "local",
            "label": "This server",
            "description": "Runs on the Branchyard server with the harnesses installed here.",
            "provider": { "kind": "local" },
            "harnesses": harnesses,
        }))
        .map_err(|error| ApiError::internal(error.to_string()))?;
        return Ok(vec![(local, PresetOrigin::Default)]);
    }
    Ok(app
        .config
        .execution_presets
        .iter()
        .filter(|p| {
            p.tenants
                .as_ref()
                .is_none_or(|tenants| tenants.iter().any(|t| t == tenant))
        })
        .map(|p| (p.clone(), PresetOrigin::Operator))
        .collect())
}

/// `GET /v1/execution-presets`, as
/// `branchyard_client::workspace_api::ExecutionPresetList` reads it.
#[derive(Serialize)]
pub(super) struct PresetList {
    required: bool,
    presets: Vec<PresetView>,
}

#[derive(Serialize)]
struct PresetView {
    id: String,
    label: String,
    description: String,
    provider: String,
    origin: PresetOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    can_start: Option<bool>,
    harnesses: Vec<HarnessView>,
    environment: Readiness,
    #[serde(skip_serializing_if = "Option::is_none")]
    reachability: Option<Readiness>,
}

#[derive(Serialize)]
struct HarnessView {
    harness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_model: Option<String>,
    tool_approvals: bool,
    writer: bool,
    reviewer: bool,
    effort: bool,
    credentials: PresetCredentials,
    executable: Readiness,
    auth: Readiness,
}

/// What the caller's workspace role lets it start.
#[derive(Clone, Copy)]
struct Roles {
    run: bool,
    comment: bool,
}

impl Roles {
    /// Refuses a caller who is not a member of `workspace`.
    fn of(app: &App, caller: &Caller, workspace: &str) -> Result<Roles, ApiError> {
        auth::authorize_workspace(app, caller, workspace, "read")?;
        let permits = |scope: &str| match auth::authorize_workspace(app, caller, workspace, scope) {
            Ok(_) => Ok(true),
            Err(error) if error.status.is_server_error() => Err(error),
            Err(_) => Ok(false),
        };
        Ok(Roles {
            run: permits("run")?,
            comment: permits("comment")?,
        })
    }
}

/// The presets offered to the caller's tenant; with `workspace`, the caller
/// must be a member and each preset says whether its role can start a
/// session there. Reads configuration, the PATH check and cached offline
/// checks: it never opens a provider, contacts an environment, runs a
/// harness or calls a model. Blocks.
pub(super) fn discover(
    app: &App,
    caller: &Caller,
    workspace: Option<&str>,
) -> Result<PresetList, ApiError> {
    super::scope(caller, "read")?;
    let roles = workspace.map(|id| Roles::of(app, caller, id)).transpose()?;
    let installed = installed(app)?;
    let mut presets = Vec::new();
    for (preset, origin) in offered(app, caller.tenant())? {
        let place = Place::of(&preset.provider);
        let harnesses = preset
            .harnesses
            .iter()
            .map(|harness| {
                let abilities = abilities(&harness.harness);
                let credentials = credentials(&preset, &harness.harness);
                HarnessView {
                    harness: harness.harness.clone(),
                    models: harness.models.clone(),
                    default_model: harness.default_model().map(str::to_owned),
                    tool_approvals: abilities.tool_approvals,
                    writer: abilities.tool_approvals || app.config.allow_unapproved_tools,
                    reviewer: abilities.tool_approvals,
                    effort: abilities.effort,
                    credentials,
                    executable: place.executable(&installed, &harness.harness),
                    auth: Readiness::fixed(match credentials {
                        PresetCredentials::Configured => Reason::KeyConfigured,
                        PresetCredentials::None => Reason::NoKey,
                    }),
                }
            })
            .collect::<Vec<_>>();
        let can_start = roles.map(|roles| {
            harnesses
                .iter()
                .any(|h| (roles.run && h.writer) || (roles.comment && h.reviewer))
        });
        presets.push(PresetView {
            provider: preset.provider.label(),
            environment: place.environment()?,
            reachability: place.reachability(),
            id: preset.id,
            label: preset.label,
            description: preset.description,
            origin,
            can_start,
            harnesses,
        });
    }
    Ok(PresetList {
        required: app.config.require_workspace_execution_presets,
        presets,
    })
}

/// What a session start's `preset` resolved to. The session row keeps it
/// (`_preset`), so every continuation and every error view uses what the
/// session started with, whatever the configuration says later.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct ResolvedPreset {
    /// The preset's ID, shown as the session's `preset`.
    pub(super) id: String,
    /// The provider every turn runs on.
    pub(super) provider: branchyard::Provider,
    /// Secret names added to the start's provisioning.
    pub(super) secrets: Vec<String>,
    /// Worker labels every turn requires.
    pub(super) require_labels: Vec<String>,
    /// The model the start runs: the request's, or the harness's default.
    pub(super) model: Option<String>,
    /// Configured values the session's errors must not show, longest first.
    pub(super) hidden: Vec<Hidden>,
}

/// The preset a session start selects, after workspace authorization and
/// the replay check: `None` for a start without `preset` where none is
/// required. Refuses `preset_required`, `unknown_preset`, `preset_conflict`
/// and `preset_choice_unavailable`; readiness refuses nothing.
pub(super) fn resolve(
    app: &App,
    caller: &Caller,
    body: &Value,
) -> Result<Option<ResolvedPreset>, ApiError> {
    let request: StartSessionRequest = serde_json::from_value(body.clone())
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let Some(id) = request.preset.as_deref() else {
        if app.config.require_workspace_execution_presets {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "preset_required",
                "this server's workspace sessions must name an execution preset; list them with \
                 GET /v1/execution-presets",
            ));
        }
        return Ok(None);
    };
    let preset = offered(app, caller.tenant())?
        .into_iter()
        .map(|(preset, _)| preset)
        .find(|preset| preset.id == id)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown_preset",
                format!(
                    "no execution preset {id} is offered to this tenant; list them with GET \
                     /v1/execution-presets"
                ),
            )
            .detail(json!({ "preset": id }))
        })?;
    let model = choose(&preset, &request)?;
    let hidden = hidden(&preset, &app.config.secrets);
    Ok(Some(ResolvedPreset {
        id: preset.id,
        provider: preset.provider,
        secrets: preset.secrets,
        require_labels: preset.require_labels,
        model,
        hidden,
    }))
}

/// What `request` may choose within `preset`, enforced whether or not
/// presets are required, and the model the start runs.
fn choose(
    preset: &ExecutionPreset,
    request: &StartSessionRequest,
) -> Result<Option<String>, ApiError> {
    if request.provider.is_some() || request.provision.is_some() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "preset_conflict",
            "a session started with an execution preset may not carry provider or provision: \
             the preset supplies them; choose model and effort with the top-level fields",
        ));
    }
    let unavailable = |field: &str, value: &str, allowed: Vec<String>| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "preset_choice_unavailable",
            format!(
                "execution preset {} does not offer {field} {value}; detail.allowed lists what it \
                 offers",
                preset.id
            ),
        )
        .detail(json!({ "field": field, "allowed": allowed }))
    };
    let harness = preset
        .harnesses
        .iter()
        .find(|h| h.harness == request.harness)
        .ok_or_else(|| {
            let allowed = preset.harnesses.iter().map(|h| h.harness.clone()).collect();
            unavailable("harness", &request.harness, allowed)
        })?;
    let model = match (&request.model, &harness.models) {
        (Some(model), Some(models)) if models.contains(model) => Some(model.clone()),
        (Some(model), models) => {
            return Err(unavailable(
                "model",
                model,
                models.clone().unwrap_or_default(),
            ))
        }
        (None, _) => harness.default_model().map(str::to_owned),
    };
    if let Some(effort) = &request.effort {
        if !abilities(&harness.harness).effort {
            return Err(unavailable("effort", effort, Vec::new()));
        }
    }
    Ok(model)
}

/// A configured value a preset session's errors must not show, and the
/// fixed placeholder that replaces it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Hidden {
    value: String,
    placeholder: String,
}

/// What a hidden value is, as its placeholder names it.
#[derive(Clone, Copy)]
enum Configured {
    BridgeKey,
    ApiToken,
    CertificateAuthority,
    ClientCertificate,
    ClientKey,
    RouterCertificateAuthority,
    ControlEndpoint,
    Router,
    ServiceName,
    PassedVariable,
    Secret,
    SecretSource,
}

impl Configured {
    fn text(self) -> &'static str {
        match self {
            Configured::BridgeKey => "bridge key",
            Configured::ApiToken => "API token file",
            Configured::CertificateAuthority => "certificate authority",
            Configured::ClientCertificate => "client certificate",
            Configured::ClientKey => "client key",
            Configured::RouterCertificateAuthority => "router certificate authority",
            Configured::ControlEndpoint => "control endpoint",
            Configured::Router => "router",
            Configured::ServiceName => "TLS service name",
            Configured::PassedVariable => "passed variable",
            Configured::Secret => "secret",
            Configured::SecretSource => "secret source",
        }
    }
}

/// `hidden` longest value first, so a path is replaced before a shorter
/// value inside it, each value once and none empty.
fn longest_first(hidden: &mut Vec<Hidden>) {
    hidden.retain(|h| !h.value.is_empty());
    hidden.sort_by(|a, b| {
        b.value
            .len()
            .cmp(&a.value.len())
            .then_with(|| a.value.cmp(&b.value))
    });
    hidden.dedup_by(|a, b| a.value == b.value);
}

/// Every configured value of `preset` an error could name: its provider's
/// paths (as written and as resolved), endpoints and variable names, and
/// its secrets' names and sources, in [`longest_first`] order.
fn hidden(
    preset: &ExecutionPreset,
    secrets: &BTreeMap<String, branchyard::SecretSource>,
) -> Vec<Hidden> {
    let mut found: Vec<(String, Configured)> = Vec::new();
    for provider in std::iter::once(&preset.provider).chain(preset.written.as_ref()) {
        if let Some(options) = substrate_options(provider) {
            found.push((options.key.display().to_string(), Configured::BridgeKey));
            for (path, what) in [
                (options.api_token_file, Configured::ApiToken),
                (options.ca, Configured::CertificateAuthority),
                (options.client_cert, Configured::ClientCertificate),
                (options.client_key, Configured::ClientKey),
                (options.router_ca, Configured::RouterCertificateAuthority),
            ] {
                found.extend(path.map(|path| (path.display().to_string(), what)));
            }
            for (url, what) in [
                (&options.endpoint, Configured::ControlEndpoint),
                (&options.router, Configured::Router),
            ] {
                found.extend(endpoint_forms(url).into_iter().map(|form| (form, what)));
            }
            found.extend(
                options
                    .server_name
                    .map(|name| (name, Configured::ServiceName)),
            );
        }
        if let Ok(Value::Object(fields)) = serde_json::to_value(provider) {
            let passed = fields.get("pass_env").and_then(Value::as_array);
            for name in passed.into_iter().flatten() {
                found.extend(
                    name.as_str()
                        .map(|name| (name.to_owned(), Configured::PassedVariable)),
                );
            }
        }
    }
    for name in &preset.secrets {
        found.push((name.clone(), Configured::Secret));
        match secrets.get(name).and_then(|secret| secret.from.as_ref()) {
            Some(branchyard::SecretFrom::Env { var }) => {
                found.push((var.clone(), Configured::SecretSource));
            }
            Some(branchyard::SecretFrom::File { path }) => {
                found.push((path.display().to_string(), Configured::SecretSource));
            }
            None => {}
        }
    }
    let mut hidden = found
        .into_iter()
        .map(|(value, what)| Hidden {
            value,
            placeholder: format!("[preset {}: {}]", preset.id, what.text()),
        })
        .collect();
    longest_first(&mut hidden);
    hidden
}

/// The forms an endpoint takes in error text: the URL as configured, its
/// origin, and its host with and without the port, as a refused connection
/// names it (`127.0.0.1:1`) or a failed lookup does.
fn endpoint_forms(url: &str) -> Vec<String> {
    let mut forms = vec![url.to_owned()];
    if let Ok(parsed) = reqwest::Url::parse(url) {
        forms.push(parsed.origin().ascii_serialization());
        if let Some(host) = parsed.host_str() {
            forms.extend(
                parsed
                    .port_or_known_default()
                    .map(|port| format!("{host}:{port}")),
            );
            forms.push(host.to_owned());
        }
    }
    forms
}

/// A character that continues a name, so a value next to one is part of a
/// longer word, not the configured value: `ate` in `create`, `127.0.0.1:1`
/// in `127.0.0.1:10`.
fn continues_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// `text` with each of a preset session's configured values replaced by its
/// placeholder where it stands as a whole word, in one pass, so a
/// placeholder is never rewritten.
pub(super) fn project_error(text: &str, hidden: &[Hidden]) -> String {
    let mut projected = String::with_capacity(text.len());
    let mut before = None;
    let mut rest = text;
    'scan: while let Some(next) = rest.chars().next() {
        if !before.is_some_and(continues_word) {
            for value in hidden.iter().filter(|h| !h.value.is_empty()) {
                if let Some(after) = rest.strip_prefix(value.value.as_str()) {
                    if !after.chars().next().is_some_and(continues_word) {
                        projected.push_str(&value.placeholder);
                        before = value.value.chars().next_back();
                        rest = after;
                        continue 'scan;
                    }
                }
            }
        }
        projected.push(next);
        before = Some(next);
        rest = &rest[next.len_utf8()..];
    }
    projected
}

/// Every string in `value`, object keys included, through [`project_error`].
pub(super) fn project_value(value: &mut Value, hidden: &[Hidden]) {
    match value {
        Value::String(text) => *text = project_error(text, hidden),
        Value::Array(values) => {
            for value in values {
                project_value(value, hidden);
            }
        }
        Value::Object(fields) => {
            for (key, mut value) in std::mem::take(fields) {
                project_value(&mut value, hidden);
                fields.insert(project_error(&key, hidden), value);
            }
        }
        _ => {}
    }
}

/// The preset a session row recorded at its start, if it started with one.
pub(super) fn stored(session: &Record) -> Result<Option<ResolvedPreset>, ApiError> {
    session
        .body
        .get("_preset")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| ApiError::internal(format!("stored session preset: {error}")))
}

/// The configured values a resource's text must not show: those of the
/// preset session it belongs to; empty for anything else.
#[derive(Clone, Debug, Default)]
pub(crate) struct Projection(Vec<Hidden>);

impl Projection {
    fn of_row(session: &Record) -> Result<Projection, ApiError> {
        Ok(Projection(
            stored(session)?
                .map(|preset| preset.hidden)
                .unwrap_or_default(),
        ))
    }

    /// The projection of session `session_id`, when its row belongs to
    /// workspace `workspace_id`.
    pub(crate) fn of_session(
        app: &App,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Projection, ApiError> {
        match app.registry.console_get(Collection::Sessions, session_id)? {
            Some(session)
                if session.body.get("workspace_id").and_then(Value::as_str)
                    == Some(workspace_id) =>
            {
                Projection::of_row(&session)
            }
            _ => Ok(Projection::default()),
        }
    }

    /// The projection of the session `branch` of `repo` belongs to: the
    /// nearest of the branch and its ancestors that is a session's own
    /// branch in a workspace on `repo`. A session's preset and a branch's
    /// parent never change, so neither does the answer.
    pub(crate) fn of_branch(app: &App, repo: &str, branch: &str) -> Result<Projection, ApiError> {
        let mut seen = BTreeSet::new();
        let mut current = Some(branch.to_owned());
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                break;
            }
            if let Some(session) = session_on(app, repo, &name)? {
                return Projection::of_row(&session);
            }
            current = app
                .repos
                .get(repo)
                .and_then(|state| state.yard.branch(&name).ok())
                .and_then(|branch| branch.info().parent.clone());
        }
        Ok(Projection::default())
    }

    /// The projections of every branch `operation` names, merged.
    pub(crate) fn of_operation(app: &App, operation: &Operation) -> Result<Projection, ApiError> {
        let mut hidden = Vec::new();
        for branch in &operation.branches {
            hidden.extend(Projection::of_branch(app, &operation.repo, branch)?.0);
        }
        longest_first(&mut hidden);
        Ok(Projection(hidden))
    }

    /// Whether this projection hides nothing.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every string in `value`, object keys included, through this
    /// projection.
    pub(crate) fn apply(&self, value: &mut Value) {
        if !self.is_empty() {
            project_value(value, &self.0);
        }
    }

    /// `body` as the response a caller reads, with the value at each JSON
    /// pointer through the projection beside it. When every projection is
    /// empty the body is serialized as it always was, so what a resource
    /// outside any preset session shows does not change.
    pub(crate) fn respond_at(
        body: impl Serialize,
        at: &[(String, Projection)],
    ) -> Result<Response, ApiError> {
        if at.iter().all(|(_, projection)| projection.is_empty()) {
            return Ok(Json(body).into_response());
        }
        let mut value =
            serde_json::to_value(body).map_err(|error| ApiError::internal(error.to_string()))?;
        for (pointer, projection) in at {
            if let Some(found) = value.pointer_mut(pointer) {
                projection.apply(found);
            }
        }
        Ok(Json(value).into_response())
    }
}

/// The session whose own branch is `name`, in a workspace on `repo`.
fn session_on(app: &App, repo: &str, name: &str) -> Result<Option<Record>, ApiError> {
    let Some(session) = app.registry.console_get(Collection::Sessions, name)? else {
        return Ok(None);
    };
    let field = |key: &str| session.body.get(key).and_then(Value::as_str);
    if field("branch") != Some(name) {
        return Ok(None);
    }
    let Some(workspace_id) = field("workspace_id") else {
        return Ok(None);
    };
    let on_repo = app
        .registry
        .console_get(Collection::Workspaces, workspace_id)?
        .is_some_and(|workspace| workspace.body.get("repo").and_then(Value::as_str) == Some(repo));
    Ok(on_repo.then_some(session))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(value: Value) -> ExecutionPreset {
        serde_json::from_value(value).unwrap()
    }

    fn request(value: Value) -> StartSessionRequest {
        let mut body = json!({"name":"Analyst","prompt":"Summarise","access":"writer"});
        body.as_object_mut()
            .unwrap()
            .extend(value.as_object().unwrap().clone());
        serde_json::from_value(body).unwrap()
    }

    fn refused(preset: &ExecutionPreset, body: Value) -> (StatusCode, String, Value) {
        let error = choose(preset, &request(body)).unwrap_err();
        (
            error.status,
            error.body.code.clone(),
            error.body.detail.clone().unwrap_or(Value::Null),
        )
    }

    #[test]
    fn a_request_chooses_only_what_its_preset_offers() {
        let gke = preset(
            json!({"id":"gke","label":"GKE","provider":{"kind":"local"},"harnesses":[
            {"harness":"antigravity","models":["flash","pro"],"default_model":"pro"},
            {"harness":"codex","models":["gpt-small"]},
            {"harness":"claude-code"}]}),
        );
        let chosen = |body: Value| choose(&gke, &request(body)).unwrap();
        assert_eq!(
            chosen(json!({"harness":"antigravity","model":"flash"})),
            Some("flash".into())
        );
        assert_eq!(chosen(json!({"harness":"antigravity"})), Some("pro".into()));
        assert_eq!(chosen(json!({"harness":"codex"})), Some("gpt-small".into()));
        assert_eq!(chosen(json!({"harness":"claude-code"})), None);
        assert_eq!(
            refused(&gke, json!({"harness":"claude-code-acp"})),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "preset_choice_unavailable".into(),
                json!({"field":"harness","allowed":["antigravity","codex","claude-code"]})
            )
        );
        assert_eq!(
            refused(&gke, json!({"harness":"antigravity","model":"ultra"})).2,
            json!({"field":"model","allowed":["flash","pro"]})
        );
        assert_eq!(
            refused(&gke, json!({"harness":"claude-code","model":"opus"})).2,
            json!({"field":"model","allowed":[]})
        );
        assert_eq!(
            refused(&gke, json!({"harness":"antigravity","effort":"high"})).2,
            json!({"field":"effort","allowed":[]})
        );
        assert!(
            abilities("codex").effort,
            "codex takes an effort in its own home"
        );
        assert_eq!(
            chosen(json!({"harness":"codex","effort":"high"})),
            Some("gpt-small".into())
        );
        for conflict in [
            json!({"harness":"antigravity","provider":{"kind":"local"}}),
            json!({"harness":"antigravity","provision":{"model":"flash"}}),
            json!({"harness":"antigravity","provision":{}}),
        ] {
            assert_eq!(
                refused(&gke, conflict).0,
                StatusCode::BAD_REQUEST,
                "provider and provision conflict with a preset"
            );
        }
    }

    #[test]
    fn projection_hides_written_and_resolved_paths_endpoints_and_secret_names() {
        let mut gke = preset(json!({"id":"substrate-gke","label":"GKE",
            "provider":{"kind":"substrate","endpoint":"https://control.internal.example",
                "router":"https://router.internal.example/{atespace}/{actor}/","template":"harnesses",
                "key":"/srv/conf/substrate/bridge.key","ca":"/srv/conf/substrate/ca.pem",
                "pass_env":["SUBSTRATE_EXTRA"]},
            "harnesses":[{"harness":"antigravity"}],"secrets":["GEMINI_API_KEY"]}));
        let mut written = substrate_options(&gke.provider).unwrap();
        written.key = "substrate/bridge.key".into();
        gke.written = Some(branchyard::Provider::substrate(written));
        let secrets = BTreeMap::from([(
            "GEMINI_API_KEY".to_owned(),
            crate::config::parse_secret("GEMINI_API_KEY=@/etc/branchyard/gemini.key").unwrap(),
        )]);
        let hidden = hidden(&gke, &secrets);
        let text = "cannot use the Substrate bridge key /srv/conf/substrate/bridge.key \
            (written substrate/bridge.key) for https://control.internal.example with \
            /srv/conf/substrate/ca.pem; GEMINI_API_KEY from /etc/branchyard/gemini.key; \
            SUBSTRATE_EXTRA unset";
        assert_eq!(
            project_error(text, &hidden),
            "cannot use the Substrate bridge key [preset substrate-gke: bridge key] \
            (written [preset substrate-gke: bridge key]) for [preset substrate-gke: control endpoint] with \
            [preset substrate-gke: certificate authority]; [preset substrate-gke: secret] from \
            [preset substrate-gke: secret source]; [preset substrate-gke: passed variable] unset"
        );
        let mut value = json!({"error":{"message":text,"detail":{"/srv/conf/substrate/ca.pem":["GEMINI_API_KEY"]}},"n":1});
        project_value(&mut value, &hidden);
        let rendered = value.to_string();
        for secret in [
            "/srv/conf",
            "substrate/bridge.key",
            "GEMINI_API_KEY",
            "control.internal",
        ] {
            assert!(!rendered.contains(secret), "{secret} in {rendered}");
        }
        assert_eq!(value["n"], 1);
        assert_eq!(
            project_error("é—no values here", &hidden),
            "é—no values here"
        );
        assert_eq!(
            project_error(
                "tcp connect error, control.internal.example:443 refused; MY_GEMINI_API_KEY_2 \
                 unrelated; control.internal.example.",
                &hidden
            ),
            "tcp connect error, [preset substrate-gke: control endpoint] refused; \
             MY_GEMINI_API_KEY_2 unrelated; [preset substrate-gke: control endpoint]."
        );
        let short = preset(json!({"id":"ate","label":"Ate",
            "provider":{"kind":"substrate","endpoint":"http://ate:8080","router":"http://ate:8081/",
                "template":"t","key":"/k"},"harnesses":[{"harness":"antigravity"}]}));
        assert_eq!(
            project_error(
                "could not create the actor at ate:8080 (state: ate down) via /k",
                &super::hidden(&short, &BTreeMap::new())
            ),
            "could not create the actor at [preset ate: control endpoint] (state: [preset ate: \
             control endpoint] down) via [preset ate: bridge key]"
        );
        let looping = [Hidden {
            value: String::new(),
            placeholder: "[x]".into(),
        }];
        assert_eq!(project_error("text", &looping), "text");
    }

    #[test]
    fn readiness_text_is_fixed_and_says_where_it_was_measured() {
        let substrate =
            serde_json::to_value(Readiness::checked(Reason::SubstrateUnreadable, 12)).unwrap();
        assert_eq!(
            substrate,
            json!({"state":"not_ready","scope":"api_host",
                "reason":"This server cannot read the Substrate settings for this environment.",
                "action":"An operator should check this environment in the server configuration and run `by serve --check`.",
                "checked_ms_ago":12})
        );
        let auth = serde_json::to_value(Readiness::fixed(Reason::KeyConfigured)).unwrap();
        assert_eq!(auth["state"], "unknown");
        assert_eq!(auth["scope"], "environment");
        assert_eq!(auth["action"], Value::Null);
        assert!(auth.get("checked_ms_ago").is_none());
    }
}
