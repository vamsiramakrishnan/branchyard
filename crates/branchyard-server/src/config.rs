//! Server configuration: a JSON file, command-line flags, or both (flags
//! win). Relative paths in a file resolve against the file's directory.
//!
//! ```json
//! {
//!   "listen": "127.0.0.1:8421",
//!   "data_dir": "/var/lib/branchyard",
//!   "repos": { "app": "/srv/app" },
//!   "tokens": [{ "name": "ci", "token_file": "/etc/branchyard/ci.token" }],
//!   "tls": { "cert": "cert.pem", "key": "key.pem" },
//!   "max_body_bytes": 1048576,
//!   "max_running": 8,
//!   "shutdown_grace_seconds": 60,
//!   "harness_commands": { "codex": ["/opt/codex/bin/codex"] },
//!   "allow_client_commands": false,
//!   "allow_providers": ["substrate"],
//!   "allow_delegation": false,
//!   "by_path": "/usr/local/bin/by",
//!   "allow_unapproved_tools": false,
//!   "allow_workspace_scripts": ["app"],
//!   "secrets": { "ANTHROPIC_API_KEY": "ANTHROPIC_API_KEY", "CODEX_AUTH": "@/etc/branchyard/codex-auth.json" },
//!   "database": "postgres://branchyard@db/branchyard"
//! }
//! ```
//!
//! A `tokens` entry may add `tenant`, `scopes` and `repos` to scope its
//! principal; `credentials` names one by a token hash directly; `tenants`
//! sets each tenant's resource ceilings and repositories. See
//! `docs/server.md#identity-and-scopes` and `docs/server.md#quotas`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:8421";

/// Which repositories' workspace scripts a server runs: its operator's
/// decision, from `allow_workspace_scripts` in the configuration file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceScripts {
    /// None (the default): a repository's `[workspace]` is ignored.
    #[default]
    Denied,
    /// Every served repository's (`true`).
    All,
    /// These served repositories' (a list of names).
    Repos(BTreeSet<String>),
}

impl WorkspaceScripts {
    pub fn allows(&self, repo: &str) -> bool {
        match self {
            WorkspaceScripts::Denied => false,
            WorkspaceScripts::All => true,
            WorkspaceScripts::Repos(repos) => repos.contains(repo),
        }
    }
}

/// `allow_workspace_scripts` as written: `true`, `false`, or repository
/// names.
#[derive(Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) enum FileWorkspaceScripts {
    All(bool),
    Repos(Vec<String>),
}

/// The default tenant a bearer token belongs to when nothing says
/// otherwise: single-token deployments from before tenants existed, and a
/// `tokens` entry that does not name one.
pub const DEFAULT_TENANT: &str = "default";

/// Scopes a principal can hold. `read` covers every `GET`; `run` covers
/// starting, sending to, forking, reincarnating, spawning, cancelling and
/// steering a branch; `merge` covers merging and integrating; `admin`
/// covers removing a branch. See `docs/server.md#identity-and-scopes`.
pub const SCOPES: &[&str] = &["read", "run", "merge", "admin"];

pub fn check_scope_name(name: &str) -> Result<(), String> {
    match SCOPES.contains(&name) {
        true => Ok(()),
        false => Err(format!(
            "{name:?} is not a scope; use one of {}",
            SCOPES.join(", ")
        )),
    }
}

pub(crate) fn check_principal(principal: &Principal) -> Result<(), String> {
    if principal.origin == AuthenticationOrigin::Configured && principal.name.starts_with("google:")
    {
        return Err("the google: principal prefix is reserved for authenticated external identities; rename this static principal".into());
    }
    if principal.name.is_empty() || principal.name.len() > 128 {
        return Err(format!(
            "principal {:?} is not a usable subject name",
            principal.name
        ));
    }
    check_tenant_name(&principal.tenant)
        .map_err(|e| format!("principal {}: {e}", principal.name))?;
    for scope in &principal.scopes {
        check_scope_name(scope).map_err(|e| format!("principal {}: {e}", principal.name))?;
    }
    Ok(())
}

/// A tenant name: 1 to 128 characters, no `/` or control characters, so
/// that `tenant/subject` names one principal unambiguously (the scope of
/// its idempotency keys).
pub fn check_tenant_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 128 || name.chars().any(|c| c == '/' || c.is_control()) {
        return Err(format!(
            "tenant {name:?} is not a usable tenant name (1 to 128 characters, no '/')"
        ));
    }
    Ok(())
}

/// The SHA-256 of `bytes`, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// How a principal authenticated, persisted with queued work without its bearer.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthenticationOrigin {
    /// A configured or paired static credential. Never an external user assertion.
    #[default]
    Configured,
    /// A delegated paired credential retaining its verified issuing identity.
    Paired {
        /// Stable issuing subject, never a generated device name.
        subject: String,
        /// Original verified authentication provenance.
        origin: Box<AuthenticationOrigin>,
    },
    /// A Google-signed ID token matched an exact operator-configured binding.
    GoogleIdToken {
        /// The verified Google issuer.
        issuer: String,
        /// The verified external subject, never a display name or email.
        subject: String,
        /// The server audience against which the token was verified.
        audience: String,
    },
}

/// An exact external identity mapped to existing server authorization policy.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleIdentityBinding {
    /// Exact Google issuer, including its scheme when present.
    pub issuer: String,
    /// Exact Google subject identifier. No wildcard or email matching.
    pub subject: String,
    /// Operator-controlled tenant, scopes and repository policy.
    pub principal: Principal,
}

/// Google-only ID-token admission, independent of Google API access tokens.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleIdTokenConfig {
    /// Exact audience identifying this Branchyard server.
    pub audience: String,
    /// The only external identities admitted by this server.
    pub bindings: Vec<GoogleIdentityBinding>,
}

impl GoogleIdTokenConfig {
    /// Validate the trusted mapping before any token is accepted.
    pub fn validate(&self) -> Result<(), String> {
        if self.audience.is_empty() || self.audience.len() > 2048 || self.bindings.is_empty() {
            return Err("google_identity requires an audience and at least one binding".into());
        }
        let mut identities = BTreeSet::new();
        for binding in &self.bindings {
            if !matches!(
                binding.issuer.as_str(),
                "https://accounts.google.com" | "accounts.google.com"
            ) || binding.subject.is_empty()
                || binding.subject.len() > 256
                || binding.subject.chars().any(char::is_control)
            {
                return Err(
                    "google_identity requires an exact Google issuer and nonempty bounded subject"
                        .into(),
                );
            }
            check_principal(&binding.principal)?;
            if binding.principal.origin != AuthenticationOrigin::Configured {
                return Err(
                    "google_identity binding principal must not supply authentication provenance"
                        .into(),
                );
            }
            if !identities.insert((&binding.issuer, &binding.subject)) {
                return Err("google_identity has duplicate issuer and subject bindings".into());
            }
        }
        Ok(())
    }

    /// Recheck a direct identity's exact policy or a paired identity's issuer ceiling.
    pub fn still_allows(&self, principal: &Principal) -> bool {
        let Some(origin) = principal.root_origin() else {
            return false;
        };
        let AuthenticationOrigin::GoogleIdToken {
            issuer,
            subject,
            audience,
        } = origin
        else {
            return false;
        };
        if audience != &self.audience {
            return false;
        }
        self.bindings.iter().any(|binding| {
            if &binding.issuer != issuer || &binding.subject != subject {
                return false;
            }
            match &principal.origin {
                AuthenticationOrigin::Paired { .. } => principal.within_policy(&binding.principal),
                _ => {
                    let mut policy = principal.clone();
                    policy.origin = AuthenticationOrigin::Configured;
                    binding.principal == policy
                }
            }
        })
    }
}

/// Who a verified bearer token acts as: a tenant, a subject name, the
/// scopes it holds, and, optionally, a narrower repository allowlist than
/// its tenant's. Requests never carry a `tenant_id`; a principal's tenant
/// comes only from the credential that authenticated it. See
/// `docs/server.md#identity-and-scopes`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    /// Authenticated origin, defaulting to static for older records.
    #[serde(default)]
    pub origin: AuthenticationOrigin,
    /// The subject this credential names, such as an operator or a CI
    /// system; distinct from the tenant. Used for idempotency scoping and
    /// audit, and appears in `Debug` and logs (never the token itself).
    pub name: String,
    pub tenant: String,
    #[serde(default)]
    pub scopes: BTreeSet<String>,
    /// `None` allows every repository its tenant allows.
    #[serde(default)]
    pub repos: Option<BTreeSet<String>>,
}

impl Principal {
    /// A valid delegation chain's original authentication provenance.
    /// Every paired hop keeps one stable subject; Google roots must reproduce it.
    pub(crate) fn root_origin(&self) -> Option<&AuthenticationOrigin> {
        let subject = self.connector_subject();
        let mut origin = &self.origin;
        while let AuthenticationOrigin::Paired {
            subject: paired,
            origin: parent,
        } = origin
        {
            if paired != &subject {
                return None;
            }
            origin = parent;
        }
        if matches!(self.origin, AuthenticationOrigin::Paired { .. })
            && matches!(origin, AuthenticationOrigin::GoogleIdToken { .. })
        {
            let mut root = self.clone();
            root.origin = origin.clone();
            if root.connector_subject() != subject {
                return None;
            }
        }
        Some(origin)
    }

    /// Whether this policy is no broader than a current issuer's ceiling.
    pub(crate) fn within_policy(&self, issuer: &Principal) -> bool {
        self.tenant == issuer.tenant
            && self.scopes.is_subset(&issuer.scopes)
            && match (&self.repos, &issuer.repos) {
                (_, None) => true,
                (Some(asked), Some(allowed)) => asked.is_subset(allowed),
                (None, Some(_)) => false,
            }
    }
    /// Every scope, every repository, in [`DEFAULT_TENANT`]: what a
    /// `tokens` entry becomes when it names no tenant or scopes of its own,
    /// keeping single-token configurations working unchanged.
    pub fn default_for(name: &str) -> Principal {
        Principal {
            origin: Default::default(),
            name: name.to_owned(),
            tenant: DEFAULT_TENANT.to_owned(),
            scopes: SCOPES.iter().map(|s| s.to_string()).collect(),
            repos: None,
        }
    }

    /// Stable gateway subject: external identities cannot collapse into display names.
    /// The digest is over a canonical JSON array of verified issuer and subject.
    pub fn connector_subject(&self) -> String {
        match &self.origin {
            AuthenticationOrigin::Configured => self.name.clone(),
            AuthenticationOrigin::Paired { subject, .. } => subject.clone(),
            AuthenticationOrigin::GoogleIdToken {
                issuer, subject, ..
            } => {
                let identity = serde_json::json!([issuer, subject]).to_string();
                format!("google:{}", sha256_hex(identity.as_bytes()))
            }
        }
    }

    /// Delegate this identity to a device while retaining its verified subject.
    /// Policy remains unchanged; callers may narrow it before persisting the token.
    pub fn delegated_to(&self, device_name: String) -> Self {
        let mut delegated = self.clone();
        delegated.origin = AuthenticationOrigin::Paired {
            subject: self.connector_subject(),
            origin: Box::new(self.origin.clone()),
        };
        delegated.name = device_name;
        delegated
    }

    pub fn allows(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }

    /// Whether this principal may act on `repo`, given its tenant's policy:
    /// the tenant must allow it, and, if this principal has its own
    /// allowlist, that must too.
    pub fn repo_allowed(&self, tenant: &TenantPolicy, repo: &str) -> bool {
        tenant.allows_repo(repo) && self.repos.as_ref().is_none_or(|r| r.contains(repo))
    }
}

/// A tenant's resource ceilings and the repositories it owns. Quotas are
/// `None` when not configured, meaning unlimited; see
/// `docs/server.md#quotas`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TenantPolicy {
    /// Repositories this tenant owns. `None` (the default) means every
    /// repository this server serves: the same reach a single-token
    /// deployment always had.
    #[serde(default)]
    pub repos: Option<BTreeSet<String>>,
    /// Turns running at once across this tenant's operations.
    #[serde(default)]
    pub max_running: Option<usize>,
    /// Branches this tenant may have open across its repositories.
    #[serde(default)]
    pub max_branches: Option<usize>,
    /// Total spend, in USD, this tenant may reserve across every branch it
    /// currently has open (their recorded `cost_usd`, summed): lifetime,
    /// not a rolling window, since that is the only spend this server
    /// already tracks durably. See `docs/server.md#quotas`.
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// Total artifact bytes this tenant may have published across its
    /// repositories.
    #[serde(default)]
    pub max_artifact_bytes: Option<u64>,
    /// This tenant's share of the queue against other tenants at the same
    /// priority (default 1): claims go to the tenant with the lowest usage
    /// per unit of weight. See `docs/server.md#scheduling`.
    #[serde(default)]
    pub weight: Option<f64>,
    /// The highest priority this tenant's operations may carry; a request
    /// asking for more is admitted at this. Default 10, the highest.
    #[serde(default)]
    pub max_priority: Option<i32>,
}

