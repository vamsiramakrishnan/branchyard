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

fn check_principal(principal: &Principal) -> Result<(), String> {
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

/// Who a verified bearer token acts as: a tenant, a subject name, the
/// scopes it holds, and, optionally, a narrower repository allowlist than
/// its tenant's. Requests never carry a `tenant_id`; a principal's tenant
/// comes only from the credential that authenticated it. See
/// `docs/server.md#identity-and-scopes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
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
    /// Every scope, every repository, in [`DEFAULT_TENANT`]: what a
    /// `tokens` entry becomes when it names no tenant or scopes of its own,
    /// keeping single-token configurations working unchanged.
    pub fn default_for(name: &str) -> Principal {
        Principal {
            name: name.to_owned(),
            tenant: DEFAULT_TENANT.to_owned(),
            scopes: SCOPES.iter().map(|s| s.to_string()).collect(),
            repos: None,
        }
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

/// A resolved configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    /// Name and path of each served repository.
    pub repos: Vec<(String, PathBuf)>,
    pub tokens: Vec<Token>,
    /// A `tokens` entry's principal (tenant, scopes, repository
    /// allowlist), keyed by its `name`. An entry missing here gets
    /// [`Principal::default_for`]: every scope, every repository, in
    /// [`DEFAULT_TENANT`] — unchanged single-token behavior.
    pub principals: BTreeMap<String, Principal>,
    /// Credentials naming their principal by a token hash directly,
    /// alongside `tokens`.
    pub credentials: Vec<Credential>,
    /// Each tenant's resource ceilings and repositories, keyed by tenant
    /// name. A tenant a principal names but that has no entry here has no
    /// configured limits (unlimited) and, by default, every repository.
    pub tenants: BTreeMap<String, TenantPolicy>,
    pub tls: Option<TlsFiles>,
    /// Serve plain HTTP on a non-loopback address. Only from the flag.
    pub insecure_bind: bool,
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
    /// Secrets a request may name, and where this server reads each: a
    /// variable of its own environment or a file. A request names secrets
    /// only; it never chooses a source. Empty by default.
    pub secrets: BTreeMap<String, branchyard::SecretSource>,
    /// A PostgreSQL URL: keep branch state and operations there instead of
    /// SQLite. Only in a build with the `postgres` feature.
    pub database: Option<String>,
    /// How often the feed looks in the repository's store for activity
    /// recorded by other processes.
    pub poll_interval: Duration,
    /// Log one line per request to stderr.
    pub log_requests: bool,
    /// Webhook targets notified of every served repository's activity feed.
    pub webhooks: Vec<WebhookConfig>,
    /// Allow a webhook's `http://` URL off loopback. Only from the flag.
    pub webhook_insecure: bool,
}

impl Config {
    /// Defaults for everything but repositories, tokens and data directory.
    pub fn new(data_dir: PathBuf) -> Config {
        Config {
            listen: DEFAULT_LISTEN.parse().expect("valid default address"),
            data_dir,
            repos: Vec::new(),
            tokens: Vec::new(),
            principals: BTreeMap::new(),
            credentials: Vec::new(),
            tenants: BTreeMap::new(),
            tls: None,
            insecure_bind: false,
            max_body_bytes: 1024 * 1024,
            max_artifact_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
            max_running: 8,
            operation_lease: crate::ops::DEFAULT_LEASE,
            worker_only: false,
            shutdown_grace: Duration::from_secs(60),
            harness_commands: BTreeMap::new(),
            allow_client_commands: false,
            allow_providers: BTreeSet::new(),
            allow_delegation: false,
            by_path: None,
            allow_unapproved_tools: false,
            secrets: BTreeMap::new(),
            database: None,
            poll_interval: Duration::from_millis(500),
            log_requests: true,
            webhooks: Vec::new(),
            webhook_insecure: false,
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
        if self.repos.is_empty() {
            return Err("no repositories to serve".into());
        }
        if self.tokens.is_empty() && self.credentials.is_empty() && !self.worker_only {
            return Err("no tokens or credentials configured; every request needs one".into());
        }
        for (name, _) in &self.repos {
            check_repo_name(name)?;
        }
        let mut names: Vec<&str> = self.repos.iter().map(|(n, _)| n.as_str()).collect();
        names.sort_unstable();
        if let Some(pair) = names.windows(2).find(|w| w[0] == w[1]) {
            return Err(format!("repository {} is configured twice", pair[0]));
        }
        for token in &self.tokens {
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
        }
        let mut hashes = BTreeSet::new();
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
            if !hashes.insert(hash.clone()) {
                return Err(format!("credential {hash} is configured twice"));
            }
            check_principal(&credential.principal)?;
        }
        for name in self.tenants.keys() {
            check_tenant_name(name)?;
        }
        for provider in &self.allow_providers {
            check_provider_name(provider)?;
        }
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
        if self.max_body_bytes < 1024 {
            return Err("max_body_bytes must be at least 1024".into());
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
}

/// The providers a server can allow requests to name.
pub const PROVIDERS: &[&str] = &["local", "microsandbox", "substrate"];

pub fn check_provider_name(name: &str) -> Result<(), String> {
    match PROVIDERS.contains(&name) {
        true => Ok(()),
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
struct FileConfig {
    listen: Option<String>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    repos: BTreeMap<String, PathBuf>,
    #[serde(default)]
    tokens: Vec<FileToken>,
    #[serde(default)]
    credentials: Vec<FileCredential>,
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
    #[serde(default)]
    secrets: BTreeMap<String, String>,
    database: Option<String>,
    #[serde(default)]
    webhooks: Vec<FileWebhook>,
    #[serde(default)]
    webhook_insecure: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileWebhook {
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
struct FileToken {
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
struct FileCredential {
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
struct FileTenantPolicy {
    #[serde(default)]
    repos: Option<Vec<String>>,
    max_running: Option<usize>,
    max_branches: Option<usize>,
    max_cost_usd: Option<f64>,
    max_artifact_bytes: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileTls {
    cert: PathBuf,
    key: PathBuf,
}

/// Settings from a file, before flags and defaults.
#[derive(Debug, Default)]
pub struct Partial {
    pub listen: Option<SocketAddr>,
    pub data_dir: Option<PathBuf>,
    pub repos: Vec<(String, PathBuf)>,
    pub tokens: Vec<Token>,
    pub principals: BTreeMap<String, Principal>,
    pub credentials: Vec<Credential>,
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
    pub secrets: BTreeMap<String, branchyard::SecretSource>,
    pub database: Option<String>,
    pub webhooks: Vec<WebhookConfig>,
    pub webhook_insecure: bool,
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
            (Some(_), None) => {
                inline = true;
                token.token.unwrap()
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
    Ok(Partial {
        listen,
        data_dir: file.data_dir.map(resolve),
        repos: file
            .repos
            .into_iter()
            .map(|(name, p)| (name, resolve(p)))
            .collect(),
        tokens,
        principals,
        credentials,
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
        secrets: file
            .secrets
            .into_iter()
            .map(|(name, source)| {
                let secret = parse_secret(&format!("{name}={source}"))
                    .map_err(|e| format!("config {}: {e}", path.display()))?;
                Ok((name, resolve_secret(secret, dir)))
            })
            .collect::<Result<_, String>>()?,
        database: file.database,
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
        warnings,
    })
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
        let mut c = Config::new(PathBuf::from("/tmp/x"));
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
    fn files_resolve_relative_paths_and_reject_unknown_keys() {
        let dir = std::env::temp_dir().join(format!("branchyard-config-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("t.token"), "0123456789abcdef\n").unwrap();
        let path = dir.join("server.json");
        fs::write(
            &path,
            r#"{"listen": "127.0.0.1:0", "data_dir": "data", "repos": {"app": "repo"},
                "tokens": [{"name": "ci", "token_file": "t.token"}],
                "harness_commands": {"codex": ["/bin/codex"]},
                "secrets": {"ANTHROPIC_API_KEY": "ANTHROPIC_API_KEY", "CODEX_AUTH": "@codex.json"}}"#,
        )
        .unwrap();
        let partial = load_file(&path).unwrap();
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
        let _ = fs::remove_dir_all(dir);
    }
}
