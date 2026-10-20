//! A yard configured from `branchyard.toml`: [`Yard::open_configured`],
//! [`Yard::configuration`], [`Yard::configure`] and the defaults a new task
//! takes from the files. The files, their types and their layering are
//! [`crate::config`] (`docs/setup.md#configuration`); this module turns the
//! merged configuration into the engine's settings, one path for `by`, a
//! server and an SDK caller:
//!
//! - `[connectors]`: the connector gateway the yard's turns are given (the
//!   URL `gateway` pins, else the live gateway registered for this yard in
//!   the repository's service registry) and where its sandboxes reach this
//!   host.
//! - `[approvals]`: the person's policy for connector operations.
//! - `[models]` and `[secrets]`: the model gateway, signing with the
//!   connector gateway's keys when the yard has one.
//! - `[knowledge]`: what branches are given and when they are distilled.
//! - `[defaults]`, `[microsandbox]`, `[mcp]`, `[network]`, the grants of
//!   `[connectors]` and the access of `[models]`: what [`Yard::task`]
//!   starts from ([`task_options`]) and what a run fills in where the
//!   caller set nothing ([`fill_provisioning`]).
//!
//! Nothing here reads a variable. A caller that must not read the files,
//! such as `by` inside a harness's branch ([`crate::ENV_BRANCH`] set), opens
//! the yard with [`Yard::open`] instead and gets no defaults.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{
    self, Connectors, DistillerConfig, Effective, KnowledgeConfig, Microsandbox, ModelsConfig,
    PermissionsMode, ProjectConfig, ProviderKind,
};
use crate::connectors::{self, AnvilPackager};
use crate::effects::ApprovalSettings;
use crate::models::{self, KeySource, Signer};
use crate::services::{Query, Service, KIND_CONNECTOR_GATEWAY};
use crate::{
    projection, Budget, DistillTrigger, Effort, Error, HarnessJudge, Judge, JudgeSpec,
    KnowledgeSettings, McpServerSpec, Policy, PolicyPreset, Provider, Provisioning, SandboxHost,
    SandboxKeep, SandboxOptions, SecretSource, TaskOptions, Yard,
};

/// The hint under a configuration file's error.
const HINT: &str = "(fix it, or check it with `by config validate`)";

/// The command for Anvil when `[connectors] anvil` names none.
pub const ANVIL: &str = "anvil";

fn section(name: &str, what: impl Display) -> Error {
    Error::Config(format!("{name}: {what}"))
}

/// The configuration files that apply to the repository at `root`,
/// layered (the project's `branchyard.toml` over the user file): `None`
/// when neither exists. A file that does not load is an error naming it
/// and its line, with the hint to check it.
pub fn configuration(root: &Path) -> Result<Option<Effective>, Error> {
    if !config::locate(root).any() {
        return Ok(None);
    }
    config::load(root, None)
        .map(Some)
        .map_err(|e| Error::Config(format!("{e}\n{HINT}")))
}

/// What [`Yard::configure`] keeps: the configuration, and the options a
/// new task starts from, made once.
pub(crate) struct Configured {
    pub config: Arc<ProjectConfig>,
    pub options: TaskOptions,
}

impl Yard {
    /// [`Yard::open`], then [`Yard::configure`] with the files that apply
    /// to the repository ([`Yard::configuration`]), or with an empty
    /// configuration when there are none, which still finds a connector
    /// gateway registered for the yard. What `by` does for every command
    /// run outside a harness.
    pub fn open_configured(path: impl AsRef<Path>) -> Result<Yard, Error> {
        let yard = Yard::open(path)?;
        let config = yard
            .configuration()?
            .map(|effective| effective.config)
            .unwrap_or_default();
        yard.configure(&config)?;
        Ok(yard)
    }

    /// The configuration files that apply to this repository, layered:
    /// `branchyard.toml` at its root over the user file
    /// (`~/.config/branchyard/config.toml`, or `BRANCHYARD_USER_CONFIG`);
    /// `None` when neither exists. Each value's source is in
    /// [`Effective::sources`]. See `docs/setup.md#configuration`.
    pub fn configuration(&self) -> Result<Option<Effective>, Error> {
        configuration(self.root())
    }