impl TenantPolicy {
    pub fn allows_repo(&self, repo: &str) -> bool {
        self.repos.as_ref().is_none_or(|r| r.contains(repo))
    }
}

/// A credential naming its principal directly by a token hash, for the
/// hash-only configuration path (`branchyard-server token new`); see
/// `docs/server.md#identity-and-scopes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    /// SHA-256 of a bearer token, lowercase hex. The plaintext token never
    /// enters the configuration.
    pub token_sha256: String,
    pub principal: Principal,
}

/// Default [`Config::max_artifact_bytes`]: sane for occasional build
/// outputs and logs, raised by the operator for larger ones.
pub const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// A bearer token. Its secret never appears in `Debug` output or logs.
#[derive(Clone)]
pub struct Token {
    pub name: String,
    pub secret: String,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("name", &self.name)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// An operator-configured webhook target; see `docs/server.md#webhooks`.
#[derive(Clone)]
pub struct WebhookConfig {
    /// Stable across restarts: identifies this target's durable delivery
    /// cursor. Derived from the URL unless the file gives one explicitly.
    pub id: String,
    pub url: String,
    /// HMAC-SHA256 key signing each envelope. Never in `Debug` output.
    pub secret: String,
    /// Activity kinds this target receives (`status`, `stall`,
    /// `permission_wait`, `merge`, `failure`); empty means every kind.
    pub events: BTreeSet<String>,
}

impl fmt::Debug for WebhookConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookConfig")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("secret", &"<redacted>")
            .field("events", &self.events)
            .finish()
    }
}

/// Recognized webhook event kinds, matched against a stall, a permission
/// request, and a status change (further split into `merge` and `failure`
/// for its `Merged` and `Failed` states). An empty filter means all of
/// them.
pub const WEBHOOK_EVENT_KINDS: &[&str] =
    &["status", "stall", "permission_wait", "merge", "failure"];

pub fn check_webhook_event_kind(name: &str) -> Result<(), String> {
    match WEBHOOK_EVENT_KINDS.contains(&name) {
        true => Ok(()),
        false => Err(format!(
            "{name:?} is not a webhook event kind; use one of {}",
            WEBHOOK_EVENT_KINDS.join(", ")
        )),
    }
}

/// Refuse a webhook URL that is not `https://`, unless its host is a
/// loopback literal (`localhost`, `127.0.0.1`, `::1`) or `insecure` is set.
pub fn check_webhook_url(url: &str, insecure: bool) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("webhook url {url:?}: {e}"))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
            match loopback || insecure {
                true => Ok(()),
                false => Err(format!(
                    "webhook url {url:?} is not https and not loopback; pass \
                     --webhook-insecure to allow it anyway"
                )),
            }
        }
        other => Err(format!(
            "webhook url {url:?} has scheme {other:?}; use https:// (or http:// on loopback)"
        )),
    }
}

/// Operator-owned storage for repository-free console projects.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ManagedKnowledge {
    /// Explicit operator opt-in; disabled by default.
    pub enabled: bool,
    /// Absolute server-owned root; defaults to `data_dir/knowledge`.
    pub root: Option<PathBuf>,
    /// Maximum workspace allocations per tenant.
    pub max_workspaces: usize,
    /// Maximum original source bytes per workspace.
    pub max_source_bytes: u64,
    /// Maximum size of one source upload.
    pub max_upload_bytes: usize,
}
impl Default for ManagedKnowledge {
    fn default() -> Self {
        Self {
            enabled: false,
            root: None,
            max_workspaces: 100,
            max_source_bytes: 64 * 1024 * 1024,
            max_upload_bytes: 8 * 1024 * 1024,
        }
    }
}

/// An operator-approved execution environment for workspace sessions, one
/// entry of `execution_presets`. A session start names its `id`; the server
/// supplies the provider, secrets and worker labels, and then applies the
/// same checks as to any request. See docs/server.md#execution-presets.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExecutionPreset {
    /// Stable ID a session start names: 1 to 64 of a-z, 0-9 and '-'.
    pub id: String,
    /// Plain-language name for the console.
    pub label: String,
    /// Plain-language description for the console.
    #[serde(default)]
    pub description: String,
    /// The environment, in the form of a request's `provider`: `local`,
    /// `substrate` or `microsandbox`. Its kind must be in `allow_providers`
    /// (`local` always is). Relative paths resolve against this file's
    /// directory.
    pub provider: branchyard::Provider,
    /// Harness or profile IDs a session may choose, matched exactly.
    pub harnesses: Vec<PresetHarness>,
    /// Names in `secrets` added to the session's provisioning.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Tenants that may use this preset. Absent: every tenant.
    #[serde(default)]
    pub tenants: Option<Vec<String>>,
    /// Worker labels every turn of the session requires, so it runs only
    /// where this environment's files are.
    #[serde(default)]
    pub require_labels: Vec<String>,
    /// The provider as the file wrote it, when resolving its relative paths
    /// changed it: error projection hides both forms.
    #[serde(skip)]
    pub written: Option<branchyard::Provider>,
}

/// One harness an execution preset offers, and the models it may run.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PresetHarness {
    /// A harness or profile ID, matched exactly: `claude-code` does not
    /// admit `claude-code-acp`.
    pub harness: String,
    /// Models a session may choose. Absent: a session may not choose one.
    #[serde(default)]
    pub models: Option<Vec<String>>,
    /// The model a session runs when it names none. Default: the first of
    /// `models`.
    #[serde(default)]
    pub default_model: Option<String>,
}

impl PresetHarness {
    /// The model a session that names none runs: `default_model`, else the
    /// first of `models`, else the harness's own choice.
    pub fn default_model(&self) -> Option<&str> {
        self.default_model
            .as_deref()
            .or_else(|| self.models.as_ref()?.first().map(String::as_str))
    }
}

/// A Substrate provider's options. Read through the provider's JSON form:
/// only `crates/branchyard/src/providers/` names a `Provider` variant
/// (`crates/branchyard/tests/provider_seam.rs`).
pub(crate) fn substrate_options(
    provider: &branchyard::Provider,
) -> Option<branchyard::SubstrateOptions> {
    if provider.label() != "substrate" {
        return None;
    }
    serde_json::to_value(provider)
        .and_then(serde_json::from_value)
        .ok()
}

/// `preset` with its Substrate paths resolved by `resolve`, keeping the
/// provider as written when that changed it.
fn resolve_preset(
    mut preset: ExecutionPreset,
    resolve: impl Fn(PathBuf) -> PathBuf,
) -> ExecutionPreset {
    let Some(written) = substrate_options(&preset.provider) else {
        return preset;
    };
    let mut options = written.clone();
    options.key = resolve(options.key);
    for path in [
        &mut options.api_token_file,
        &mut options.ca,
        &mut options.client_cert,
        &mut options.client_key,
        &mut options.router_ca,
    ] {
        *path = path.take().map(&resolve);
    }
    if options != written {
        preset.written = Some(preset.provider.clone());
        preset.provider = branchyard::Provider::substrate(options);
    }
    preset
}

/// An execution preset ID: 1 to 64 of a-z, 0-9 and '-'.
fn valid_preset_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A resolved server configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    /// Name and path of each served repository.
    pub repos: Vec<(String, PathBuf)>,
    /// Operator-owned knowledge workspace storage and limits.
    pub managed_knowledge: ManagedKnowledge,
    pub tokens: Vec<Token>,
    /// The token file serving made or used because the configuration and
    /// the flags gave no credential (`<data_dir>/token`): what a client
    /// passes as `--token-file`. `None` when credentials were configured.
    pub default_token_file: Option<PathBuf>,
    /// A `tokens` entry's principal (tenant, scopes, repository
    /// allowlist), keyed by its `name`. An entry missing here gets
    /// [`Principal::default_for`]: every scope, every repository, in
    /// [`DEFAULT_TENANT`] — unchanged single-token behavior.
    pub principals: BTreeMap<String, Principal>,
    /// Credentials naming their principal by a token hash directly,
    /// alongside `tokens`.
    pub credentials: Vec<Credential>,
    /// Optional Google-only verified caller admission.
    pub google_identity: Option<GoogleIdTokenConfig>,
    #[cfg(test)]
    pub(crate) google_jwks_url: Option<String>,
    /// Each tenant's resource ceilings and repositories, keyed by tenant
    /// name. A tenant a principal names but that has no entry here has no
    /// configured limits (unlimited) and, by default, every repository.
    pub tenants: BTreeMap<String, TenantPolicy>,
    pub tls: Option<TlsFiles>,
    /// Serve plain HTTP on a non-loopback address. Only from the flag.
    pub insecure_bind: bool,
    /// Listen on this Unix domain socket instead of `listen` (plain HTTP,
    /// mode 0600, in a directory only its owner may enter). Only from the
    /// flag, `--listen-unix`; it is how `by --remote ssh://` reaches a
    /// server it starts (docs/remote-ssh.md).
    pub listen_unix: Option<PathBuf>,
    pub max_body_bytes: usize,
    /// Largest artifact a `POST .../artifacts` upload may publish; larger
    /// ones are refused with `413 body_too_large` before being written
    /// anywhere. See `docs/storage.md`.
    pub max_artifact_bytes: u64,
    /// Operations running at once; more wait queued.
    pub max_running: usize,
    /// How long a worker's claim on a queued operation lasts without
    /// renewal. Another worker takes over a claim that expired.
    pub operation_lease: Duration,
    /// Only run queued operations: bind no listener and deliver no
    /// webhooks. Needs `database`.
    pub worker_only: bool,
    /// The labels this server's worker carries (`gpu`, `linux`): it claims
    /// only operations whose `require_labels` are all among them. See
    /// `docs/server.md#worker-labels`.
    pub labels: Vec<String>,
    /// Detect the harnesses on this machine and advertise them with the
    /// worker's beats, adding a `harness:<id>` label for each that can run
    /// (docs/harness-lifecycle.md). On by default.
    pub inventory: bool,
    /// How to detect it; `None` uses the process's source
    /// (`crate::ops::inventory_source`). For embedding and tests.
    pub inventory_source: Option<crate::ops::InventorySource>,
    /// How long an operation may wait queued before it says why no live
    /// worker can claim it.
    pub unclaimable_after: Duration,
    /// How long shutdown waits for running operations.
    pub shutdown_grace: Duration,
    /// Executable per harness, used when a request names none.
    pub harness_commands: BTreeMap<String, Vec<String>>,
    /// Accept a request's own `command`. Off by default: it lets any token
    /// holder choose what the server executes.
    pub allow_client_commands: bool,
    /// Providers a request may name besides `local`: `microsandbox`,
    /// `substrate`. Empty by default.
    pub allow_providers: BTreeSet<String>,
    /// Accept a delegation envelope, `allow_delegation`, and the spawn
    /// endpoint. Off by default.
    pub allow_delegation: bool,
    /// The `by` a delegating harness gets. Default: the server's own
    /// executable when it is `by`, else `by` beside it, else on `PATH`.
    pub by_path: Option<PathBuf>,
    /// Accept `unapproved_tools`, which runs profiles whose tools bypass
    /// the request's policy. Off by default.
    pub allow_unapproved_tools: bool,
    /// Which served repositories' `[workspace]` scripts (branchyard.toml)
    /// this server runs for new branches. None by default; a request can
    /// never ask for them. See `docs/workspace.md`.
    pub allow_workspace_scripts: WorkspaceScripts,
    /// Secrets a request may name, and where this server reads each: a
    /// variable of its own environment or a file. A request names secrets
    /// only; it never chooses a source. Empty by default.
    pub secrets: BTreeMap<String, branchyard::SecretSource>,
    /// Operator-approved environments a workspace session may name
    /// (`execution_presets`). Empty by default: the console then offers one
    /// implicit `local` preset.
    pub execution_presets: Vec<ExecutionPreset>,
    /// Workspace session starts must name an execution preset and may not
    /// carry `provider` or `provision`. Off by default; needs at least one
    /// preset.
    pub require_workspace_execution_presets: bool,
    /// A PostgreSQL URL: keep branch state and operations there instead of
    /// SQLite. Only in a build with the `postgres` feature.
    pub database: Option<String>,
    /// How often the feed looks in the repository's store for activity
    /// recorded by other processes.
    pub poll_interval: Duration,
    /// How often each served repository is recovered: branches whose
    /// engine stopped, and waiting dependents whose prerequisites settled
    /// (`Yard::resume_graph`). Every server and `by worker` on a database
    /// does this; see `docs/graph.md#durable-scheduling`.
    pub recover_interval: Duration,
    /// Log one line per request to stderr.
    pub log_requests: bool,
    /// Webhook targets notified of every served repository's activity feed.
    pub webhooks: Vec<WebhookConfig>,
    /// Allow a webhook's `http://` URL off loopback. Only from the flag.
    pub webhook_insecure: bool,
    /// The connector gateway its branches' turns are given, and whether
    /// this server runs it; see `docs/connectors.md`. `None`: off.
    pub connectors: Option<ConnectorsConfig>,
    /// The model gateway (docs/model-gateway.md), as `[models]` in
    /// branchyard.toml says it. `None`: off.
    pub models: Option<branchyard_setup::config::ModelsConfig>,
    /// Each principal's ceiling: the most a branch acting for it may reach
    /// (docs/model-gateway.md#one-scope).
    pub ceilings: BTreeMap<String, branchyard::Ceiling>,
    /// The administrator's locked approval policy and the people's
    /// (docs/effects.md#approvals).
    pub approvals: branchyard::effects::ApprovalSettings,
    /// Waiting this long queued raises an operation's effective priority
    /// by one; `None` turns aging off. See `docs/server.md#scheduling`.
    pub aging: Option<Duration>,
    /// How long a claim keeps counting toward its tenant's usage, as the
    /// time constant of its decay.
    pub fair_share_window: Duration,
    /// `/metrics`; `None` (the default): not served.
    pub metrics: Option<MetricsConfig>,
    /// The metrics registry and tracer to use, for embedding and tests;
    /// `None` (the default): a fresh registry, and traces as the
    /// OpenTelemetry variables configure them. Never from a file or flag.
    pub observability: Option<crate::observe::Observability>,
    /// Triggers and schedules: the public URL in webhook URLs, which
    /// repositories' triggers may run prechecks, the dispatcher's tick and
    /// clock. See `docs/triggers.md`.
    pub triggers: crate::triggers::Settings,
    /// The web companion at `/app/`, pairing links and Web Push; off by
    /// default. See `docs/companion.md`.
    pub app: AppConfig,
    /// Sync served repositories' branches to durable storage: pull a task
    /// before running it, under its lease, and push as it goes. `None`
    /// (the default): no sync. See `docs/sync.md`.
    pub sync: Option<SyncSettings>,
}