    /// Give this yard what `config` describes: its connector gateway and
    /// sandbox host (`[connectors]`), the person's approvals
    /// (`[approvals]`), its model gateway (`[models]` with `[secrets]`),
    /// its knowledge settings (`[knowledge]`), and the defaults every
    /// [`Yard::task`] then starts from ([`task_options`] now,
    /// [`fill_provisioning`] when it runs). A section that describes
    /// nothing usable is an error naming it, and nothing is changed.
    pub fn configure(&self, config: &ProjectConfig) -> Result<(), Error> {
        apply(self, config)
    }

    /// The configuration [`Yard::configure`] gave this yard, if any.
    pub fn configured(&self) -> Option<Arc<ProjectConfig>> {
        projection::lock(&self.hub.config)
            .as_ref()
            .map(|c| c.config.clone())
    }

    /// The configuration and the options a new task starts from.
    pub(crate) fn task_defaults(&self) -> Option<(Arc<ProjectConfig>, TaskOptions)> {
        projection::lock(&self.hub.config)
            .as_ref()
            .map(|c| (c.config.clone(), c.options.clone()))
    }
}

/// [`Yard::configure`]: everything checked first, then given to the yard.
fn apply(yard: &Yard, config: &ProjectConfig) -> Result<(), Error> {
    let host = sandbox_host(&config.connectors)?;
    let gateway = locate_gateway(yard, &config.connectors)?
        .map(|at| connector_gateway(yard, &config.connectors, &at))
        .transpose()?;
    let person = config
        .approvals
        .policy()
        .map_err(|e| section("[approvals]", e))?;
    // The model gateway signs with the connector gateway's keys: the one
    // found here, else the one the yard already has.
    let existing = yard.connectors();
    let signer = signer(yard, gateway.as_ref().or(existing.as_deref()))?;
    let models = model_gateway_signed(yard, &config.models, &config.secrets, signer)?;
    let knowledge = match config.knowledge.is_empty() {
        true => None,
        false => Some(knowledge_settings(&config.knowledge)?),
    };
    let options = task_options(config)?;
    if let Some(host) = host {
        yard.use_sandbox_host(host);
    }
    if let Some(gateway) = gateway {
        yard.use_connectors(gateway);
    }
    if person.is_some() {
        yard.use_approvals(ApprovalSettings {
            person,
            ..ApprovalSettings::default()
        });
    }
    if let Some(models) = models {
        yard.use_models(models);
    }
    if let Some(knowledge) = knowledge {
        yard.use_knowledge(knowledge);
    }
    yard.use_max_running(config.usage.max_running.clone());
    *projection::lock(&yard.hub.config) = Some(Arc::new(Configured {
        config: Arc::new(config.clone()),
        options,
    }));
    Ok(())
}

// `[connectors]`

/// Where a yard's connector gateway is, as [`locate_gateway`] finds it.
#[derive(Clone, Debug)]
pub enum GatewayLocation {
    /// `[connectors] gateway` names its `/mcp` URL.
    Pinned(String),
    /// The live gateway registered for this yard (its issuer) in the
    /// repository's service registry; see `docs/registry.md`.
    Registered(Box<Service>),
}

impl GatewayLocation {
    /// The gateway's `/mcp` URL.
    pub fn url(&self) -> &str {
        match self {
            GatewayLocation::Pinned(url) => url,
            GatewayLocation::Registered(service) => service.url().unwrap_or_default(),
        }
    }

    /// `pinned` or `registry`, for messages.
    pub fn source(&self) -> &'static str {
        match self {
            GatewayLocation::Pinned(_) => "pinned",
            GatewayLocation::Registered(_) => "registry",
        }
    }
}