/// `sync` in the configuration file; see `docs/sync.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncSettings {
    pub config: branchyard_sync::SyncConfig,
    /// How long a task's lease lasts between renewals.
    pub lease: Duration,
}

/// Push services a subscription may name by default: Chrome's (FCM),
/// Firefox's (Mozilla autopush), Edge's (WNS) and Safari's (Apple).
pub const DEFAULT_PUSH_SERVICES: &[&str] = &[
    "fcm.googleapis.com",
    "*.push.services.mozilla.com",
    "*.notify.windows.com",
    "*.push.apple.com",
];

/// `app` in the configuration file, `--app` on the command line: the web
/// companion. See `docs/companion.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    /// Serve the page at `/app/`, accept pairing codes and paired tokens.
    pub enabled: bool,
    /// Send Web Push notifications to subscribed browsers.
    pub push: bool,
    /// The VAPID private key (PKCS#8, made if missing). Default:
    /// `<data_dir>/companion/vapid.pk8`. Servers sharing a database that
    /// all send push need the same key.
    pub vapid_key: Option<PathBuf>,
    /// The VAPID `sub`: a `mailto:` or `https:` contact for push services.
    /// Default: `public_url` when it is `https://`, else
    /// `mailto:branchyard@localhost`.
    pub push_subject: Option<String>,
    /// Hosts subscriptions may name (`host` or `*.suffix`).
    pub push_services: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> AppConfig {
        AppConfig {
            enabled: false,
            push: true,
            vapid_key: None,
            push_subject: None,
            push_services: DEFAULT_PUSH_SERVICES
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl AppConfig {
    pub fn vapid_key_path(&self, data_dir: &Path) -> PathBuf {
        self.vapid_key
            .clone()
            .unwrap_or_else(|| data_dir.join("companion").join("vapid.pk8"))
    }

    /// The VAPID subject for `config`.
    pub fn subject(&self, config: &Config) -> String {
        if let Some(subject) = &self.push_subject {
            return subject.clone();
        }
        match &config.triggers.public_url {
            Some(url) if url.starts_with("https://") => url.clone(),
            _ => "mailto:branchyard@localhost".into(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(subject) = &self.push_subject {
            if !(subject.starts_with("mailto:") || subject.starts_with("https://")) {
                return Err(format!(
                    "app.push_subject: {subject:?} is not a mailto: or https: URL"
                ));
            }
        }
        for service in &self.push_services {
            let host = service.strip_prefix("*.").unwrap_or(service);
            if host.is_empty() || host.contains(['/', ' ', '*', '@']) {
                return Err(format!(
                    "app.push_services: {service:?} is not a host name or *.suffix"
                ));
            }
        }
        Ok(())
    }
}

/// `connectors` in the configuration file: the connector gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorsConfig {
    /// The gateway's canonical `/mcp` URL: every token's audience.
    pub gateway: String,
    /// The gateway as a sandboxed harness reaches it.
    pub sandbox_gateway: Option<String>,
    /// Every token's issuer. Default: this server's URL.
    pub issuer: Option<String>,
    /// The signing key file (a private JWKS, made if missing). Default:
    /// `<data_dir>/gateway/key`.
    pub signing_key: Option<PathBuf>,
    /// The gateway's audit log, read into `connector_call` events.
    /// Default: `<data_dir>/gateway/audit.jsonl`.
    pub audit_file: Option<PathBuf>,
    /// The bundle root the gateway serves.
    pub bundles: PathBuf,
    /// Anvil's command and leading arguments. Default `["anvil"]`.
    pub anvil: Vec<String>,
    /// Run the gateway beside the server, supervised.
    pub run_gateway: bool,
    /// The address the gateway listens on when run here, when not
    /// loopback.
    pub listen: Option<String>,
    /// The gateway's vault key file. Default: `<data_dir>/gateway/vault.key`,
    /// made if missing.
    pub vault_key: Option<PathBuf>,
    /// Whether each turn calls the gateway through the effect ledger's
    /// proxy (docs/effects.md). Default on.
    pub effects_proxy: Option<bool>,
    /// What each turn's ledger proxy, and a sandboxed turn's delegation
    /// endpoint, binds. Default loopback.
    pub effects_listen: Option<std::net::IpAddr>,
    /// How a sandboxed harness reaches this host: its ledger proxy and its
    /// delegation endpoint.
    pub effects_sandbox_host: Option<String>,
}

/// What a build without the `postgres` feature says to a configuration that
/// names a `database`: where to get a build that can serve it. `by serve
/// --check` says it too, so the first start is not the first a person hears
/// of it.
pub const NO_POSTGRES: &str = "this build has no PostgreSQL support; use a prebuilt release, \
     which has it, or rebuild from a checkout with `cargo install --locked --path \
     crates/branchyard-cli --features postgres` (`--path crates/branchyard-server` for \
     branchyard-server); see docs/modes.md#postgres";

impl Config {
    /// Defaults for everything but repositories, tokens and data directory.
    #[allow(clippy::expect_used)] // ratchet: branchyard-server
    pub fn new(data_dir: PathBuf) -> Config {
        Config {
            listen: DEFAULT_LISTEN.parse().expect("valid default address"),
            data_dir,
            repos: Vec::new(),
            managed_knowledge: ManagedKnowledge::default(),
            tokens: Vec::new(),
            default_token_file: None,
            principals: BTreeMap::new(),
            credentials: Vec::new(),
            google_identity: None,
            #[cfg(test)]
            google_jwks_url: None,
            tenants: BTreeMap::new(),
            tls: None,
            insecure_bind: false,
            listen_unix: None,
            max_body_bytes: 1024 * 1024,
            max_artifact_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
            max_running: 8,
            operation_lease: crate::ops::DEFAULT_LEASE,
            worker_only: false,
            labels: Vec::new(),
            inventory: true,
            inventory_source: None,
            unclaimable_after: crate::ops::DEFAULT_UNCLAIMABLE_AFTER,
            shutdown_grace: Duration::from_secs(60),
            harness_commands: BTreeMap::new(),
            allow_client_commands: false,
            allow_providers: BTreeSet::new(),
            allow_delegation: false,
            by_path: None,
            allow_unapproved_tools: false,
            allow_workspace_scripts: WorkspaceScripts::Denied,
            secrets: BTreeMap::new(),
            execution_presets: Vec::new(),
            require_workspace_execution_presets: false,
            database: None,
            poll_interval: Duration::from_millis(500),
            recover_interval: crate::serve::RECOVER_EVERY,
            log_requests: true,
            webhooks: Vec::new(),
            webhook_insecure: false,
            connectors: None,
            models: None,
            ceilings: BTreeMap::new(),
            approvals: branchyard::effects::ApprovalSettings::default(),
            aging: Some(crate::store::DEFAULT_AGING),
            fair_share_window: crate::store::DEFAULT_FAIR_SHARE_WINDOW,
            metrics: None,
            observability: None,
            triggers: crate::triggers::Settings::default(),
            app: AppConfig::default(),
            sync: None,
        }
    }

    /// How this server's dispatcher orders its claims: the configured
    /// tenants' weights, aging and fair-share window.
    pub fn scheduling(&self) -> crate::store::Scheduling {
        crate::store::Scheduling {
            weights: self
                .tenants
                .iter()
                .filter_map(|(name, t)| t.weight.map(|w| (name.clone(), w)))
                .collect(),
            aging: self.aging,
            window: self.fair_share_window,
            clock_ms: None,
        }
    }

    /// A `tokens` entry's principal: its own from `principals` if given,
    /// else [`Principal::default_for`] (every scope, every repository, in
    /// [`DEFAULT_TENANT`]) — what makes a single-token deployment keep
    /// working unchanged.
    pub fn principal_for(&self, token_name: &str) -> Principal {
        self.principals
            .get(token_name)
            .cloned()
            .unwrap_or_else(|| Principal::default_for(token_name))
    }

    /// A tenant's policy, or the unlimited default when it is not
    /// configured.
    pub fn tenant_policy(&self, tenant: &str) -> TenantPolicy {
        self.tenants.get(tenant).cloned().unwrap_or_default()
    }

    /// Apply configured name-based policy to the corresponding authenticated subjects.
    pub(crate) fn actor_policy<T: Clone>(
        &self,
        policies: &BTreeMap<String, T>,
    ) -> BTreeMap<String, T> {
        let mut expanded = policies.clone();
        if let Some(google) = &self.google_identity {
            for binding in &google.bindings {
                if let Some(policy) = policies.get(&binding.principal.name) {
                    let mut principal = binding.principal.clone();
                    principal.origin = AuthenticationOrigin::GoogleIdToken {
                        issuer: binding.issuer.clone(),
                        subject: binding.subject.clone(),
                        audience: google.audience.clone(),
                    };
                    expanded.insert(principal.connector_subject(), policy.clone());
                }
            }
        }
        expanded
    }

    /// Every credential this server accepts: `tokens` (hashed here, so the
    /// verifier never holds a plaintext secret) with their principal, plus
    /// `credentials` directly.
    pub fn all_credentials(&self) -> Vec<Credential> {
        let mut all: Vec<Credential> = self
            .tokens
            .iter()
            .map(|t| Credential {
                token_sha256: sha256_hex(t.secret.as_bytes()),
                principal: self.principal_for(&t.name),
            })
            .collect();
        all.extend(self.credentials.iter().cloned());
        all
    }

    /// Refuse what cannot work, and a plain-HTTP bind beyond loopback
    /// unless explicitly allowed. Returns a warning to print loudly when
    /// serving insecurely.
    pub fn validate(&self) -> Result<Option<String>, String> {
        if self.repos.is_empty() && !self.managed_knowledge.enabled {
            return Err(
                "no repositories to serve; enable managed_knowledge for knowledge projects".into(),
            );
        }
        if self.managed_knowledge.enabled {
            let knowledge = &self.managed_knowledge;
            if knowledge
                .root
                .as_ref()
                .is_some_and(|root| !root.is_absolute())
            {
                return Err(
                    "managed_knowledge.root must be an absolute operator-owned path".into(),
                );
            }
            if knowledge.max_workspaces == 0
                || knowledge.max_source_bytes == 0
                || knowledge.max_upload_bytes == 0
            {
                return Err("managed_knowledge quotas must be positive".into());
            }
        }
        if let Some(c) = &self.connectors {
            for url in std::iter::once(&c.gateway).chain(&c.sandbox_gateway) {
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(format!(
                        "connectors: {url:?} is not an http:// or https:// URL"
                    ));
                }
            }
            if c.anvil.is_empty() {
                return Err("connectors.anvil names no command".into());
            }
        }
        self.app.validate()?;
        if let Some(identity) = &self.google_identity {
            identity.validate()?;
        }
        if self.tokens.is_empty()
            && self.credentials.is_empty()
            && self.google_identity.is_none()
            && !self.worker_only
        {
            return Err("no tokens or credentials configured; every request needs one".into());
        }
        for (name, _) in &self.repos {
            check_repo_name(name)?;
        }
        if let Some(url) = &self.triggers.public_url {
            if !(url.starts_with("https://") || url.starts_with("http://"))
                || url.contains(char::is_whitespace)
            {
                return Err(format!(
                    "public_url: {url:?} is not an http:// or https:// URL"
                ));
            }
        }
        if let WorkspaceScripts::Repos(allowed) = &self.triggers.allow_prechecks {
            for name in allowed {
                if !self.repos.iter().any(|(n, _)| n == name) {
                    return Err(format!(
                        "allow_trigger_prechecks names {name}, which this server does not serve"
                    ));
                }
            }
        }
        if let WorkspaceScripts::Repos(allowed) = &self.allow_workspace_scripts {
            for name in allowed {
                if !self.repos.iter().any(|(repo, _)| repo == name) {
                    return Err(format!(
                        "allow_workspace_scripts names {name}, which this server does not serve"
                    ));
                }
            }
        }
        let mut names: Vec<&str> = self.repos.iter().map(|(n, _)| n.as_str()).collect();
        names.sort_unstable();
        if let Some(pair) = names.windows(2).find(|w| w[0] == w[1]) {
            return Err(format!("repository {} is configured twice", pair[0]));
        }
        for token in &self.tokens {
            check_principal(&self.principal_for(&token.name))?;
            if token.secret.len() < 16 {
                return Err(format!(
                    "token {} is shorter than 16 characters",
                    token.name
                ));
            }
        }
        for (name, principal) in &self.principals {
            if !self.tokens.iter().any(|t| &t.name == name) {
                return Err(format!(
                    "principals.{name} names no token; principals apply only to tokens entries"
                ));
            }
            check_principal(principal)?;
            if principal.origin != AuthenticationOrigin::Configured {
                return Err(
                    "configured principal cannot assert an external authentication origin".into(),
                );
            }
        }
        // Every accepted secret, plaintext or hashed, is one entry: the
        // same secret twice could name two principals, and the verifier
        // would silently pick one. Refused naming the entries, never the
        // secret.
        let mut hashes = BTreeSet::new();
        let mut token_names = std::collections::BTreeMap::new();
        for token in &self.tokens {
            let hash = sha256_hex(token.secret.as_bytes());
            if let Some(first) = token_names.insert(hash.clone(), token.name.clone()) {
                return Err(format!(
                    "tokens {first} and {} have the same secret; each needs its own",
                    token.name
                ));
            }
            hashes.insert(hash);
        }
        for credential in &self.credentials {
            let hash = &credential.token_sha256;
            if hash.len() != 64
                || !hash
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err(format!(
                    "credential {hash:?} is not a 64-character lowercase hex SHA-256"
                ));
            }
            if let Some(token) = token_names.get(hash) {
                return Err(format!(
                    "credential {hash} is also token {token}'s secret; configure it once"
                ));
            }
            if !hashes.insert(hash.clone()) {
                return Err(format!("credential {hash} is configured twice"));
            }
            check_principal(&credential.principal)?;
            if credential.principal.origin != AuthenticationOrigin::Configured {
                return Err(
                    "configured credential cannot assert an external authentication origin".into(),
                );
            }
        }
        for name in self.tenants.keys() {
            check_tenant_name(name)?;
        }
        for provider in &self.allow_providers {
            check_provider_name(provider)?;
        }
        self.check_execution_presets()?;
        if let Some(by) = &self.by_path {
            if !by.is_file() {
                return Err(format!("by_path {} is not a file", by.display()));
            }
        }
        if let Some(url) = &self.database {
            if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
                return Err("database must be a PostgreSQL URL, postgres://user@host/name".into());
            }
        }
        if self.worker_only && self.database.is_none() {
            return Err(
                "a worker needs --database: it runs operations other servers queued there".into(),
            );
        }
        if self.operation_lease < Duration::from_millis(100) {
            return Err("operation_lease must be at least 100 milliseconds".into());
        }
        if let Some(bad) = self.labels.iter().find(|l| !crate::store::valid_label(l)) {
            return Err(format!(
                "label {bad:?} is not a worker label: 1 to 63 of a-z, 0-9, '.', '_' and '-', \
                 starting with a letter or digit"
            ));
        }
        if self.max_body_bytes < 1024 {
            return Err("max_body_bytes must be at least 1024".into());
        }
        for (name, tenant) in &self.tenants {
            if let Some(weight) = tenant.weight {
                if !(weight.is_finite() && weight > 0.0 && weight <= 1e6) {
                    return Err(format!(
                        "tenant {name}: weight {weight} must be a positive number up to 1000000"
                    ));
                }
            }
            if let Some(max) = tenant.max_priority {
                let range = crate::store::MIN_PRIORITY..=crate::store::MAX_PRIORITY;
                if !range.contains(&max) {
                    return Err(format!(
                        "tenant {name}: max_priority {max} is outside {} to {}",
                        crate::store::MIN_PRIORITY,
                        crate::store::MAX_PRIORITY
                    ));
                }
            }
        }
        if let Some(metrics) = &self.metrics {
            if let Some(addr) = metrics.listen {
                if !addr.ip().is_loopback() && !self.insecure_bind {
                    return Err(format!(
                        "refusing to serve metrics over plain HTTP on {addr}, which is not a \
                         loopback address; pass --insecure-bind to allow it"
                    ));
                }
                if !addr.ip().is_loopback() && metrics.token_sha256.is_none() {
                    return Err(format!(
                        "the metrics listener on {addr} is not loopback: give it a metrics token"
                    ));
                }
                if addr == self.listen && addr.port() != 0 && !self.worker_only {
                    return Err(format!(
                        "the metrics listener {addr} is the server's own listen address"
                    ));
                }
            }
        }
        if self.max_artifact_bytes < 1024 {
            return Err("max_artifact_bytes must be at least 1024".into());
        }
        let mut ids: Vec<&str> = self.webhooks.iter().map(|w| w.id.as_str()).collect();
        ids.sort_unstable();
        if let Some(pair) = ids.windows(2).find(|w| w[0] == w[1]) {
            return Err(format!("webhook {} is configured twice", pair[0]));
        }
        for webhook in &self.webhooks {
            check_webhook_url(&webhook.url, self.webhook_insecure)?;
            if webhook.secret.len() < 16 {
                return Err(format!(
                    "webhook {} secret is shorter than 16 characters",
                    webhook.id
                ));
            }
            for kind in &webhook.events {
                check_webhook_event_kind(kind)?;
            }
        }
        if let Some(path) = &self.listen_unix {
            if self.tls.is_some() {
                return Err("--listen-unix serves plain HTTP on a socket; it takes no TLS".into());
            }
            if !path.is_absolute() {
                return Err(format!(
                    "--listen-unix {} must be an absolute path",
                    path.display()
                ));
            }
            return Ok(None);
        }
        if self.listen.ip().is_loopback() || self.tls.is_some() {
            return Ok(None);
        }
        if !self.insecure_bind {
            return Err(format!(
                "refusing to serve plain HTTP on {}, which is not a loopback address; \
                 configure TLS (--tls-cert and --tls-key) or pass --insecure-bind",
                self.listen
            ));
        }
        Ok(Some(format!(
            "WARNING: serving plain HTTP on {} with --insecure-bind. Bearer tokens, prompts \
             and code cross the network unencrypted; anyone on the path can read them and \
             replay the tokens.",
            self.listen
        )))
    }

    /// Refuse an execution preset that could not run as written, after the
    /// flags merged `allow_providers` and `secrets` in.
    fn check_execution_presets(&self) -> Result<(), String> {
        if self.require_workspace_execution_presets && self.execution_presets.is_empty() {
            return Err(
                "require_workspace_execution_presets needs at least one execution_presets entry"
                    .into(),
            );
        }
        let mut ids = BTreeSet::new();
        for preset in &self.execution_presets {
            let id = &preset.id;
            if !valid_preset_id(id) {
                return Err(format!(
                    "execution preset {id:?} is not an ID: 1 to 64 of a-z, 0-9 and '-'"
                ));
            }
            if !ids.insert(id) {
                return Err(format!("execution preset {id} is configured twice"));
            }
            let fail = |what: String| Err(format!("execution preset {id}: {what}"));
            if preset.label.trim().is_empty() {
                return fail("label is empty".into());
            }
            let kind = preset.provider.label();
            if kind.starts_with("recipe") {
                return fail(
                    "a server never runs environment recipes (docs/recipes.md); use local, \
                     substrate or microsandbox"
                        .into(),
                );
            }
            if kind != "local" && !self.allow_providers.contains(&kind) {
                return fail(format!(
                    "its provider {kind} is not in allow_providers; allow it with \
                     --allow-provider {kind}"
                ));
            }
            if let Some(options) = substrate_options(&preset.provider) {
                let paths = [
                    Some(&options.key),
                    options.api_token_file.as_ref(),
                    options.ca.as_ref(),
                    options.client_cert.as_ref(),
                    options.client_key.as_ref(),
                    options.router_ca.as_ref(),
                ];
                if let Some(path) = paths.into_iter().flatten().find(|p| !p.is_absolute()) {
                    return fail(format!(
                        "{} must be an absolute path on this server",
                        path.display()
                    ));
                }
            }
            if preset.harnesses.is_empty() {
                return fail("lists no harnesses".into());
            }
            let mut harnesses = BTreeSet::new();
            for harness in &preset.harnesses {
                let name = &harness.harness;
                if branchyard::harness_profile(name).is_err() {
                    return fail(format!(
                        "{name:?} is not a harness or profile; see `by harnesses --profiles`"
                    ));
                }
                if !harnesses.insert(name) {
                    return fail(format!("lists harness {name} twice"));
                }
                if let Some(models) = &harness.models {
                    if models.is_empty() || models.iter().any(|m| m.trim().is_empty()) {
                        return fail(format!(
                            "harness {name}: models must name models; omit it to let the \
                             harness choose"
                        ));
                    }
                }
                match (&harness.models, &harness.default_model) {
                    (Some(models), Some(default)) if !models.contains(default) => {
                        return fail(format!(
                            "harness {name}: default_model {default} is not among its models"
                        ))
                    }
                    (None, Some(default)) if default.trim().is_empty() => {
                        return fail(format!("harness {name}: default_model is empty"))
                    }
                    _ => {}
                }
            }
            if let Some(secret) = preset
                .secrets
                .iter()
                .find(|s| !self.secrets.contains_key(*s))
            {
                return fail(format!(
                    "secret {secret} is not in secrets; define it with --secret {secret}"
                ));
            }
            if let Some(tenants) = &preset.tenants {
                if tenants.is_empty() {
                    return fail("tenants is empty; omit it for every tenant".into());
                }
                for tenant in tenants {
                    check_tenant_name(tenant)?;
                }
            }
            if let Some(bad) = preset
                .require_labels
                .iter()
                .find(|l| !crate::store::valid_label(l))
            {
                return fail(format!(
                    "require_labels: {bad:?} is not a worker label: 1 to 63 of a-z, 0-9, '.', \
                     '_', '-' and ':', starting with a letter or digit"
                ));
            }
        }
        Ok(())
    }
}

/// `/metrics` (docs/observability.md): served on the main listener to a
/// principal with the `admin` scope or the metrics token, and, with
/// `listen`, on a listener of its own.
#[derive(Clone, PartialEq, Eq)]
pub struct MetricsConfig {
    /// A separate plain-HTTP listener serving only `/metrics`.
    pub listen: Option<SocketAddr>,
    /// SHA-256 of the metrics token, which reads `/metrics` and nothing
    /// else. Without one, the separate listener needs no token.
    pub token_sha256: Option<String>,
}

impl fmt::Debug for MetricsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetricsConfig")
            .field("listen", &self.listen)
            .field("token", &self.token_sha256.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The providers a server can allow requests to name.
pub const PROVIDERS: &[&str] = &["local", "microsandbox", "substrate"];

pub fn check_provider_name(name: &str) -> Result<(), String> {
    match PROVIDERS.contains(&name) {
        true => Ok(()),
        false if name == "recipe" => Err(
            "\"recipe\" cannot be allowed: a server never runs environment recipes, which run \
             where the repository is, as the person who trusted them (docs/recipes.md)"
                .into(),
        ),
        false => Err(format!(
            "{name:?} is not a provider; allow one of {}",
            PROVIDERS.join(", ")
        )),
    }
}

/// Repository names are URL segments: lowercase `[a-z0-9._-]`, starting
/// with a letter or digit.
pub fn check_repo_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
        && name.starts_with(|c: char| c.is_ascii_alphanumeric());
    match ok {
        true => Ok(()),
        false => Err(format!(
            "{name:?} is not a usable repository name: use lowercase letters, digits, '.', '_' \
             and '-', starting with a letter or digit"
        )),
    }
}