/// Where the yard's connector gateway is: the URL `config` pins, else the
/// live gateway registered for this yard in the repository's service
/// registry, else `None`.
pub fn locate_gateway(yard: &Yard, config: &Connectors) -> Result<Option<GatewayLocation>, Error> {
    if let Some(url) = &config.gateway {
        return Ok(Some(GatewayLocation::Pinned(url.clone())));
    }
    if !yard.has_services() {
        return Ok(None);
    }
    let issuer = connectors::local_issuer(yard.root())?;
    let query = Query::kind(KIND_CONNECTOR_GATEWAY).require("issuer", issuer);
    Ok(yard
        .resolve_service(&query)?
        .filter(|s| s.url().is_some())
        .map(|s| GatewayLocation::Registered(Box::new(s))))
}

/// Anvil's command, `[connectors] anvil` split into words, else [`ANVIL`].
pub fn anvil_command(config: &Connectors) -> Result<Vec<String>, Error> {
    let words = config::split_words(config.anvil.as_deref().unwrap_or(ANVIL))
        .map_err(|e| section("connectors.anvil", e))?;
    match words.is_empty() {
        true => Err(Error::Config("connectors.anvil names no command".into())),
        false => Ok(words),
    }
}

/// The bundle root, `[connectors] bundles`, else `connectors/` under
/// `root`.
pub fn bundles(root: &Path, config: &Connectors) -> PathBuf {
    config
        .bundles
        .as_ref()
        .map_or_else(|| root.join("connectors"), PathBuf::from)
}

/// The yard's connector gateway at `at`, as `config` describes it: Anvil's
/// packager over the bundle root, each turn's effect-ledger proxy
/// (`docs/effects.md`) and the URL its sandboxes dial.
pub fn connector_gateway(
    yard: &Yard,
    config: &Connectors,
    at: &GatewayLocation,
) -> Result<connectors::Gateway, Error> {
    let packager = AnvilPackager {
        command: anvil_command(config)?,
        root: bundles(yard.root(), config),
    };
    let mut gateway = connectors::Gateway::local(yard, at.url(), Arc::new(packager))?;
    if let Some(enabled) = config.effects_proxy {
        gateway.effects.enabled = enabled;
    }
    if let Some(listen) = effects_listen(config)? {
        gateway.effects.listen = listen;
    }
    gateway.effects.sandbox_host = config.effects_sandbox_host.clone();
    gateway.sandbox_url = config.sandbox_gateway.clone().or_else(|| match at {
        GatewayLocation::Registered(service) => service.text("sandbox_url").map(str::to_owned),
        GatewayLocation::Pinned(_) => None,
    });
    Ok(gateway)
}

/// `[connectors] effects_listen`, parsed.
pub fn effects_listen(config: &Connectors) -> Result<Option<IpAddr>, Error> {
    config
        .effects_listen
        .as_deref()
        .map(|listen| {
            listen
                .parse()
                .map_err(|_| Error::Config(format!("connectors.effects_listen: {listen:?}")))
        })
        .transpose()
}

/// Where a sandbox reaches this host, from `[connectors]
/// effects_sandbox_host` and `effects_listen`: the effect-ledger proxy's
/// address, which a sandboxed turn's delegation endpoint shares
/// (`docs/delegation.md`), with or without a gateway.
pub fn sandbox_host(config: &Connectors) -> Result<Option<SandboxHost>, Error> {
    let Some(host) = config.effects_sandbox_host.clone() else {
        return Ok(None);
    };
    let listen = effects_listen(config)?.unwrap_or(Ipv4Addr::LOCALHOST.into());
    Ok(Some(SandboxHost { listen, host }))
}

// `[models]` and `[secrets]`

/// Where the secret `name` is: its `[secrets]` entry's variable, or its
/// file (`@path`, relative to `root` or `~/`), else the variable `name`.
pub fn key_source(root: &Path, secrets: &BTreeMap<String, String>, name: &str) -> KeySource {
    match secrets.get(name) {
        Some(source) => match source.strip_prefix('@') {
            Some(path) => KeySource::File(config::resolve(root, path)),
            None => KeySource::Env(source.clone()),
        },
        None => KeySource::Env(name.to_owned()),
    }
}

/// `[models]` as the gateway takes it: the file's table and the engine's
/// agree field for field.
pub fn models_config(config: &ModelsConfig) -> Result<models::Config, Error> {
    serde_json::to_value(config)
        .and_then(serde_json::from_value)
        .map_err(|e| section("models", e))
}