/// A repository name from a directory name, such as `My App` to `my-app`.
pub fn repo_name_for(path: &Path) -> String {
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut name = String::new();
    for c in base.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_end_matches('-').to_owned();
    if name.is_empty() {
        "repo".into()
    } else {
        name
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileConfig {
    /// Ignored: a `"$schema": "https://.../server.config.json"` key so
    /// editors with JSON Schema support (e.g. VS Code) offer completion
    /// and inline docs while editing this file. See
    /// `schema/server.config.json` and `docs/server.md#published-schema`.
    #[serde(rename = "$schema", default)]
    _schema: Option<String>,
    listen: Option<String>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    managed_knowledge: ManagedKnowledge,
    #[serde(default)]
    repos: BTreeMap<String, PathBuf>,
    #[serde(default)]
    tokens: Vec<FileToken>,
    #[serde(default)]
    credentials: Vec<FileCredential>,
    google_identity: Option<GoogleIdTokenConfig>,
    #[serde(default)]
    tenants: BTreeMap<String, FileTenantPolicy>,
    tls: Option<FileTls>,
    max_body_bytes: Option<usize>,
    max_artifact_bytes: Option<u64>,
    max_running: Option<usize>,
    shutdown_grace_seconds: Option<f64>,
    #[serde(default)]
    harness_commands: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    allow_client_commands: bool,
    #[serde(default)]
    allow_providers: Vec<String>,
    #[serde(default)]
    allow_delegation: bool,
    by_path: Option<PathBuf>,
    #[serde(default)]
    allow_unapproved_tools: bool,
    /// Run served repositories' `[workspace]` scripts (branchyard.toml)
    /// for new branches: `true` for every repository, or a list of
    /// repository names. Off by default; see docs/workspace.md.
    allow_workspace_scripts: Option<FileWorkspaceScripts>,
    #[serde(default)]
    secrets: BTreeMap<String, String>,
    /// Operator-approved environments a workspace session may name by ID;
    /// the server supplies their provider, secrets and worker labels. See
    /// docs/server.md#execution-presets.
    #[serde(default)]
    execution_presets: Vec<ExecutionPreset>,
    /// Workspace session starts must name one of `execution_presets` and
    /// may not carry `provider` or `provision`. Default false.
    #[serde(default)]
    require_workspace_execution_presets: bool,
    database: Option<String>,
    /// The labels this server's worker carries; it claims only operations
    /// whose `require_labels` are all among them. See
    /// docs/server.md#worker-labels.
    #[serde(default)]
    labels: Vec<String>,
    /// Detect the harnesses installed on this machine and advertise them,
    /// with a `harness:<id>` label for each that can run. Default true. See
    /// docs/harness-lifecycle.md.
    inventory: Option<bool>,
    /// How long an operation may wait queued before it says why no live
    /// worker can claim it. Default 60.
    unclaimable_after_seconds: Option<f64>,
    #[serde(default)]
    webhooks: Vec<FileWebhook>,
    #[serde(default)]
    webhook_insecure: bool,
    /// The connector gateway (docs/connectors.md): where it is, which
    /// bundles it serves, and whether this server runs it.
    connectors: Option<FileConnectors>,
    /// The model gateway (docs/model-gateway.md): backends, routes,
    /// budgets and prices, as `[models]` in branchyard.toml. A backend's
    /// `key` names an entry of `secrets`, else a variable of this server.
    models: Option<branchyard_setup::config::ModelsConfig>,
    /// Each principal's ceiling, by its name: the connectors, models and
    /// hosts any branch acting for it may have at most, whatever a request
    /// asks for (docs/model-gateway.md#one-scope).
    #[serde(default)]
    ceilings: BTreeMap<String, FileCeiling>,
    /// The administrator's locked approval policy and each principal's
    /// (docs/effects.md#approvals).
    approvals: Option<FileApprovals>,
    /// Seconds an operation waits queued before its effective priority
    /// rises by one, so low-priority work cannot starve; 0 turns aging
    /// off. Default 60. See docs/server.md#scheduling.
    aging_seconds: Option<f64>,
    /// Seconds a claim keeps counting toward its tenant's fair share (the
    /// time constant of its exponential decay); 0 counts only running
    /// operations. Default 300.
    fair_share_window_seconds: Option<f64>,
    /// Serve Prometheus metrics at /metrics (docs/observability.md). Off
    /// without this.
    metrics: Option<FileMetrics>,
    /// This server's URL as webhook senders reach it (behind a proxy, its
    /// public address): the base of each event trigger's webhook URL.
    /// Default: http(s)://<listen>. See docs/triggers.md.
    public_url: Option<String>,
    /// Let triggers of served repositories run a precheck command before
    /// firing: `true` for every repository, or a list of repository names.
    /// Off by default; see docs/triggers.md.
    allow_trigger_prechecks: Option<FileWorkspaceScripts>,
    /// The web companion at /app/ (docs/companion.md): `true`, or its
    /// settings. Off by default.
    app: Option<FileApp>,
    /// Sync served repositories' branches to durable storage
    /// (docs/sync.md): the remote, its encryption, how often and how fast.
    sync: Option<FileSync>,
}

/// `sync`: the same keys as `[sync]` in branchyard.toml, and the lease.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileSync {
    /// `gs://bucket/prefix`, `s3://bucket/prefix` (`?endpoint=` for R2 or
    /// MinIO), `az://account/container/prefix`, `file:///path`, or
    /// `git+https://`, `git+ssh://`, `git+file://`.
    remote: String,
    /// `none` (the default), `passphrase` (from
    /// BRANCHYARD_SYNC_PASSPHRASE or `passphrase_file`), or a `kms://` URL.
    encrypt: Option<String>,
    passphrase_file: Option<PathBuf>,
    /// `aes-256-gcm` (the default) or `chacha20-poly1305`, for a new
    /// encrypted remote.
    algorithm: Option<String>,
    /// How often to look for changes to push, such as `30s`. Default `1m`.
    interval: Option<String>,
    /// Bytes a second, up and down together, such as `10MB/s`.
    bandwidth: Option<String>,
    /// Objects in flight at once. Default 8.
    concurrency: Option<usize>,
    /// Delete a task this long after its last change, such as `90d`,
    /// unless held.
    retention: Option<String>,
    /// How long an unreferenced object waits before collection. Default
    /// `24h`.
    grace: Option<String>,
    /// The stored bytes the remote may hold, such as `50GB`.
    quota: Option<String>,
    /// This server's name in conflict branches and leases. Default: the
    /// host name and a random suffix.
    device: Option<String>,
    /// Seconds a task's lease lasts between renewals. Default 60.
    lease_seconds: Option<f64>,
}

/// `app`: `true`/`false`, or the companion's settings (which turn it on
/// unless `enabled` is false).
#[derive(Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) enum FileApp {
    On(bool),
    Settings(FileAppSettings),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileAppSettings {
    /// Default true.
    enabled: Option<bool>,
    /// Send Web Push notifications to subscribed browsers. Default true.
    push: Option<bool>,
    /// The VAPID private key, made if missing. Default:
    /// `<data_dir>/companion/vapid.pk8`.
    vapid_key: Option<PathBuf>,
    /// The VAPID contact, `mailto:` or `https:`. Default: `public_url` when
    /// https, else mailto:branchyard@localhost.
    push_subject: Option<String>,
    /// Push service hosts subscriptions may name (`host` or `*.suffix`).
    /// Default: FCM, Mozilla, Windows and Apple push services.
    push_services: Option<Vec<String>>,
}

/// `metrics`: Prometheus metrics at `/metrics`; see docs/observability.md.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileMetrics {
    /// A separate plain-HTTP listener (`IP:port`) serving only /metrics,
    /// as `--metrics-addr` sets it.
    listen: Option<String>,
    /// A token that reads /metrics and nothing else, at least 16
    /// characters. Exactly one of `token` and `token_file`, or neither.
    token: Option<String>,
    token_file: Option<PathBuf>,
}

/// One principal's ceiling. Unset parts leave that scope as requested.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileCeiling {
    /// The widest connector grant, in `--connector` form (`github:read`).
    connectors: Option<Vec<String>>,
    /// The models its branches may call through the model gateway, as
    /// globs (`claude-*`).
    models: Option<Vec<String>>,
    /// The hosts its branches may reach, as `--network` rules; `[]` is
    /// none.
    network: Option<Vec<String>>,
}

impl FileCeiling {
    fn ceiling(self, name: &str) -> Result<branchyard::Ceiling, String> {
        let at = |e: String| format!("ceilings.{name}: {e}");
        Ok(branchyard::Ceiling {
            connectors: self
                .connectors
                .map(|grants| {
                    grants
                        .iter()
                        .map(|g| branchyard::connectors::GrantEntry::parse(g))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()
                .map_err(at)?,
            models: match self.models {
                Some(allow) => {
                    let access = branchyard::models::ModelAccess { allow };
                    access.check().map_err(at)?;
                    Some(access)
                }
                None => None,
            },
            network: self
                .network
                .map(|rules| {
                    branchyard::Network::from_rules(&rules, branchyard::NetworkEnforce::BestEffort)
                })
                .transpose()
                .map_err(at)?,
        })
    }
}

/// `connectors`: the connector gateway; see `docs/connectors.md`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileConnectors {
    /// The gateway's canonical `/mcp` URL, such as
    /// `http://127.0.0.1:8931/mcp`: every token's audience.
    gateway: String,
    /// The gateway as a sandboxed harness reaches it.
    sandbox_gateway: Option<String>,
    /// Every token's issuer. Default: this server's URL.
    issuer: Option<String>,
    /// The signing key, a private JSON Web Key Set made if missing.
    /// Default: `<data_dir>/gateway/key`. Its public keys are served at
    /// `GET /.well-known/jwks.json`.
    signing_key: Option<PathBuf>,
    /// The gateway's audit log. Default: `<data_dir>/gateway/audit.jsonl`.
    audit_file: Option<PathBuf>,
    /// The bundle root the gateway serves.
    bundles: PathBuf,
    /// Anvil's command and leading arguments. Default `["anvil"]`.
    anvil: Option<Vec<String>>,
    /// Run the gateway beside the server, supervised. Default false.
    #[serde(default)]
    run_gateway: bool,
    /// The address the gateway listens on when run here, when not
    /// loopback.
    listen: Option<String>,
    /// The gateway's vault key (a 0600 file of 64 hex characters).
    /// Default: `<data_dir>/gateway/vault.key`, made if missing.
    vault_key: Option<PathBuf>,
    /// Call the gateway through each turn's effect-ledger proxy, which
    /// decides approvals and writes every effectful call to the ledger
    /// before it is made (docs/effects.md). Default true.
    effects_proxy: Option<bool>,
    /// The address each turn's ledger proxy, and a sandboxed turn's
    /// delegation endpoint, binds. Default `127.0.0.1`.
    effects_listen: Option<std::net::IpAddr>,
    /// How a sandboxed harness reaches this host (a host address its guest
    /// routes to), where its ledger proxy and its delegation endpoint
    /// listen. Without it, a sandboxed turn calls the gateway directly and
    /// its effects reach the ledger only from the audit log, and a
    /// sandboxed harness gets no delegation tools.
    effects_sandbox_host: Option<String>,
}

/// `approvals`: the administrator's locked policy and the people's
/// (docs/effects.md#approvals).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileApprovals {
    /// Locked: nothing below it can loosen it, and only it may change how
    /// deletions are treated.
    admin: Option<FileApprovalPolicy>,
    /// Each principal's policy, by its name.
    #[serde(default)]
    people: BTreeMap<String, FileApprovalPolicy>,
}

/// One approval policy.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileApprovalPolicy {
    /// Pattern to `allow`, `ask`, `block` or `stage`: a tool name glob
    /// (`Bash`), or `connector:operation` (`github:issues.*`).
    #[serde(default)]
    rules: BTreeMap<String, String>,
    /// Effect class (`reversible`, `compensable`, `irreversible`) to a
    /// decision, for operations no rule names.
    #[serde(default)]
    classes: BTreeMap<String, String>,
    /// How deletions are treated; honoured only in `admin`.
    deletion: Option<String>,
}

impl FileApprovalPolicy {
    fn policy(self, at: &str) -> Result<branchyard::effects::ApprovalPolicy, String> {
        branchyard::effects::ApprovalPolicy::from_strings(self.rules, self.classes, self.deletion)
            .map_err(|e| format!("{at}: {e}"))
    }
}

impl FileApprovals {
    fn settings(self) -> Result<branchyard::effects::ApprovalSettings, String> {
        Ok(branchyard::effects::ApprovalSettings {
            admin: self
                .admin
                .map(|a| a.policy("approvals.admin"))
                .transpose()?,
            person: None,
            people: self
                .people
                .into_iter()
                .map(|(name, p)| {
                    let at = format!("approvals.people.{name}");
                    p.policy(&at).map(|p| (name, p))
                })
                .collect::<Result<_, _>>()?,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileWebhook {
    /// Defaults to the URL when omitted.
    id: Option<String>,
    url: String,
    secret: Option<String>,
    secret_file: Option<PathBuf>,
    #[serde(default)]
    events: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileToken {
    name: String,
    token: Option<String>,
    token_file: Option<PathBuf>,
    /// This principal's tenant; defaults to [`DEFAULT_TENANT`] when this
    /// entry gives no tenant, scopes or repos of its own.
    tenant: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    repos: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileCredential {
    token_sha256: String,
    tenant: String,
    /// Defaults to `token_sha256`'s first 12 characters when omitted.
    name: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    repos: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileTenantPolicy {
    #[serde(default)]
    repos: Option<Vec<String>>,
    max_running: Option<usize>,
    max_branches: Option<usize>,
    max_cost_usd: Option<f64>,
    max_artifact_bytes: Option<u64>,
    /// The tenant's share of the queue against other tenants at the same
    /// priority. Default 1.
    weight: Option<f64>,
    /// The highest priority its operations may carry (-10 to 10); higher
    /// requests are admitted at this. Default 10.
    max_priority: Option<i32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(crate) struct FileTls {
    cert: PathBuf,
    key: PathBuf,
}

/// The configuration file's JSON Schema, built from `FileConfig` with
/// `schemars` rather than hand-maintained; see `schema/server.config.json`
/// (freshness checked by `tests/server_config_schema.rs`) and
/// `tools/schema-gen`.
#[cfg(feature = "schema")]
pub mod schema {
    use schemars::schema_for;

    /// The full JSON Schema document, pretty-printed with a trailing
    /// newline, matching `schema/server.config.json` byte for byte.
    #[allow(clippy::expect_used)] // ratchet: branchyard-server
    pub fn server_config_json() -> String {
        let schema = schema_for!(super::FileConfig);
        let mut text =
            serde_json::to_string_pretty(&schema).expect("a JSON Schema document serializes");
        text.push('\n');
        text
    }
}

/// Settings from a file, before flags and defaults.
#[derive(Debug, Default)]
pub struct Partial {
    pub listen: Option<SocketAddr>,
    pub data_dir: Option<PathBuf>,
    /// Operator knowledge storage settings from the config file.
    pub managed_knowledge: ManagedKnowledge,
    pub repos: Vec<(String, PathBuf)>,
    pub tokens: Vec<Token>,
    pub principals: BTreeMap<String, Principal>,
    pub credentials: Vec<Credential>,
    /// Optional Google-only verified caller admission.
    pub google_identity: Option<GoogleIdTokenConfig>,
    pub tenants: BTreeMap<String, TenantPolicy>,
    pub tls: Option<TlsFiles>,
    pub max_body_bytes: Option<usize>,
    pub max_artifact_bytes: Option<u64>,
    pub max_running: Option<usize>,
    pub shutdown_grace: Option<Duration>,
    pub harness_commands: BTreeMap<String, Vec<String>>,
    pub allow_client_commands: bool,
    pub allow_providers: Vec<String>,
    pub allow_delegation: bool,
    pub by_path: Option<PathBuf>,
    pub allow_unapproved_tools: bool,
    pub allow_workspace_scripts: WorkspaceScripts,
    pub secrets: BTreeMap<String, branchyard::SecretSource>,
    /// `execution_presets`, their relative paths resolved.
    pub execution_presets: Vec<ExecutionPreset>,
    /// `require_workspace_execution_presets`.
    pub require_workspace_execution_presets: bool,
    pub database: Option<String>,
    pub labels: Vec<String>,
    pub inventory: Option<bool>,
    pub unclaimable_after: Option<Duration>,
    pub webhooks: Vec<WebhookConfig>,
    pub webhook_insecure: bool,
    pub connectors: Option<ConnectorsConfig>,
    pub models: Option<branchyard_setup::config::ModelsConfig>,
    pub ceilings: BTreeMap<String, branchyard::Ceiling>,
    pub approvals: branchyard::effects::ApprovalSettings,
    /// `aging_seconds`: `Some(None)` turns aging off.
    pub aging: Option<Option<Duration>>,
    pub fair_share_window: Option<Duration>,
    pub metrics: Option<MetricsConfig>,
    pub public_url: Option<String>,
    pub allow_trigger_prechecks: WorkspaceScripts,
    /// `app`, when the file sets it.
    pub app: Option<AppConfig>,
    /// `sync`, when the file sets it.
    pub sync: Option<SyncSettings>,
    /// Warnings to print, such as a world-readable token file.
    pub warnings: Vec<String>,
}

/// Read the first line of a token file, warning when others can read it.
pub fn read_token_file(path: &Path, warnings: &mut Vec<String>) -> Result<String, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("token file {}: {e}", path.display()))?;
    let token = text.lines().next().unwrap_or("").trim().to_owned();
    if token.is_empty() {
        return Err(format!("token file {} is empty", path.display()));
    }
    if let Some(warning) = readable_by_others(path) {
        warnings.push(warning);
    }
    Ok(token)
}

#[cfg(unix)]
fn readable_by_others(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path).ok()?.permissions().mode();
    (mode & 0o077 != 0).then(|| {
        format!(
            "{} holds a token and is readable by other users (mode {:o}); chmod 600 it",
            path.display(),
            mode & 0o777
        )
    })
}

#[cfg(not(unix))]
fn readable_by_others(_: &Path) -> Option<String> {
    None
}

pub fn load_file(path: &Path) -> Result<Partial, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("config {}: {e}", path.display()))?;
    let file: FileConfig =
        serde_json::from_str(&text).map_err(|e| format!("config {}: {e}", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let resolve = |p: PathBuf| if p.is_absolute() { p } else { dir.join(p) };
    let mut warnings = Vec::new();
    let mut tokens = Vec::new();
    let mut principals = BTreeMap::new();
    let mut inline = false;
    for token in file.tokens {
        let secret = match (&token.token, &token.token_file) {
            (Some(secret), None) => {
                inline = true;
                secret.clone()
            }
            (None, Some(file)) => read_token_file(&resolve(file.clone()), &mut warnings)?,
            _ => {
                return Err(format!(
                    "config {}: token {} needs exactly one of token and token_file",
                    path.display(),
                    token.name
                ))
            }
        };
        if token.tenant.is_some() || !token.scopes.is_empty() || token.repos.is_some() {
            let principal = Principal {
                origin: Default::default(),
                name: token.name.clone(),
                tenant: token.tenant.unwrap_or_else(|| DEFAULT_TENANT.to_owned()),
                scopes: match token.scopes.is_empty() {
                    true => SCOPES.iter().map(|s| s.to_string()).collect(),
                    false => token.scopes.into_iter().collect(),
                },
                repos: token.repos.map(|r| r.into_iter().collect()),
            };
            principals.insert(token.name.clone(), principal);
        }
        tokens.push(Token {
            name: token.name,
            secret,
        });
    }
    let mut credentials = Vec::new();
    for credential in file.credentials {
        let name = credential
            .name
            .unwrap_or_else(|| credential.token_sha256.chars().take(12).collect());
        credentials.push(Credential {
            token_sha256: credential.token_sha256,
            principal: Principal {
                origin: Default::default(),
                name,
                tenant: credential.tenant,
                scopes: match credential.scopes.is_empty() {
                    true => SCOPES.iter().map(|s| s.to_string()).collect(),
                    false => credential.scopes.into_iter().collect(),
                },
                repos: credential.repos.map(|r| r.into_iter().collect()),
            },
        });
    }
    let tenants = file
        .tenants
        .into_iter()
        .map(|(name, p)| {
            (
                name,
                TenantPolicy {
                    repos: p.repos.map(|r| r.into_iter().collect()),
                    max_running: p.max_running,
                    max_branches: p.max_branches,
                    max_cost_usd: p.max_cost_usd,
                    max_artifact_bytes: p.max_artifact_bytes,
                    weight: p.weight,
                    max_priority: p.max_priority,
                },
            )
        })
        .collect();
    let password = file.database.as_deref().is_some_and(|url| {
        url.split_once("://")
            .and_then(|(_, rest)| rest.split_once('@'))
            .is_some_and(|(user, _)| user.contains(':'))
    });
    if inline || password {
        if let Some(warning) = readable_by_others(path) {
            warnings.push(warning);
        }
    }
    let listen = match file.listen {
        Some(text) => Some(parse_listen(&text)?),
        None => None,
    };
    let shutdown_grace = match file.shutdown_grace_seconds {
        Some(s) if s.is_finite() && s >= 0.0 => Some(Duration::from_secs_f64(s)),
        Some(s) => return Err(format!("shutdown_grace_seconds {s} is not usable")),
        None => None,
    };
    let unclaimable_after = match file.unclaimable_after_seconds {
        Some(s) if (0.0..1e9).contains(&s) => Some(Duration::from_secs_f64(s)),
        Some(s) => return Err(format!("unclaimable_after_seconds {s} is not usable")),
        None => None,
    };
    let seconds = |name: &str, value: Option<f64>| match value {
        Some(s) if (0.0..1e9).contains(&s) => Ok(Some(Duration::from_secs_f64(s))),
        Some(s) => Err(format!("{name} {s} is not usable")),
        None => Ok(None),
    };
    let aging = seconds("aging_seconds", file.aging_seconds)?.map(|d| (!d.is_zero()).then_some(d));
    let fair_share_window = seconds("fair_share_window_seconds", file.fair_share_window_seconds)?;
    let sync = match file.sync {
        None => None,
        Some(s) => {
            let lease = match s.lease_seconds {
                Some(v) if (1.0..86_400.0).contains(&v) => Duration::from_secs_f64(v),
                Some(v) => {
                    return Err(format!(
                        "config {}: sync.lease_seconds {v} is not between 1 and 86400",
                        path.display()
                    ))
                }
                None => Duration::from_secs(60),
            };
            let config = branchyard_sync::SyncConfig {
                remote: s.remote,
                encrypt: s.encrypt,
                passphrase_file: s.passphrase_file.map(&resolve),
                algorithm: s.algorithm,
                interval: s.interval,
                bandwidth: s.bandwidth,
                concurrency: s.concurrency,
                retention: s.retention,
                grace: s.grace,
                quota: s.quota,
                device: s.device,
            };
            config
                .check()
                .map_err(|e| format!("config {}: sync: {e}", path.display()))?;
            Some(SyncSettings { config, lease })
        }
    };
    let metrics = match file.metrics {
        None => None,
        Some(m) => {
            let token = match (m.token, m.token_file) {
                (Some(token), None) => {
                    if let Some(warning) = readable_by_others(path) {
                        warnings.push(warning);
                    }
                    Some(token)
                }
                (None, Some(file)) => Some(read_token_file(&resolve(file), &mut warnings)?),
                (None, None) => None,
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "config {}: metrics takes token or token_file, not both",
                        path.display()
                    ))
                }
            };
            if token.as_ref().is_some_and(|t| t.len() < 16) {
                return Err(format!(
                    "config {}: the metrics token is shorter than 16 characters",
                    path.display()
                ));
            }
            Some(MetricsConfig {
                listen: m.listen.as_deref().map(parse_listen).transpose()?,
                token_sha256: token.map(|t| sha256_hex(t.as_bytes())),
            })
        }
    };
    Ok(Partial {
        aging,
        fair_share_window,
        metrics,
        sync,
        listen,
        data_dir: file.data_dir.map(resolve),
        managed_knowledge: ManagedKnowledge {
            root: file.managed_knowledge.root.map(resolve),
            ..file.managed_knowledge
        },
        repos: file
            .repos
            .into_iter()
            .map(|(name, p)| (name, resolve(p)))
            .collect(),
        tokens,
        principals,
        credentials,
        google_identity: file.google_identity,
        tenants,
        tls: file.tls.map(|t| TlsFiles {
            cert: resolve(t.cert),
            key: resolve(t.key),
        }),
        max_body_bytes: file.max_body_bytes,
        max_artifact_bytes: file.max_artifact_bytes,
        max_running: file.max_running,
        shutdown_grace,
        harness_commands: file.harness_commands,
        allow_client_commands: file.allow_client_commands,
        allow_providers: file.allow_providers,
        allow_delegation: file.allow_delegation,
        by_path: file.by_path.map(resolve),
        allow_unapproved_tools: file.allow_unapproved_tools,
        allow_workspace_scripts: scripts(file.allow_workspace_scripts),
        secrets: file
            .secrets
            .into_iter()
            .map(|(name, source)| {
                let secret = parse_secret(&format!("{name}={source}"))
                    .map_err(|e| format!("config {}: {e}", path.display()))?;
                Ok((name, resolve_secret(secret, dir)))
            })
            .collect::<Result<_, String>>()?,
        execution_presets: file
            .execution_presets
            .into_iter()
            .map(|preset| resolve_preset(preset, resolve))
            .collect(),
        require_workspace_execution_presets: file.require_workspace_execution_presets,
        database: file.database,
        labels: file.labels,
        inventory: file.inventory,
        unclaimable_after,
        webhooks: file
            .webhooks
            .into_iter()
            .map(|w| {
                let secret = match (w.secret, w.secret_file) {
                    (Some(secret), None) => secret,
                    (None, Some(file)) => read_token_file(&resolve(file), &mut warnings)?,
                    _ => {
                        return Err(format!(
                            "config {}: webhook {} needs exactly one of secret and secret_file",
                            path.display(),
                            w.url
                        ))
                    }
                };
                for kind in &w.events {
                    check_webhook_event_kind(kind)
                        .map_err(|e| format!("config {}: {e}", path.display()))?;
                }
                Ok(WebhookConfig {
                    id: w.id.unwrap_or_else(|| w.url.clone()),
                    url: w.url,
                    secret,
                    events: w.events.into_iter().collect(),
                })
            })
            .collect::<Result<_, String>>()?,
        webhook_insecure: file.webhook_insecure,
        connectors: file.connectors.map(|c| ConnectorsConfig {
            gateway: c.gateway,
            sandbox_gateway: c.sandbox_gateway,
            issuer: c.issuer,
            signing_key: c.signing_key.map(resolve),
            audit_file: c.audit_file.map(resolve),
            bundles: resolve(c.bundles),
            anvil: c.anvil.unwrap_or_else(|| vec!["anvil".to_owned()]),
            run_gateway: c.run_gateway,
            listen: c.listen,
            vault_key: c.vault_key.map(resolve),
            effects_proxy: c.effects_proxy,
            effects_listen: c.effects_listen,
            effects_sandbox_host: c.effects_sandbox_host,
        }),
        models: match file.models {
            Some(models) => {
                models
                    .check()
                    .map_err(|e| format!("config {}: {e}", path.display()))?;
                Some(models)
            }
            None => None,
        },
        ceilings: file
            .ceilings
            .into_iter()
            .map(|(name, c)| c.ceiling(&name).map(|c| (name, c)))
            .collect::<Result<_, String>>()
            .map_err(|e| format!("config {}: {e}", path.display()))?,
        approvals: file
            .approvals
            .map(FileApprovals::settings)
            .transpose()
            .map_err(|e| format!("config {}: {e}", path.display()))?
            .unwrap_or_default(),
        public_url: file.public_url,
        allow_trigger_prechecks: scripts(file.allow_trigger_prechecks),
        app: file.app.map(|app| match app {
            FileApp::On(enabled) => AppConfig {
                enabled,
                ..AppConfig::default()
            },
            FileApp::Settings(s) => {
                let default = AppConfig::default();
                AppConfig {
                    enabled: s.enabled.unwrap_or(true),
                    push: s.push.unwrap_or(default.push),
                    vapid_key: s.vapid_key.map(&resolve),
                    push_subject: s.push_subject,
                    push_services: s.push_services.unwrap_or(default.push_services),
                }
            }
        }),
        warnings,
    })
}

/// `true`, `false` or repository names, as the operator wrote them.
fn scripts(written: Option<FileWorkspaceScripts>) -> WorkspaceScripts {
    match written {
        None | Some(FileWorkspaceScripts::All(false)) => WorkspaceScripts::Denied,
        Some(FileWorkspaceScripts::All(true)) => WorkspaceScripts::All,
        Some(FileWorkspaceScripts::Repos(repos)) => {
            WorkspaceScripts::Repos(repos.into_iter().collect())
        }
    }
}

/// `NAME`, `NAME=VAR` or `NAME=@FILE`, always with its source: a bare
/// name is the server's variable of that name.
pub fn parse_secret(text: &str) -> Result<branchyard::SecretSource, String> {
    let mut secret = branchyard::SecretSource::parse(text)?;
    secret
        .from
        .get_or_insert_with(|| branchyard::SecretFrom::Env {
            var: secret.name.clone(),
        });
    Ok(secret)
}

/// A secret's relative file, against `dir`.
pub fn resolve_secret(
    mut secret: branchyard::SecretSource,
    dir: &Path,
) -> branchyard::SecretSource {
    if let Some(branchyard::SecretFrom::File { path }) = &mut secret.from {
        if path.is_relative() {
            *path = dir.join(&*path);
        }
    }
    secret
}

pub fn parse_listen(text: &str) -> Result<SocketAddr, String> {
    text.parse()
        .map_err(|_| format!("{text:?} is not an address such as 127.0.0.1:8421 or [::1]:8421"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(listen: &str) -> Config {
        let mut c = Config::new(PathBuf::from("/nonexistent/x"));
        c.listen = listen.parse().unwrap();
        c.repos.push(("app".into(), PathBuf::from("/srv/app")));
        c.tokens.push(Token {
            name: "t".into(),
            secret: "0123456789abcdef".into(),
        });
        c
    }

    #[test]
    fn non_loopback_needs_tls_or_the_flag() {
        assert_eq!(config("127.0.0.1:0").validate(), Ok(None));
        assert_eq!(config("[::1]:0").validate(), Ok(None));
        let open = config("0.0.0.0:8421");
        assert!(open.validate().unwrap_err().contains("--insecure-bind"));
        let mut tls = open.clone();
        tls.tls = Some(TlsFiles {
            cert: "c".into(),
            key: "k".into(),
        });
        assert_eq!(tls.validate(), Ok(None));
        let mut insecure = open;
        insecure.insecure_bind = true;
        assert!(insecure.validate().unwrap().unwrap().starts_with("WARNING"));
    }

    #[test]
    fn names_tokens_and_repos_are_checked() {
        let mut c = config("127.0.0.1:0");
        c.tokens[0].secret = "short".into();
        assert!(c.validate().unwrap_err().contains("shorter than 16"));
        let mut c = config("127.0.0.1:0");
        c.repos.push(("app".into(), PathBuf::from("/other")));
        assert!(c.validate().unwrap_err().contains("twice"));
        assert!(check_repo_name("my-app.2").is_ok());
        assert!(check_repo_name("My").is_err());
        assert!(check_repo_name("-x").is_err());
        assert_eq!(repo_name_for(Path::new("/src/My App")), "my-app");
        assert_eq!(repo_name_for(Path::new("/")), "repo");
        let debug = format!("{:?}", config("127.0.0.1:0").tokens[0]);
        assert!(!debug.contains("0123456789abcdef"), "{debug}");
    }

    fn hook(id: &str, url: &str) -> WebhookConfig {
        WebhookConfig {
            id: id.into(),
            url: url.into(),
            secret: "0123456789abcdef".into(),
            events: BTreeSet::new(),
        }
    }

    #[test]
    fn webhook_urls_need_https_unless_loopback_or_insecure() {
        assert!(check_webhook_url("https://example.com/hook", false).is_ok());
        assert!(check_webhook_url("http://127.0.0.1:9/hook", false).is_ok());
        assert!(check_webhook_url("http://localhost/hook", false).is_ok());
        let err = check_webhook_url("http://example.com/hook", false).unwrap_err();
        assert!(err.contains("--webhook-insecure"), "{err}");
        assert!(check_webhook_url("http://example.com/hook", true).is_ok());
        assert!(check_webhook_url("ftp://example.com/hook", false).is_err());
        assert!(check_webhook_event_kind("stall").is_ok());
        assert!(check_webhook_event_kind("bogus").is_err());
    }

    #[test]
    fn one_secret_is_one_entry_across_tokens_and_credentials() {
        // Two plaintext tokens with the same secret.
        let mut c = config("127.0.0.1:0");
        c.tokens.push(Token {
            name: "u".into(),
            secret: "0123456789abcdef".into(),
        });
        let err = c.validate().unwrap_err();
        assert!(err.contains("tokens t and u have the same secret"), "{err}");
        assert!(!err.contains("0123456789abcdef"), "{err}");
        // A token's secret again as a hashed credential for another tenant.
        let mut c = config("127.0.0.1:0");
        let mut other = Principal::default_for("x");
        other.tenant = "other".into();
        c.credentials.push(Credential {
            token_sha256: sha256_hex(b"0123456789abcdef"),
            principal: other,
        });
        let err = c.validate().unwrap_err();
        assert!(err.contains("also token t's secret"), "{err}");
    }

    #[test]
    fn webhooks_are_validated_and_refused_when_duplicated_or_weak() {
        let mut c = config("127.0.0.1:0");
        c.webhooks.push(hook("h", "https://example.com/a"));
        assert_eq!(c.validate(), Ok(None));
        c.webhooks.push(hook("h", "https://example.com/b"));
        assert!(c.validate().unwrap_err().contains("configured twice"));
        let mut c = config("127.0.0.1:0");
        c.webhooks.push(hook("h", "http://example.com/a"));
        assert!(c.validate().unwrap_err().contains("--webhook-insecure"));
        let mut c = config("127.0.0.1:0");
        let mut weak = hook("h", "https://example.com/a");
        weak.secret = "short".into();
        c.webhooks.push(weak);
        assert!(c.validate().unwrap_err().contains("shorter than 16"));
    }

    #[test]
    fn the_model_gateway_and_ceilings_load_and_are_checked() {
        let temp = tempfile::Builder::new()
            .prefix("branchyard-config-")
            .tempdir()
            .unwrap();
        let dir = temp.path();
        fs::write(dir.join("t.token"), "0123456789abcdef\n").unwrap();
        let path = dir.join("server.json");
        let write = |models: &str, ceilings: &str| {
            fs::write(
                &path,
                format!(
                    r#"{{"listen": "127.0.0.1:0", "data_dir": "data", "repos": {{"app": "repo"}},
                    "tokens": [{{"name": "ci", "token_file": "t.token"}}],
                    "secrets": {{"anthropic": "@anthropic.key"}},
                    "models": {models}, "ceilings": {ceilings}}}"#
                ),
            )
            .unwrap();
        };
        write(
            r#"{"backends": {"anthropic": {"api": "anthropic", "key": "anthropic"}},
                "routes": [{"model": "claude-*", "backends": ["anthropic"]}],
                "budget": {"monthly_usd": 100}, "audit": "contents"}"#,
            r#"{"ci": {"models": ["claude-haiku-*"], "network": ["api.github.com:443"],
                       "connectors": ["github:read"]}}"#,
        );
        let partial = load_file(&path).unwrap();
        let models = partial.models.clone().unwrap();
        assert_eq!(
            models.backends["anthropic"].key.as_deref(),
            Some("anthropic")
        );
        let ceiling = &partial.ceilings["ci"];
        assert_eq!(ceiling.models.as_ref().unwrap().allow, ["claude-haiku-*"]);
        assert_eq!(
            ceiling.network.as_ref().unwrap().rules(),
            ["api.github.com:443"]
        );
        assert_eq!(
            ceiling.connectors.as_ref().unwrap()[0].to_string(),
            "github:read"
        );
        // The repository's gateway: the key from the server's secrets,
        // branches named under the repository.
        let mut config = Config::new(dir.join("data"));
        config.secrets = partial.secrets.clone();
        config.models = partial.models.clone();
        let gateway = crate::models::gateway_for(&config, "app").unwrap().unwrap();
        assert_eq!(gateway.branch_scope.as_deref(), Some("app"));
        assert_eq!(
            gateway.backends[0].key,
            Some(branchyard::models::KeySource::File(
                dir.join("anthropic.key")
            ))
        );
        assert_eq!(gateway.budget.monthly_usd, Some(100.0));
        assert_eq!(gateway.audit, branchyard::models::Audit::Contents);
        config.models = None;
        assert!(crate::models::gateway_for(&config, "app")
            .unwrap()
            .is_none());
        // Refused by name.
        for (models, ceilings, needle) in [
            (
                r#"{"routes": [{"model": "*", "backends": ["x"]}]}"#,
                "{}",
                "no backend x",
            ),
            (
                r#"{"backends": {"a": {"api": "x"}}}"#,
                "{}",
                "models.backends.a.api",
            ),
            ("{}", r#"{"ci": {"models": ["a b"]}}"#, "ceilings.ci"),
            ("{}", r#"{"ci": {"network": ["https://x"]}}"#, "ceilings.ci"),
            ("{}", r#"{"ci": {"budget": 1}}"#, "budget"),
            (r#"{"audit": "all"}"#, "{}", "unknown variant `all`"),
        ] {
            write(models, ceilings);
            let error = load_file(&path).unwrap_err();
            assert!(error.contains(needle), "{models} {ceilings}: {error}");
        }
    }

    #[test]
    fn approvals_and_the_ledger_proxy_load_and_are_checked() {
        use branchyard::effects::{Approval, EffectClass};
        let temp = tempfile::Builder::new()
            .prefix("branchyard-config-")
            .tempdir()
            .unwrap();
        let dir = temp.path();
        fs::write(dir.join("t.token"), "0123456789abcdef\n").unwrap();
        let path = dir.join("server.json");
        let write = |approvals: &str| {
            fs::write(
                &path,
                format!(
                    r#"{{"listen": "127.0.0.1:0", "data_dir": "data", "repos": {{"app": "repo"}},
                    "tokens": [{{"name": "ci", "token_file": "t.token"}}],
                    "connectors": {{"gateway": "http://127.0.0.1:8931/mcp", "bundles": "b",
                                    "effects_proxy": true, "effects_listen": "0.0.0.0",
                                    "effects_sandbox_host": "192.168.127.1"}},
                    "approvals": {approvals}}}"#
                ),
            )
            .unwrap();
        };
        write(
            r#"{"admin": {"rules": {"gmail:*": "ask", "Bash": "block"},
                          "classes": {"compensable": "ask"}, "deletion": "ask"},
                "people": {"ci": {"rules": {"github:*": "allow"}}}}"#,
        );
        let partial = load_file(&path).unwrap();
        let admin = partial.approvals.admin.clone().unwrap();
        assert_eq!(admin.rules["gmail:*"], Approval::Ask);
        assert_eq!(admin.classes[&EffectClass::Compensable], Approval::Ask);
        assert_eq!(admin.deletion, Some(Approval::Ask));
        assert_eq!(
            partial.approvals.people["ci"].rules["github:*"],
            Approval::Allow
        );
        let connectors = partial.connectors.unwrap();
        assert_eq!(connectors.effects_proxy, Some(true));
        assert_eq!(connectors.effects_listen, Some("0.0.0.0".parse().unwrap()));
        assert_eq!(
            connectors.effects_sandbox_host.as_deref(),
            Some("192.168.127.1")
        );
        for (approvals, needle) in [
            (
                r#"{"admin": {"rules": {"x:y": "maybe"}}}"#,
                "approvals.admin",
            ),
            (
                r#"{"people": {"ci": {"classes": {"loud": "ask"}}}}"#,
                "approvals.people.ci",
            ),
            (
                r#"{"admin": {"rules": {"github:": "ask"}}}"#,
                "approval pattern",
            ),
            (r#"{"admin": {"lock": true}}"#, "lock"),
        ] {
            write(approvals);
            let error = load_file(&path).unwrap_err();
            assert!(error.contains(needle), "{approvals}: {error}");
        }
    }