/// The signer of the yard's model-gateway tokens: the yard's local keys,
/// or the connector gateway's when it has one.
fn signer(yard: &Yard, connectors: Option<&connectors::Gateway>) -> Result<Signer, Error> {
    let mut signer = Signer::local(yard)?;
    if let Some(gateway) = connectors {
        signer.issuer = gateway.issuer.clone();
        signer.key_file = gateway.key_file.clone();
        signer.jwks_file = gateway.jwks_file.clone();
    }
    Ok(signer)
}

/// The model gateway `config` describes for `yard`, its backends' keys
/// where `secrets` says ([`key_source`]), signing with the connector
/// gateway's keys when the yard has one; `None` when it names no backend.
/// See `docs/model-gateway.md`.
pub fn model_gateway(
    yard: &Yard,
    config: &ModelsConfig,
    secrets: &BTreeMap<String, String>,
) -> Result<Option<models::Gateway>, Error> {
    let connectors = yard.connectors();
    let signer = signer(yard, connectors.as_deref())?;
    model_gateway_signed(yard, config, secrets, signer)
}

fn model_gateway_signed(
    yard: &Yard,
    config: &ModelsConfig,
    secrets: &BTreeMap<String, String>,
    signer: Signer,
) -> Result<Option<models::Gateway>, Error> {
    if config.backends.is_empty() {
        return Ok(None);
    }
    let engine = models_config(config)?;
    let root = yard.root();
    let gateway = models::Gateway::new(&engine, signer, |name| key_source(root, secrets, name))
        .map_err(|e| section("[models]", e))?;
    Ok(Some(gateway))
}

// `[knowledge]`

/// The distiller `[knowledge] distiller` names, as a judge.
pub fn distiller(config: &DistillerConfig) -> Result<Arc<dyn Judge>, Error> {
    let effort = config
        .effort
        .as_deref()
        .map(Effort::parse)
        .transpose()
        .map_err(|e| section("knowledge.distiller.effort", e))?;
    let command = config
        .command
        .as_deref()
        .map(config::split_words)
        .transpose()
        .map_err(|e| section("knowledge.distiller.command", e))?;
    Ok(Arc::new(HarnessJudge {
        spec: JudgeSpec {
            harness: config.harness.clone(),
            model: config.model.clone(),
            effort,
            command,
            rubric: None,
        },
        options: TaskOptions::default(),
    }))
}