    #[test]
    fn files_resolve_relative_paths_and_reject_unknown_keys() {
        let temp = tempfile::Builder::new()
            .prefix("branchyard-config-")
            .tempdir()
            .unwrap();
        let dir = temp.path();
        fs::write(dir.join("t.token"), "0123456789abcdef\n").unwrap();
        let path = dir.join("server.json");
        fs::write(
            &path,
            r#"{"listen": "127.0.0.1:0", "data_dir": "data", "repos": {"app": "repo"},
                "tokens": [{"name": "ci", "token_file": "t.token"}],
                "harness_commands": {"codex": ["/bin/codex"]},
                "secrets": {"ANTHROPIC_API_KEY": "ANTHROPIC_API_KEY", "CODEX_AUTH": "@codex.json"},
                "connectors": {"gateway": "http://127.0.0.1:8931/mcp", "bundles": "bundles",
                               "signing_key": "gw.key", "run_gateway": true}}"#,
        )
        .unwrap();
        let partial = load_file(&path).unwrap();
        let connectors = partial.connectors.clone().unwrap();
        assert_eq!(connectors.bundles, dir.join("bundles"));
        assert_eq!(connectors.signing_key, Some(dir.join("gw.key")));
        assert_eq!(connectors.anvil, ["anvil"]);
        assert!(connectors.run_gateway);
        assert_eq!(partial.data_dir, Some(dir.join("data")));
        assert_eq!(partial.repos, [("app".to_owned(), dir.join("repo"))]);
        assert_eq!(partial.tokens[0].secret, "0123456789abcdef");
        assert_eq!(
            partial.secrets["CODEX_AUTH"].from,
            Some(branchyard::SecretFrom::File {
                path: dir.join("codex.json")
            })
        );
        assert_eq!(
            partial.secrets["ANTHROPIC_API_KEY"].from,
            Some(branchyard::SecretFrom::Env {
                var: "ANTHROPIC_API_KEY".into()
            })
        );
        fs::write(&path, r#"{"listen": "127.0.0.1:0", "lisen": 1}"#).unwrap();
        assert!(load_file(&path).unwrap_err().contains("lisen"));
    }
}

#[cfg(test)]
mod managed_knowledge_tests {
    use super::{Config, Token};
    #[test]
    fn knowledge_only_configuration_requires_valid_operator_storage_and_quotas() {
        let mut config = Config::new(std::path::PathBuf::from("/operator/data"));
        config.tokens.push(Token {
            name: "test".into(),
            secret: "managed-configuration-test-token".into(),
        });
        assert!(config.validate().is_err());
        config.managed_knowledge.enabled = true;
        assert!(config.validate().is_ok());
        config.managed_knowledge.root = Some("relative".into());
        assert!(config.validate().unwrap_err().contains("absolute"));
        config.managed_knowledge.root = Some("/operator/knowledge".into());
        for field in ["workspaces", "bytes", "upload"] {
            let mut invalid = config.clone();
            match field {
                "workspaces" => invalid.managed_knowledge.max_workspaces = 0,
                "bytes" => invalid.managed_knowledge.max_source_bytes = 0,
                _ => invalid.managed_knowledge.max_upload_bytes = 0,
            }
            assert!(invalid.validate().unwrap_err().contains("positive"));
        }
    }
}

#[cfg(test)]
mod execution_presets_tests {
    use super::*;
    use serde_json::json;

    fn preset(value: serde_json::Value) -> ExecutionPreset {
        serde_json::from_value(value).unwrap()
    }

    /// A knowledge server with a local and a Substrate preset that validate.
    fn config() -> Config {
        let mut config = Config::new(PathBuf::from("/operator/data"));
        config.managed_knowledge.enabled = true;
        config.tokens.push(Token {
            name: "operator".into(),
            secret: "execution-preset-test-token".into(),
        });
        config.allow_providers.insert("substrate".into());
        config.secrets.insert(
            "GEMINI_API_KEY".into(),
            parse_secret("GEMINI_API_KEY").unwrap(),
        );
        config.execution_presets = vec![
            preset(
                json!({"id":"local","label":"This server","provider":{"kind":"local"},
                "harnesses":[{"harness":"antigravity","models":["gemini-3.8-flash-low"]},{"harness":"claude-code"}],
                "secrets":["GEMINI_API_KEY"]}),
            ),
            preset(json!({"id":"substrate-gke","label":"Substrate on GKE",
                "provider":{"kind":"substrate","endpoint":"https://control.internal.example",
                    "router":"https://router.internal.example","template":"harnesses",
                    "key":"/operator/substrate/bridge.key"},
                "harnesses":[{"harness":"antigravity"}],"tenants":["research"],
                "require_labels":["substrate-gke"]})),
        ];
        config
    }

    #[test]
    fn a_configuration_file_resolves_preset_paths_and_check_merges_the_flags() {
        let scratch = branchyard_testkit::Scratch::new("execution-presets-config");
        let dir = scratch.path();
        let file = dir.join("server.json");
        let settings = json!({
            "data_dir": "state",
            "managed_knowledge": {"enabled": true},
            "tokens": [{"name": "operator", "token": "execution-preset-test-token"}],
            "require_workspace_execution_presets": true,
            "execution_presets": [
                {"id": "local", "label": "This server", "provider": {"kind": "local"},
                 "harnesses": [{"harness": "claude-code"}]},
                {"id": "substrate-gke", "label": "Substrate on GKE",
                 "provider": {"kind": "substrate", "endpoint": "https://control.internal.example",
                    "router": "https://router.internal.example", "template": "harnesses",
                    "key": "substrate/bridge.key", "api_token_file": "substrate/api.token",
                    "ca": "substrate/ca.pem"},
                 "harnesses": [{"harness": "antigravity", "models": ["gemini-3.8-flash-low"]}],
                 "secrets": ["GEMINI_API_KEY"]}
            ]
        });
        fs::write(&file, settings.to_string()).unwrap();
        let args = |extra: &[&str]| {
            let mut args = vec!["--config".to_owned(), file.display().to_string()];
            args.extend(extra.iter().map(|arg| (*arg).to_owned()));
            args
        };
        assert!(crate::cli::check(&args(&[])).unwrap_err().contains(
            "execution preset substrate-gke: its provider substrate is not in allow_providers"
        ));
        assert!(crate::cli::check(&args(&["--allow-provider", "substrate"]))
            .unwrap_err()
            .contains("secret GEMINI_API_KEY is not in secrets"));
        let flags = [
            "--allow-provider",
            "substrate",
            "--secret",
            "GEMINI_API_KEY",
        ];
        crate::cli::check(&args(&flags)).unwrap();
        let config = crate::cli::resolve(&args(&flags)).unwrap();
        assert!(config.require_workspace_execution_presets);
        let [local, remote] = config.execution_presets.as_slice() else {
            panic!("two presets: {:?}", config.execution_presets);
        };
        assert!(local.written.is_none());
        let resolved = substrate_options(&remote.provider).unwrap();
        assert_eq!(resolved.key, dir.join("substrate/bridge.key"));
        assert_eq!(
            resolved.api_token_file,
            Some(dir.join("substrate/api.token"))
        );
        assert_eq!(resolved.ca, Some(dir.join("substrate/ca.pem")));
        let written = substrate_options(remote.written.as_ref().unwrap()).unwrap();
        assert_eq!(written.key, PathBuf::from("substrate/bridge.key"));
        assert_eq!(
            remote.harnesses[0].default_model(),
            Some("gemini-3.8-flash-low")
        );
        assert_eq!(local.harnesses[0].default_model(), None);
        fs::write(
            &file,
            json!({"execution_presets": [], "presets": []}).to_string(),
        )
        .unwrap();
        assert!(load_file(&file)
            .unwrap_err()
            .contains("unknown field `presets`"));
    }

    #[test]
    fn each_unusable_preset_is_refused_naming_what_to_change() {
        assert_eq!(config().validate(), Ok(None));
        type Change = fn(&mut Config);
        let cases: [(Change, &str); 15] = [
            (
                |c| {
                    c.execution_presets.clear();
                    c.require_workspace_execution_presets = true;
                },
                "require_workspace_execution_presets needs at least one execution_presets entry",
            ),
            (|c| c.execution_presets[0].id = "This_Server".into(), "is not an ID"),
            (
                |c| c.execution_presets[1].id = "local".into(),
                "execution preset local is configured twice",
            ),
            (|c| c.execution_presets[0].label = " ".into(), "local: label is empty"),
            (
                |c| c.allow_providers.clear(),
                "its provider substrate is not in allow_providers; allow it with --allow-provider substrate",
            ),
            (
                |c| {
                    c.execution_presets[0].provider = serde_json::from_value(
                        json!({"kind":"recipe","name":"vm","create":"make-vm"}),
                    )
                    .unwrap();
                },
                "never runs environment recipes",
            ),
            (
                |c| {
                    let mut options = substrate_options(&c.execution_presets[1].provider).unwrap();
                    options.key = "bridge.key".into();
                    c.execution_presets[1].provider = branchyard::Provider::substrate(options);
                },
                "bridge.key must be an absolute path",
            ),
            (|c| c.execution_presets[1].harnesses.clear(), "lists no harnesses"),
            (
                |c| c.execution_presets[1].harnesses[0].harness = "no-such-harness".into(),
                "\"no-such-harness\" is not a harness or profile",
            ),
            (
                |c| {
                    let again = c.execution_presets[0].harnesses[0].clone();
                    c.execution_presets[0].harnesses.push(again);
                },
                "lists harness antigravity twice",
            ),
            (
                |c| c.execution_presets[0].harnesses[0].models = Some(Vec::new()),
                "models must name models",
            ),
            (
                |c| c.execution_presets[0].harnesses[0].default_model = Some("other".into()),
                "default_model other is not among its models",
            ),
            (|c| c.secrets.clear(), "secret GEMINI_API_KEY is not in secrets"),
            (
                |c| c.execution_presets[1].tenants = Some(Vec::new()),
                "tenants is empty; omit it for every tenant",
            ),
            (
                |c| c.execution_presets[1].require_labels = vec!["Not A Label".into()],
                "\"Not A Label\" is not a worker label",
            ),
        ];
        for (change, expected) in cases {
            let mut config = config();
            change(&mut config);
            let error = config.validate().unwrap_err();
            assert!(error.contains(expected), "{expected:?} in {error:?}");
        }
    }
}