/// The settings `[knowledge]` describes; see `docs/knowledge.md`.
pub fn knowledge_settings(config: &KnowledgeConfig) -> Result<KnowledgeSettings, Error> {
    let defaults = KnowledgeSettings::default();
    let distill_on = match &config.distill_on {
        None => defaults.distill_on,
        Some(names) => names
            .iter()
            .map(|name| match name.as_str() {
                "merged" => Ok(DistillTrigger::Merged),
                "judged_best" => Ok(DistillTrigger::JudgedBest),
                "ready" => Ok(DistillTrigger::Ready),
                other => Err(Error::Config(format!(
                    "knowledge.distill_on: {other:?} is not merged, judged_best or ready"
                ))),
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(KnowledgeSettings {
        provision: config.provision.unwrap_or(defaults.provision),
        budget_tokens: config
            .budget_tokens
            .map_or(defaults.budget_tokens, |t| t as usize),
        distill_on,
        distiller: config.distiller.as_ref().map(distiller).transpose()?,
    })
}

// `[defaults]` and what a new task takes

fn minutes(minutes: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(minutes * 60.0).ok()
}

/// `[microsandbox]` as a provider's options.
pub fn sandbox_options(config: &Microsandbox) -> SandboxOptions {
    SandboxOptions {
        image: config.image.clone(),
        cpus: config.cpus,
        memory_mib: config.memory_mib,
        pass_env: config.pass_env.clone(),
        keep: match config.keep.as_deref() {
            Some("pause") => SandboxKeep::Pause,
            _ => SandboxKeep::Destroy,
        },
        snapshots: config.snapshots,
        max_paused: config.max_paused,
        max_kept_secs: config.max_kept_minutes.map(|m| u64::from(m) * 60),
        live_branch: config.live_branch.unwrap_or(false),
    }
}

/// The options a new task of a yard configured with `config` starts from
/// ([`Yard::task`]), before the caller's own: `[defaults]` (harness,
/// check, budget, isolation, a permission preset) and `[microsandbox]`
/// when `defaults.provider` names it. `permissions = "ask"` is a
/// terminal's and keeps the default policy here; `provider = "recipe"` is
/// `by`'s, since trusting a recipe is a person's decision
/// (`docs/recipes.md`); `[fleet]` routes a `by run`, not a task started
/// here.
pub fn task_options(config: &ProjectConfig) -> Result<TaskOptions, Error> {
    let d = &config.defaults;
    let check = d
        .check
        .as_deref()
        .map(config::split_words)
        .transpose()
        .map_err(|e| section("defaults.check", e))?;
    let provider = match (d.provider, &config.microsandbox) {
        (Some(ProviderKind::Microsandbox), Some(sandbox)) => {
            Some(Provider::microsandbox(sandbox_options(sandbox)))
        }
        _ => None,
    };
    let policy = match d.permissions {
        Some(PermissionsMode::Yes) => Policy::allow_all(),
        Some(PermissionsMode::ReadOnly) => PolicyPreset::ReadOnly.policy(),
        Some(PermissionsMode::EditWorktree) => PolicyPreset::EditWorktree.policy(),
        Some(PermissionsMode::Full) => PolicyPreset::Full.policy(),
        Some(PermissionsMode::Ask) | None => Policy::default(),
    };
    Ok(TaskOptions {
        harness: d.harness.clone(),
        check,
        budget: Budget {
            max_usd: d.budget_usd,
            max_turns: d.max_turns,
            max_duration: d.max_minutes.and_then(minutes),
            ..Budget::default()
        },
        policy,
        isolated: d.isolated == Some(true),
        provider,
        ..TaskOptions::default()
    })
}

/// Whether `options` give a new branch a home of its own, where secrets
/// and connector grants can be placed: isolated, or on a provider that
/// runs the harness in a sandbox.
pub fn private_home(options: &TaskOptions) -> bool {
    options.isolated || crate::providers::of(options.provider.as_ref()).sandboxed()
}

/// Fill what `spec` leaves unset from `config`, for a new branch: the
/// model, effort and authentication of `[defaults]`, the `[network]`
/// policy, the `[mcp]` servers and, with `private_home` (see
/// [`private_home`]), the `[secrets]` and the default grants of
/// `[connectors]`; and the access `[models]` gives, which also keeps a
/// provider's key from a branch on the gateway. Servers and secrets the
/// caller named stay; the rest is added. `spec` stays `None` when there
/// is nothing to add.
pub fn fill_provisioning(
    config: &ProjectConfig,
    spec: &mut Option<Provisioning>,
    private_home: bool,
) -> Result<(), Error> {
    let d = &config.defaults;
    // The model gateway's default access, when the caller gave none.
    let models = config
        .models
        .access()
        .map_err(|e| Error::Config(e.to_string()))?;
    let given_models = spec.as_ref().is_some_and(|p| p.models.is_some());
    let on_gateway = given_models || models.is_some();
    let secrets = match private_home {
        true => config
            .secret_sources()
            .map_err(|e| Error::Config(e.to_string()))?,
        false => Vec::new(),
    };
    // A branch on the model gateway is never given a provider's key: the
    // gateway holds the backends' keys, and a harness with one could reach
    // the provider around it.
    let secrets: Vec<SecretSource> = match on_gateway {
        false => secrets,
        true => secrets
            .into_iter()
            .filter(|s| {
                !models::PROVIDER_KEYS.contains(&s.name.as_str())
                    && !config
                        .models
                        .backends
                        .values()
                        .any(|b| b.key.as_deref() == Some(s.name.as_str()))
            })
            .collect(),
    };
    let mcp: Vec<McpServerSpec> = config
        .mcp
        .iter()
        .map(|(name, command)| McpServerSpec::parse(&format!("{name}={command}")))
        .collect::<Result<_, _>>()
        .map_err(|e| section("mcp", e))?;
    let effort = d
        .effort
        .as_deref()
        .map(Effort::parse)
        .transpose()
        .map_err(|e| section("defaults.effort", e))?;
    // Default grants, only where they can be placed and only when the
    // caller gave none.
    let grants = match private_home {
        true => config
            .connectors
            .grant_entries()
            .map_err(|e| Error::Config(e.to_string()))?,
        false => Vec::new(),
    };
    let given_grants = spec.as_ref().is_some_and(|p| !p.connectors.is_empty());
    // The network policy, when the caller gave none.
    let network = config
        .network
        .policy()
        .map_err(|e| Error::Config(e.to_string()))?;
    let given_network = spec.as_ref().is_some_and(|p| p.network.is_some());
    let wanted = d.model.is_some()
        || (network.is_some() && !given_network)
        || effort.is_some()
        || d.auth.is_some()
        || !secrets.is_empty()
        || !mcp.is_empty()
        || (!grants.is_empty() && !given_grants)
        || (models.is_some() && !given_models);
    if !wanted {
        return Ok(());
    }
    let spec = spec.get_or_insert_with(Default::default);
    if spec.model.is_none() {
        spec.model = d.model.clone();
    }
    if spec.effort.is_none() {
        spec.effort = effort;
    }
    if spec.auth.is_none() {
        spec.auth = d.auth.clone();
    }
    for secret in secrets {
        if !spec.secrets.iter().any(|s| s.name == secret.name) {
            spec.secrets.push(secret);
        }
    }
    for server in mcp {
        if !spec.mcp_servers.iter().any(|s| s.name == server.name) {
            spec.mcp_servers.push(server);
        }
    }
    if spec.connectors.is_empty() {
        spec.connectors = grants;
    }
    if spec.network.is_none() {
        spec.network = network;
    }
    if spec.models.is_none() {
        spec.models = models;
    }
    Ok(())
}

/// What a run of a task on a yard configured with `config` fills in before
/// it starts: the provisioning ([`fill_provisioning`], with the secrets
/// and grants its home can take) and `defaults.instructions`, read from
/// its file, where the caller gave none.
pub(crate) fn complete(
    yard: &Yard,
    config: &ProjectConfig,
    options: &mut TaskOptions,
) -> Result<(), Error> {
    let private = private_home(options);
    fill_provisioning(config, &mut options.provision, private)?;
    if let Some(path) = &config.defaults.instructions {
        let given = options
            .provision
            .as_ref()
            .is_some_and(|p| p.instructions.is_some());
        if !given {
            let file = config::resolve(yard.root(), path);
            let text = std::fs::read_to_string(&file).map_err(|e| {
                Error::Config(format!("defaults.instructions {}: {e}", file.display()))
            })?;
            options
                .provision
                .get_or_insert_with(Default::default)
                .instructions = Some(text);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> ProjectConfig {
        config::parse(text).unwrap()
    }

    #[test]
    fn task_options_take_the_defaults_and_the_sandbox() {
        let config = parsed(
            r#"
[defaults]
harness = "codex"
budget_usd = 5
max_turns = 9
max_minutes = 2
permissions = "read-only"
isolated = true
check = "cargo test -p x"
provider = "microsandbox"
[microsandbox]
image = "ghcr.io/acme/harness:1"
cpus = 2
keep = "pause"
max_kept_minutes = 3
live_branch = true
"#,
        );
        let options = task_options(&config).unwrap();
        assert_eq!(options.harness.as_deref(), Some("codex"));
        assert_eq!(options.budget.max_usd, Some(5.0));
        assert_eq!(options.budget.max_turns, Some(9));
        assert_eq!(options.budget.max_duration, Some(Duration::from_secs(120)));
        assert_eq!(options.policy.preset(), Some(PolicyPreset::ReadOnly));
        assert!(options.isolated);
        assert_eq!(
            options.check.as_deref(),
            Some(
                &[
                    "cargo".to_owned(),
                    "test".to_owned(),
                    "-p".to_owned(),
                    "x".to_owned()
                ][..]
            )
        );
        // The provider, by its kind and its serialized options: only the
        // providers directory names a variant.
        let provider = options.provider.unwrap();
        assert_eq!(provider.kind().name(), "microsandbox");
        let sandbox = serde_json::to_value(&provider).unwrap();
        assert_eq!(sandbox["image"], "ghcr.io/acme/harness:1");
        assert_eq!(sandbox["cpus"], 2);
        assert_eq!(sandbox["keep"], "pause");
        assert_eq!(sandbox["max_kept_secs"], 180);
        assert_eq!(sandbox["live_branch"], true);
    }

    #[test]
    fn an_empty_configuration_gives_the_default_options() {
        let options = task_options(&ProjectConfig::default()).unwrap();
        assert_eq!(options.harness, None);
        assert_eq!(options.check, None);
        assert_eq!(options.budget, Budget::default());
        assert_eq!(options.provider, None);
        assert!(!options.isolated);
        assert_eq!(options.policy.preset(), None);
    }

    #[test]
    fn yes_allows_everything_and_ask_keeps_the_default() {
        let yes = parsed("[defaults]\npermissions = \"yes\"\n");
        let options = task_options(&yes).unwrap();
        assert_eq!(options.policy.summary(), Policy::allow_all().summary());
        let ask = parsed("[defaults]\npermissions = \"ask\"\n");
        let options = task_options(&ask).unwrap();
        assert_eq!(options.policy.summary(), Policy::default().summary());
    }

    #[test]
    fn a_bad_check_names_its_key() {
        // The parser refuses such a file; a built configuration reaches
        // the same check.
        let config = ProjectConfig {
            defaults: config::Defaults {
                check: Some("cargo 'test".into()),
                ..config::Defaults::default()
            },
            ..ProjectConfig::default()
        };
        let error = match task_options(&config) {
            Ok(_) => panic!("accepted"),
            Err(e) => e.to_string(),
        };
        assert!(error.starts_with("defaults.check: "), "{error}");
    }

    const PROVISIONED: &str = r#"
[defaults]
model = "large"
effort = "high"
auth = "api-key"
[secrets]
OPENAI_API_KEY = "MY_OPENAI"
GITHUB_TOKEN = "GH"
[mcp]
docs = "/bin/docs --stdio"
[network]
allow = ["github.com"]
"#;

    #[test]
    fn provisioning_is_filled_where_the_caller_set_nothing() {
        let config = parsed(PROVISIONED);
        let mut spec = Some(Provisioning {
            model: Some("small".into()),
            ..Provisioning::default()
        });
        fill_provisioning(&config, &mut spec, true).unwrap();
        let spec = spec.unwrap();
        assert_eq!(spec.model.as_deref(), Some("small"), "the caller's wins");
        assert_eq!(spec.effort, Some(Effort::High));
        assert_eq!(spec.auth.as_deref(), Some("api-key"));
        let names: Vec<&str> = spec.secrets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["GITHUB_TOKEN", "OPENAI_API_KEY"]);
        assert_eq!(spec.mcp_servers.len(), 1);
        assert_eq!(spec.mcp_servers[0].name, "docs");
        assert!(spec.network.is_some());
    }

    #[test]
    fn secrets_need_a_private_home() {
        let config = parsed(PROVISIONED);
        let mut spec = None;
        fill_provisioning(&config, &mut spec, false).unwrap();
        let spec = spec.unwrap();
        assert!(spec.secrets.is_empty());
        assert_eq!(spec.model.as_deref(), Some("large"));
    }

    #[test]
    fn nothing_to_add_leaves_no_provisioning() {
        let mut spec = None;
        fill_provisioning(&ProjectConfig::default(), &mut spec, true).unwrap();
        assert!(spec.is_none());
    }

    #[test]
    fn a_branch_on_the_model_gateway_gets_no_provider_key() {
        let config = parsed(
            r#"
[secrets]
OPENAI_API_KEY = "MY_OPENAI"
ZAI_KEY = "ZAI"
GITHUB_TOKEN = "GH"
[models]
allow = ["glm-*"]
[models.backends.zai]
api = "openai"
url = "https://api.z.ai/v1"
key = "ZAI_KEY"
"#,
        );
        let mut spec = None;
        fill_provisioning(&config, &mut spec, true).unwrap();
        let spec = spec.unwrap();
        let names: Vec<&str> = spec.secrets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["GITHUB_TOKEN"]);
        assert!(spec.models.is_some());
    }

    #[test]
    fn private_home_is_isolation_or_a_sandbox() {
        assert!(!private_home(&TaskOptions::default()));
        assert!(private_home(&TaskOptions {
            isolated: true,
            ..TaskOptions::default()
        }));
        assert!(private_home(&TaskOptions {
            provider: Some(Provider::microsandbox(SandboxOptions::default())),
            ..TaskOptions::default()
        }));
        let local: Provider = serde_json::from_value(serde_json::json!({"kind": "local"})).unwrap();
        assert!(!private_home(&TaskOptions {
            provider: Some(local),
            ..TaskOptions::default()
        }));
    }

    #[test]
    fn knowledge_settings_follow_the_table() {
        let config = parsed(
            r#"
[knowledge]
provision = false
budget_tokens = 400
distill_on = ["ready"]
distiller = { harness = "claude-code", model = "small" }
"#,
        );
        let settings = knowledge_settings(&config.knowledge).unwrap();
        assert!(!settings.provision);
        assert_eq!(settings.budget_tokens, 400);
        assert_eq!(settings.distill_on, [DistillTrigger::Ready]);
        assert!(settings.distiller.is_some());
        // The parser refuses such a file; a built table reaches the same
        // check.
        let bad = KnowledgeConfig {
            distill_on: Some(vec!["always".into()]),
            ..KnowledgeConfig::default()
        };
        let error = knowledge_settings(&bad).unwrap_err().to_string();
        assert!(error.starts_with("knowledge.distill_on: "), "{error}");
    }

    /// `effects_sandbox_host` is this host's address for a sandbox, bound
    /// on `effects_listen` (loopback by default), with or without a
    /// gateway; a listen address that does not parse is refused by name.
    #[test]
    fn connector_settings_parse() {
        let config = parsed(
            r#"
[connectors]
anvil = "/opt/anvil --quiet"
bundles = "bundles"
effects_sandbox_host = "192.168.127.1"
"#,
        );
        assert_eq!(
            anvil_command(&config.connectors).unwrap(),
            ["/opt/anvil", "--quiet"]
        );
        assert_eq!(anvil_command(&Connectors::default()).unwrap(), [ANVIL]);
        assert_eq!(
            bundles(Path::new("/r"), &Connectors::default()),
            PathBuf::from("/r/connectors")
        );
        assert_eq!(sandbox_host(&Connectors::default()).unwrap(), None);
        assert_eq!(
            sandbox_host(&config.connectors).unwrap(),
            Some(SandboxHost {
                listen: Ipv4Addr::LOCALHOST.into(),
                host: "192.168.127.1".into(),
            })
        );
        let config = Connectors {
            effects_listen: Some("0.0.0.0".into()),
            ..config.connectors
        };
        assert_eq!(
            sandbox_host(&config).unwrap().unwrap().listen,
            IpAddr::from([0, 0, 0, 0])
        );
        let config = Connectors {
            effects_listen: Some("everywhere".into()),
            ..config
        };
        let refused = sandbox_host(&config).unwrap_err().to_string();
        assert!(refused.contains("connectors.effects_listen"), "{refused}");
    }

    #[test]
    fn a_key_source_follows_the_secrets_table() {
        let mut secrets = BTreeMap::new();
        secrets.insert("A".to_owned(), "MY_A".to_owned());
        secrets.insert("B".to_owned(), "@keys/b".to_owned());
        let root = Path::new("/r");
        assert_eq!(
            key_source(root, &secrets, "A"),
            KeySource::Env("MY_A".into())
        );
        assert_eq!(
            key_source(root, &secrets, "B"),
            KeySource::File(PathBuf::from("/r/keys/b"))
        );
        assert_eq!(key_source(root, &secrets, "C"), KeySource::Env("C".into()));
    }
}
