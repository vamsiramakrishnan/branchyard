//! The model gateway: a harness's model traffic through Branchyard, on the
//! turn's token, with the upstream key held here (`docs/model-gateway.md`).
//!
//! A branch whose provisioning has [`ModelAccess`] (`--model-gateway`)
//! gets, for each turn, a gateway on a port of its own: an HTTP reverse
//! proxy speaking the providers' own APIs, so a harness works unmodified
//! once its base URL points at it. The harness is given the turn's token
//! as its API key (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) and the
//! gateway's address as its base URL (`ANTHROPIC_BASE_URL`,
//! `OPENAI_BASE_URL`); the provider's key never leaves this process.
//!
//! For each request the gateway checks the token (signature, expiry, the
//! turn it was minted for) and its `by_models` scope, picks a backend by
//! the yard's routes (weighted, then in order, then the route's fallbacks;
//! the next on a refused connection, a 5xx or a 429), holds per-route rate
//! limits, the branch's budget and the yard's daily and monthly budgets,
//! forwards the request with the backend's key, streams the response back
//! as it arrives, and records one [`UsageRecord`] (tokens from the
//! provider's own usage fields, streamed or not; cost from
//! `catalog/pricing.toml` or `[models.prices]`) in the store and one
//! [`ModelActivity::Call`] on the branch. The turn's metered cost is the
//! branch's cost.
//!
//! The gateway lives as long as the turn, in the engine's process (as the
//! egress proxy does), so it runs wherever the turn runs: under `by run`,
//! `by serve` and a worker alike, with nothing to supervise.
//!
//! A harness logged in by subscription (Claude Code's or Codex's OAuth
//! login) cannot take an injected key: such a turn, or one whose harness
//! speaks an API no backend serves, runs **direct**, its provider's hosts
//! added to its egress policy, and says so ([`ModelActivity::Direct`]).

pub(crate) mod gateway;
pub mod pricing;
pub(crate) mod upstream;

/// A backend's or server's base URL, parsed: scheme, host, port and path
/// prefix. The one parser for them; `branchyard_client::http::Endpoint`
/// builds on it.
pub use upstream::Target as BaseUrl;
mod usage;

use branchyard_support::LockExt as _;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use branchyard_provision::models::{narrow, ModelAccess};
use branchyard_provision::network::HostRule;
use serde::{Deserialize, Serialize};

use crate::access::TokenScopes;
use crate::connectors::{keys, Actor};
use crate::state::Record;
use crate::{Error, Yard};

pub use gateway::TurnGateway;
pub use usage::Tokens;

/// The API a backend speaks, and a request's path family at the gateway.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Api {
    /// Anthropic's Messages API (`/v1/messages`), with `x-api-key`.
    Anthropic,
    /// OpenAI's API (`/v1/chat/completions`, `/v1/responses` and the
    /// rest), with `Authorization: Bearer`.
    Openai,
    /// Anything else, passed through with the backend's key in its header.
    Generic,
}

impl Api {
    pub fn as_str(&self) -> &'static str {
        match self {
            Api::Anthropic => "anthropic",
            Api::Openai => "openai",
            Api::Generic => "generic",
        }
    }

    /// The provider's public API, for a backend that names no URL.
    fn default_url(&self) -> Option<&'static str> {
        match self {
            Api::Anthropic => Some("https://api.anthropic.com"),
            Api::Openai => Some("https://api.openai.com"),
            Api::Generic => None,
        }
    }
}

impl fmt::Display for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a backend's key comes from. Read when a turn's gateway starts,
/// so a rotated key is picked up by the next turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// An environment variable of the process that runs the turn.
    Env(String),
    /// A file's contents, trimmed.
    File(PathBuf),
}

impl KeySource {
    /// The key, or why it is missing.
    pub fn read(&self) -> Result<String, String> {
        let value = match self {
            KeySource::Env(var) => {
                std::env::var(var).map_err(|_| format!("the variable {var} is not set"))?
            }
            KeySource::File(path) => {
                std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?
            }
        };
        let value = value.trim().to_owned();
        match value.is_empty() {
            true => Err(format!("{self} is empty")),
            false => Ok(value),
        }
    }
}

impl fmt::Display for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeySource::Env(var) => write!(f, "${var}"),
            KeySource::File(path) => write!(f, "@{}", path.display()),
        }
    }
}

/// `[models]`, as the engine takes it: what `branchyard.toml` or a
/// server's configuration file says. Keys are named, never given.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Models a new branch may call through the gateway when
    /// `--model-gateway` gives none; set, it puts new branches on the
    /// gateway. Read by `by`, not the engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// Backends by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub backends: BTreeMap<String, BackendConfig>,
    /// Routes, first match wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<RouteConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetConfig>,
    /// Prices for models the catalog does not price, dollars per million
    /// tokens, by model id or glob.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prices: BTreeMap<String, Price>,
    /// The address a turn's gateway listens on (default `127.0.0.1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// The host a sandboxed harness reaches the gateway at; without it, a
    /// sandboxed branch on the gateway fails its turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_host: Option<String>,
    /// Seeds the weighted choice of backends, for a reproducible order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

/// One backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub api: Api,
    /// Its base URL (default: the provider's public API).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The secret holding its key, by name (`[secrets]`, else a variable
    /// of that name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// For a generic backend, the header that carries the key (default
    /// `authorization`, as `Bearer <key>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
}

/// One route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// A glob over the requested model.
    pub model: String,
    /// Backends chosen among by weight.
    pub backends: Vec<String>,
    /// One weight per backend (default: all 1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weights: Vec<u32>,
    /// Tried in order after the weighted backends.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<String>,
    /// At most this many requests a minute through this route, in this
    /// process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_minute: Option<u32>,
}

/// Daily and monthly limits (UTC), over every branch of the yard.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly_tokens: Option<u64>,
    /// The share of a limit at which an alert is recorded (default 0.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_at: Option<f64>,
}

/// Dollars per million tokens.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

/// A backend, resolved.
#[derive(Clone, Debug)]
pub struct Backend {
    pub name: String,
    pub api: Api,
    pub url: String,
    pub key: Option<KeySource>,
    pub header: Option<String>,
}

/// A route, resolved.
#[derive(Clone, Debug)]
pub struct Route {
    pub model: String,
    /// Backend names and weights.
    pub backends: Vec<(String, u32)>,
    pub fallbacks: Vec<String>,
    pub requests_per_minute: Option<u32>,
}

/// Who signs the turns' tokens: the connector gateway's keys and issuer
/// when the yard has one, so one token serves both.
#[derive(Clone, Debug)]
pub struct Signer {
    pub issuer: String,
    pub key_file: PathBuf,
    pub jwks_file: Option<PathBuf>,
    /// The subject and tenant of a branch that records none.
    pub subject: String,
    pub tenant: String,
}

impl Signer {
    /// A local yard's: `.branchyard/gateway/key`, issuer
    /// `branchyard:local:<yard id>`, subject `local:<os user>`; what the
    /// connector gateway uses too.
    pub fn local(yard: &Yard) -> Result<Signer, Error> {
        let dir = crate::connectors::local_dir(yard.root());
        Ok(Signer {
            issuer: crate::connectors::local_issuer(yard.root())?,
            key_file: dir.join("key"),
            jwks_file: Some(dir.join("jwks.json")),
            subject: crate::connectors::local_subject(),
            tenant: "local".to_owned(),
        })
    }

    fn keys(&self) -> Result<keys::KeyRing, Error> {
        keys::KeyRing::load_or_create(&self.key_file, self.jwks_file.as_deref())
    }
}

/// A yard's model gateway: its backends, routes and budgets, and the state
/// its turns share in this process (rate windows, the weighted choice).
/// Set with [`Yard::use_models`].
pub struct Gateway {
    pub backends: Vec<Backend>,
    pub routes: Vec<Route>,
    pub budget: BudgetConfig,
    pub prices: BTreeMap<String, Price>,
    pub listen: IpAddr,
    pub sandbox_host: Option<String>,
    pub signer: Signer,
    /// Prefixed to the branch in usage records and tokens, as the
    /// connector gateway's `by_branch` is, on a server.
    pub branch_scope: Option<String>,
    state: Mutex<Shared>,
}

impl fmt::Debug for Gateway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gateway")
            .field("backends", &self.backends)
            .field("routes", &self.routes)
            .finish_non_exhaustive()
    }
}

/// What a yard's turns share.
struct Shared {
    /// Request times (ms) in the last minute, by route.
    windows: HashMap<usize, VecDeque<u64>>,
    /// The weighted choice's generator.
    rng: branchyard_support::rng::SplitMix64,
    /// Budget alerts already recorded, by period and its start.
    alerted: Vec<(String, u64)>,
}

impl Gateway {
    /// The gateway `config` describes, with each backend's key named by
    /// `key` (a secret name to where its value is).
    pub fn new(
        config: &Config,
        signer: Signer,
        key: impl Fn(&str) -> KeySource,
    ) -> Result<Gateway, String> {
        let mut backends = Vec::new();
        for (name, b) in &config.backends {
            check_name(name).map_err(|e| format!("models.backends: {e}"))?;
            let url = match (&b.url, b.api.default_url()) {
                (Some(url), _) => url.trim_end_matches('/').to_owned(),
                (None, Some(url)) => url.to_owned(),
                (None, None) => {
                    return Err(format!(
                        "models.backends.{name}: a generic backend needs a url"
                    ))
                }
            };
            upstream::Target::parse(&url)
                .map_err(|e| format!("models.backends.{name}.url: {e}"))?;
            if b.header.is_some() && b.api != Api::Generic {
                return Err(format!(
                    "models.backends.{name}.header: only a generic backend takes a header"
                ));
            }
            backends.push(Backend {
                name: name.clone(),
                api: b.api,
                url,
                key: b.key.as_deref().map(&key),
                header: b.header.clone(),
            });
        }
        let known = |name: &str, at: &str| -> Result<(), String> {
            match config.backends.contains_key(name) {
                true => Ok(()),
                false => Err(format!("{at}: no backend {name}")),
            }
        };
        let mut routes = Vec::new();
        for (i, r) in config.routes.iter().enumerate() {
            let at = format!("models.routes[{i}]");
            branchyard_provision::models::check_pattern(&r.model)
                .map_err(|e| format!("{at}.model: {e}"))?;
            if r.backends.is_empty() {
                return Err(format!("{at}.backends: name at least one backend"));
            }
            if !r.weights.is_empty() && r.weights.len() != r.backends.len() {
                return Err(format!("{at}.weights: give one weight per backend"));
            }
            if r.weights.contains(&0) {
                return Err(format!("{at}.weights: a weight must be at least 1"));
            }
            for name in r.backends.iter().chain(&r.fallbacks) {
                known(name, &at)?;
            }
            if r.requests_per_minute == Some(0) {
                return Err(format!("{at}.requests_per_minute: must be at least 1"));
            }
            routes.push(Route {
                model: r.model.clone(),
                backends: r
                    .backends
                    .iter()
                    .enumerate()
                    .map(|(j, b)| (b.clone(), r.weights.get(j).copied().unwrap_or(1)))
                    .collect(),
                fallbacks: r.fallbacks.clone(),
                requests_per_minute: r.requests_per_minute,
            });
        }
        let budget = config.budget.clone().unwrap_or_default();
        if budget.alert_at.is_some_and(|a| !(a > 0.0 && a <= 1.0)) {
            return Err("models.budget.alert_at: give a share above 0 and at most 1".into());
        }
        for (what, value) in [
            ("daily_usd", budget.daily_usd),
            ("monthly_usd", budget.monthly_usd),
        ] {
            if value.is_some_and(|v| !(v.is_finite() && v >= 0.0)) {
                return Err(format!("models.budget.{what}: give a number of dollars"));
            }
        }
        for model in config.prices.keys() {
            branchyard_provision::models::check_pattern(model)
                .map_err(|e| format!("models.prices: {e}"))?;
        }
        let listen = match &config.listen {
            Some(text) => text
                .parse::<IpAddr>()
                .map_err(|_| format!("models.listen: {text:?} is not an IP address"))?,
            None => IpAddr::V4(Ipv4Addr::LOCALHOST),
        };
        let seed = config
            .seed
            .unwrap_or_else(branchyard_support::rng::fresh_seed);
        Ok(Gateway {
            backends,
            routes,
            budget,
            prices: config.prices.clone(),
            listen,
            sandbox_host: config.sandbox_host.clone(),
            signer,
            branch_scope: None,
            state: Mutex::new(Shared {
                windows: HashMap::new(),
                rng: branchyard_support::rng::SplitMix64::new(seed),
                alerted: Vec::new(),
            }),
        })
    }

    fn backend(&self, name: &str) -> Option<&Backend> {
        self.backends.iter().find(|b| b.name == name)
    }

    /// Whether some backend speaks `api`.
    pub fn serves(&self, api: Api) -> bool {
        self.backends.iter().any(|b| b.api == api)
    }

    fn by_branch(&self, branch: &str) -> String {
        match &self.branch_scope {
            Some(scope) => format!("{scope}/{branch}"),
            None => branch.to_owned(),
        }
    }

    /// The backends to try for `model` on `api`, in order: the route's
    /// weighted backends (one chosen by weight first, then the rest in
    /// order), then its fallbacks. Without a route, every backend of
    /// `api` in order. Also the route's index, for its rate limit.
    pub(crate) fn plan(&self, api: Api, model: Option<&str>) -> (Option<usize>, Vec<&Backend>) {
        let route = model.and_then(|m| {
            self.routes.iter().enumerate().find(|(_, r)| {
                branchyard_provision::connectors::glob_match(&r.model, m)
                    && r.backends
                        .iter()
                        .map(|(name, _)| name)
                        .chain(&r.fallbacks)
                        .filter_map(|name| self.backend(name))
                        .any(|b| b.api == api)
            })
        });
        let Some((index, route)) = route else {
            let all = self.backends.iter().filter(|b| b.api == api).collect();
            return (None, all);
        };
        let total: u64 = route.backends.iter().map(|(_, w)| u64::from(*w)).sum();
        let pick = {
            let mut state = self.state.lock_recovering("state");
            state.rng.next_u64() % total.max(1)
        };
        let mut first = 0;
        let mut acc = 0u64;
        for (i, (_, weight)) in route.backends.iter().enumerate() {
            acc += u64::from(*weight);
            if pick < acc {
                first = i;
                break;
            }
        }
        let mut names: Vec<&str> = vec![route.backends[first].0.as_str()];
        for (i, (name, _)) in route.backends.iter().enumerate() {
            if i != first && !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
        for name in &route.fallbacks {
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
        let order = names
            .into_iter()
            .filter_map(|n| self.backend(n))
            .filter(|b| b.api == api)
            .collect();
        (Some(index), order)
    }

    /// Take one request from route `index`'s rate limit at `now_ms`;
    /// `Err(seconds)` until a slot frees when it is used up.
    pub(crate) fn admit(&self, index: usize, now_ms: u64) -> Result<(), u64> {
        let Some(limit) = self.routes.get(index).and_then(|r| r.requests_per_minute) else {
            return Ok(());
        };
        let mut state = self.state.lock_recovering("state");
        let window = state.windows.entry(index).or_default();
        while window
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) >= 60_000)
        {
            window.pop_front();
        }
        if window.len() >= limit as usize {
            let oldest = window.front().copied().unwrap_or(now_ms);
            return Err((60_000 - now_ms.saturating_sub(oldest))
                .div_ceil(1000)
                .max(1));
        }
        window.push_back(now_ms);
        Ok(())
    }

    /// Whether an alert for `period` starting at `start` is still to be
    /// recorded in this process; marks it recorded.
    fn first_alert(&self, period: &str, start: u64) -> bool {
        let mut state = self.state.lock_recovering("state");
        let key = (period.to_owned(), start);
        match state.alerted.contains(&key) {
            true => false,
            false => {
                state.alerted.push(key);
                true
            }
        }
    }

    /// What `model`'s tokens cost on `api`: `[models.prices]` first, then
    /// the catalog. `None` when neither prices it.
    pub fn cost(&self, api: Api, model: &str, tokens: &Tokens) -> Option<f64> {
        let configured = self
            .prices
            .iter()
            .filter(|(pattern, _)| branchyard_provision::connectors::glob_match(pattern, model))
            .max_by_key(|(pattern, _)| pattern.len());
        if let Some((_, price)) = configured {
            return Some(
                (tokens.input as f64 * price.input
                    + tokens.output as f64 * price.output
                    + tokens.cache_read as f64 * price.cache_read
                    + (tokens.cache_write + tokens.cache_write_1h) as f64 * price.cache_write)
                    / 1e6,
            );
        }
        pricing::cost(api, model, tokens)
    }
}

/// A backend name: `[A-Za-z0-9_-]`, 1 to 64 characters.
fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    match ok {
        true => Ok(()),
        false => Err(format!(
            "backend name {name:?} must be 1 to 64 letters, digits, - or _"
        )),
    }
}

/// One call through the gateway, as the store keeps it for budgets and
/// `by models`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub id: String,
    /// Milliseconds since the Unix epoch, when the call ended.
    pub at_ms: u64,
    pub branch: String,
    pub turn: u32,
    /// Who the branch acts for.
    pub subject: String,
    pub model: String,
    pub api: Api,
    pub backend: String,
    #[serde(flatten)]
    pub tokens: Tokens,
    /// `None` when neither the catalog nor `[models.prices]` prices the
    /// model.
    pub cost_usd: Option<f64>,
    pub latency_ms: u64,
    /// The upstream's status.
    pub status: u16,
    pub streamed: bool,
}

/// The usage store, in SQLite or PostgreSQL beside the branches. Rows are
/// kept when their branch is removed: they are the yard's spending.
pub(crate) trait UsageBackend: Send + Sync + fmt::Debug {
    fn put_usage(&self, row: &UsageRecord) -> Result<(), Error>;
    /// Every row at or after `since_ms`, oldest first.
    fn usage_since(&self, since_ms: u64) -> Result<Vec<UsageRecord>, Error>;
}

/// What the gateway did, recorded on the branch.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelActivity {
    /// The turn's harness calls its models through the gateway at `url`.
    Gateway {
        url: String,
        /// The models the token allows.
        models: Vec<String>,
        /// The APIs the gateway serves for it.
        apis: Vec<Api>,
    },
    /// The branch is on the gateway, but this turn's harness calls its
    /// provider itself, and why.
    Direct {
        reason: String,
        /// Hosts added to the turn's egress policy for it.
        hosts: Vec<String>,
    },
    /// One call: forwarded, or refused before it was.
    Call(ModelCall),
    /// The yard's spending in a period passed its alert threshold.
    Alert {
        /// `day` or `month` (UTC).
        period: String,
        /// `usd` or `tokens`.
        unit: String,
        used: f64,
        limit: f64,
    },
}

impl ModelActivity {
    /// One line for people, as `by log` prints it.
    pub fn describe(&self) -> String {
        match self {
            ModelActivity::Gateway { url, models, .. } => {
                format!(
                    "models through the gateway at {url} ({})",
                    models.join(", ")
                )
            }
            ModelActivity::Direct { reason, hosts } => match hosts.is_empty() {
                true => format!("models direct: {reason}"),
                false => format!(
                    "models direct: {reason} (egress allows {})",
                    hosts.join(", ")
                ),
            },
            ModelActivity::Call(call) => format!("model: {}", call.describe()),
            ModelActivity::Alert {
                period,
                unit,
                used,
                limit,
            } => match unit.as_str() {
                "usd" => format!("model budget alert: ${used:.4} of ${limit:.2} this {period}"),
                _ => format!("model budget alert: {used:.0} of {limit:.0} tokens this {period}"),
            },
        }
    }
}

/// One call through the gateway.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelCall {
    /// The model asked for; empty for a request that names none.
    pub model: String,
    pub api: Api,
    /// `allowed`, `denied` (the token's scope), `rate_limited`, `budget`,
    /// `unauthorized` or `failed` (no backend answered).
    pub decision: String,
    /// The backend that answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Why it was refused, or what each backend that failed did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The status the harness got.
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub latency_ms: u64,
    #[serde(default)]
    pub streamed: bool,
    /// Backends tried before the one that answered, with what each did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_over: Vec<String>,
}

impl ModelCall {
    /// `claude-sonnet-4-6 allowed via anthropic (200, 1200 in / 80 out,
    /// $0.0048, 812 ms)`.
    pub fn describe(&self) -> String {
        let model = match self.model.is_empty() {
            true => "(no model)",
            false => self.model.as_str(),
        };
        let mut out = format!("{model} {}", self.decision);
        if let Some(backend) = &self.backend {
            out.push_str(&format!(" via {backend}"));
        }
        let mut detail = vec![self.status.to_string()];
        if let Some(reason) = &self.reason {
            detail.push(reason.clone());
        }
        if let Some(tokens) = &self.tokens {
            detail.push(format!(
                "{} in / {} out",
                tokens.input_total(),
                tokens.output
            ));
        }
        if let Some(cost) = self.cost_usd {
            detail.push(format!("${cost:.4}"));
        }
        detail.push(format!("{} ms", self.latency_ms));
        out.push_str(&format!(" ({})", detail.join(", ")));
        out
    }
}

/// What a turn's model access gives its harness.
pub(crate) enum Prepared {
    /// Through this turn's gateway.
    Gateway {
        gateway: Box<TurnGateway>,
        env: Vec<(String, String)>,
        /// Variables taken out of the harness's environment: provider
        /// credentials it must not hold.
        scrub: Vec<&'static str>,
        /// The URL the harness is given, which a restricted network
        /// policy allows.
        url: String,
        activity: ModelActivity,
    },
    /// Direct, with hosts its egress must allow.
    Direct {
        hosts: Vec<HostRule>,
        activity: ModelActivity,
    },
}

/// Credentials a harness on the gateway must not hold: they would let it
/// reach its provider around the gateway.
const SCRUB: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CODEX_API_KEY",
    "OPENAI_ORG_ID",
    "AZURE_OPENAI_API_KEY",
];

/// Secrets that log a harness in by subscription, which the gateway
/// cannot stand in for.
const SUBSCRIPTION_SECRETS: &[&str] = &["CLAUDE_CODE_OAUTH_TOKEN", "CODEX_AUTH"];
/// Provider keys a branch on the gateway may not be given: the gateway
/// holds them.
pub const PROVIDER_KEYS: &[&str] = &["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "CODEX_API_KEY"];

/// The API `harness` speaks to its provider, when Branchyard knows it.
pub fn harness_api(harness: &str) -> Option<Api> {
    match harness {
        "claude-code" => Some(Api::Anthropic),
        "codex" => Some(Api::Openai),
        _ => None,
    }
}

/// The hosts a harness speaking `api` reaches directly.
fn direct_hosts(harness: &str, api: Option<Api>) -> Vec<HostRule> {
    let names: &[&str] = match (harness, api) {
        ("codex", _) => &[
            "api.openai.com:443",
            "chatgpt.com:443",
            "auth.openai.com:443",
        ],
        (_, Some(Api::Anthropic)) => &["api.anthropic.com:443", "console.anthropic.com:443"],
        (_, Some(Api::Openai)) => &["api.openai.com:443"],
        _ => &[],
    };
    names
        .iter()
        .filter_map(|n| HostRule::parse(n).ok())
        .collect()
}

/// Why `record`'s harness must call its provider itself, if it must.
fn direct_reason(gateway: &Gateway, record: &Record, api: Option<Api>) -> Option<String> {
    let spec = record.provision.as_ref()?;
    let subscription = spec
        .secrets
        .iter()
        .find(|s| SUBSCRIPTION_SECRETS.contains(&s.name.as_str()))
        .map(|s| s.name.clone())
        .or_else(|| {
            spec.auth
                .as_deref()
                .filter(|a| *a == "oauth-token")
                .map(|a| format!("--auth {a}"))
        });
    if let Some(login) = subscription {
        return Some(format!(
            "{} is logged in by subscription ({login}), which cannot take an injected key",
            record.info.harness
        ));
    }
    match api {
        Some(api) if !gateway.serves(api) => Some(format!(
            "no backend serves the {api} API {} speaks",
            record.info.harness
        )),
        None if gateway.backends.is_empty() => Some("the gateway has no backends".to_owned()),
        _ => None,
    }
}

/// Prepare the turn's model access: `None` when the branch is not on the
/// gateway. `token` is the connector token when the turn has one (it
/// carries the model scope too); otherwise the gateway's own is minted. A
/// failure is the turn's, by name.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare(
    yard: &Yard,
    record: &Record,
    token: Option<&str>,
    deadline_ms: Option<u64>,
    scopes: &TokenScopes,
    budget: crate::Budget,
    reserved: f64,
) -> Result<Option<Prepared>, String> {
    let Some(access) = record.provision.as_ref().and_then(|p| p.models.clone()) else {
        return Ok(None);
    };
    let gateway = yard.models().ok_or(
        "it is on the model gateway, but this yard has no [models] backends (docs/model-gateway.md)",
    )?;
    let spec = record
        .provision
        .as_ref()
        .expect("models imply provisioning");
    if let Some(key) = spec
        .secrets
        .iter()
        .find(|s| PROVIDER_KEYS.contains(&s.name.as_str()))
    {
        return Err(format!(
            "it is on the model gateway, which holds the provider's key; drop the secret {}",
            key.name
        ));
    }
    let api = harness_api(&record.info.harness);
    if let Some(reason) = direct_reason(&gateway, record, api) {
        let hosts = direct_hosts(&record.info.harness, api);
        return Ok(Some(Prepared::Direct {
            activity: ModelActivity::Direct {
                reason,
                hosts: hosts.iter().map(ToString::to_string).collect(),
            },
            hosts,
        }));
    }
    let sandboxed = crate::placement::sandboxed(record.provider.as_ref());
    let host = match (sandboxed, &gateway.sandbox_host) {
        (false, _) => match gateway.listen {
            IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_owned(),
            IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".to_owned(),
            IpAddr::V6(ip) => format!("[{ip}]"),
            ip => ip.to_string(),
        },
        (true, Some(host)) => host.clone(),
        (true, None) => {
            return Err(
                "it is on the model gateway and runs in a sandbox, but no gateway address \
                 reachable from a sandbox is configured ([models] sandbox_host)"
                    .into(),
            )
        }
    };
    let listener = TcpListener::bind((gateway.listen, 0))
        .map_err(|e| format!("could not start the model gateway: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("could not start the model gateway: {e}"))?
        .port();
    let url = format!("http://{host}:{port}");
    let keys = gateway
        .signer
        .keys()
        .map_err(|e| format!("could not read the signing key: {e}"))?;
    let token = match token {
        Some(token) => token.to_owned(),
        None => {
            let now = branchyard_support::time::now_ms() / 1000;
            let mut exp = now + crate::connectors::MAX_TTL.as_secs();
            if let Some(deadline) = deadline_ms {
                exp = exp.min((deadline / 1000).max(now + 1));
            }
            let actor = record.actor.clone().unwrap_or_else(|| Actor {
                subject: gateway.signer.subject.clone(),
                tenant: gateway.signer.tenant.clone(),
            });
            let claims = crate::connectors::turn_claims(
                &gateway.signer.issuer,
                &url,
                actor,
                gateway.by_branch(&record.info.name),
                record.info.turns + 1,
                (now, exp),
                Vec::new(),
                scopes,
            )?;
            keys.sign(&claims)
                .map_err(|e| format!("could not sign the model gateway token: {e}"))?
        }
    };
    let jwks = keys.jwks().map_err(|e| e.to_string())?;
    let subject = record
        .actor
        .as_ref()
        .map_or_else(|| gateway.signer.subject.clone(), |a| a.subject.clone());
    let served: Vec<Api> = [Api::Anthropic, Api::Openai]
        .into_iter()
        .filter(|a| gateway.serves(*a))
        .collect();
    let turn = gateway::TurnState {
        yard: yard.clone(),
        gateway: gateway.clone(),
        jwks,
        token: token.clone(),
        branch: record.info.name.clone(),
        by_branch: gateway.by_branch(&record.info.name),
        turn: record.info.turns + 1,
        subject,
        max_usd: budget.max_usd,
        spent_before: record.info.cost_usd.unwrap_or(0.0) + reserved,
        metered: Mutex::new(0.0),
    };
    let running = TurnGateway::start(listener, Arc::new(turn))
        .map_err(|e| format!("could not start the model gateway: {e}"))?;
    // Announced like the turn's egress proxy (docs/registry.md), by what it
    // serves; nothing to reclaim, since it lives and dies with this process.
    let service = crate::services::Service::new(
        crate::services::KIND_MODEL_GATEWAY,
        crate::services::ServiceOwner::this_process().for_branch(&record.info.name),
    )
    .with("branch", record.info.name.as_str())
    .with(
        "apis",
        served
            .iter()
            .map(|a| a.as_str().to_owned())
            .collect::<Vec<_>>(),
    )
    .with("models", access.allow.clone())
    .with_endpoint(crate::services::Endpoint::url(url.clone()));
    let running = running.registered(
        yard.register_service(service, crate::services::DEFAULT_TTL)
            .ok(),
    );
    let mut env = Vec::new();
    if gateway.serves(Api::Anthropic) {
        env.push(("ANTHROPIC_BASE_URL".to_owned(), format!("{url}/anthropic")));
        env.push(("ANTHROPIC_API_KEY".to_owned(), token.clone()));
    }
    if gateway.serves(Api::Openai) {
        env.push(("OPENAI_BASE_URL".to_owned(), format!("{url}/openai/v1")));
        env.push(("OPENAI_API_KEY".to_owned(), token.clone()));
    }
    env.push((ENV_GATEWAY.to_owned(), url.clone()));
    Ok(Some(Prepared::Gateway {
        gateway: Box::new(running),
        env,
        scrub: SCRUB.to_vec(),
        activity: ModelActivity::Gateway {
            url: url.clone(),
            models: access.allow.clone(),
            apis: served,
        },
        url,
    }))
}

/// The variable naming the turn's model gateway, for a harness or tool
/// that takes neither provider's base URL variable.
pub const ENV_GATEWAY: &str = "BRANCHYARD_MODEL_GATEWAY";

/// The yard's spending since `since_ms`, summed.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct UsageTotals {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    /// Calls whose model nothing prices.
    pub unpriced: u64,
}

impl UsageTotals {
    pub fn add(&mut self, row: &UsageRecord) {
        self.calls += 1;
        self.input_tokens += row.tokens.input;
        self.output_tokens += row.tokens.output;
        self.cache_read_tokens += row.tokens.cache_read;
        self.cache_write_tokens += row.tokens.cache_write + row.tokens.cache_write_1h;
        match row.cost_usd {
            Some(cost) => self.cost_usd += cost,
            None => self.unpriced += 1,
        }
    }

    /// Every token, as budgets count them.
    pub fn tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// Totals of `rows`, overall and by model.
pub fn summarize(rows: &[UsageRecord]) -> (UsageTotals, BTreeMap<String, UsageTotals>) {
    let mut all = UsageTotals::default();
    let mut by_model: BTreeMap<String, UsageTotals> = BTreeMap::new();
    for row in rows {
        all.add(row);
        by_model.entry(row.model.clone()).or_default().add(row);
    }
    (all, by_model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_start_at_utc_midnight_and_the_first_of_the_month() {
        // 2026-10-01T12:34:56Z.
        let now = 1_790_858_096_000;
        let (day, month) = branchyard_support::time::period_starts(now);
        assert_eq!(day, 1_790_812_800_000);
        assert_eq!(month, 1_790_812_800_000);
        // 2024-02-29T23:59:59Z, a leap day.
        let (day, month) = branchyard_support::time::period_starts(1_709_251_199_000);
        assert_eq!(day, 1_709_164_800_000);
        assert_eq!(month, 1_706_745_600_000);
    }

    #[test]
    fn rate_windows_free_a_slot_a_minute_after_it_was_taken() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "backends": {"a": {"api": "anthropic", "url": "http://127.0.0.1:1"}},
            "routes": [{"model": "*", "backends": ["a"], "requests_per_minute": 2}],
            "seed": 1
        }))
        .unwrap();
        let signer = Signer {
            issuer: "i".into(),
            key_file: "/nonexistent".into(),
            jwks_file: None,
            subject: "s".into(),
            tenant: "t".into(),
        };
        let gateway = Gateway::new(&config, signer, |n| KeySource::Env(n.into())).unwrap();
        assert_eq!(gateway.admit(0, 1_000), Ok(()));
        assert_eq!(gateway.admit(0, 2_000), Ok(()));
        assert_eq!(gateway.admit(0, 3_000), Err(58));
        assert_eq!(gateway.admit(0, 60_999), Err(1));
        assert_eq!(gateway.admit(0, 61_000), Ok(()));
    }

    fn signer() -> Signer {
        Signer {
            issuer: "i".into(),
            key_file: "/nonexistent".into(),
            jwks_file: None,
            subject: "s".into(),
            tenant: "t".into(),
        }
    }

    #[test]
    fn the_weighted_choice_follows_its_seed_and_the_weights() {
        let config = |seed: u64| -> Config {
            serde_json::from_value(serde_json::json!({
                "backends": {
                    "a": {"api": "anthropic", "url": "http://127.0.0.1:1"},
                    "b": {"api": "anthropic", "url": "http://127.0.0.1:2"},
                    "c": {"api": "anthropic", "url": "http://127.0.0.1:3"},
                    "o": {"api": "openai", "url": "http://127.0.0.1:4"}
                },
                "routes": [{"model": "claude-*", "backends": ["a", "b"], "weights": [3, 1],
                            "fallbacks": ["c", "o"]}],
                "seed": seed
            }))
            .unwrap()
        };
        let orders = |seed: u64| -> Vec<Vec<String>> {
            let gateway =
                Gateway::new(&config(seed), signer(), |n| KeySource::Env(n.into())).unwrap();
            (0..400)
                .map(|_| {
                    let (route, order) = gateway.plan(Api::Anthropic, Some("claude-sonnet-4-6"));
                    assert_eq!(route, Some(0));
                    order.iter().map(|b| b.name.clone()).collect()
                })
                .collect()
        };
        let first = orders(42);
        // The same seed, the same order, call for call.
        assert_eq!(first, orders(42));
        assert_ne!(first, orders(43));
        // The chosen backend first, the other after it, then the
        // fallbacks of this API in order; the OpenAI fallback never.
        for order in &first {
            assert!(
                order == &["a", "b", "c"] || order == &["b", "a", "c"],
                "{order:?}"
            );
        }
        // About three in four go to `a` first.
        let a = first.iter().filter(|o| o[0] == "a").count();
        assert!((250..350).contains(&a), "{a} of 400");
        // Without a route, every backend of the API in order.
        let gateway = Gateway::new(&config(1), signer(), |n| KeySource::Env(n.into())).unwrap();
        let (route, order) = gateway.plan(Api::Openai, Some("gpt-5"));
        assert_eq!(route, None);
        assert_eq!(
            order.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            ["o"]
        );
        let (route, order) = gateway.plan(Api::Anthropic, None);
        assert_eq!(route, None);
        assert_eq!(order.len(), 3);
    }

    fn record(harness: &str, provision: serde_json::Value) -> Record {
        serde_json::from_value(serde_json::json!({
            "info": {
                "name": "b", "git_branch": "by/b", "worktree": "/w",
                "prompt": "p", "harness": harness, "profile": "p", "session": null,
                "parent": null, "base": "b", "candidate": null,
                "status": {"state": "running"}, "turns": 0, "cost_usd": null,
                "created_at": 0
            },
            "created_ms": 0, "check": null, "command": null, "home": null,
            "cost_baseline": null, "provision": provision
        }))
        .unwrap()
    }

    #[test]
    fn a_subscription_login_or_an_api_no_backend_serves_runs_direct() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "backends": {"a": {"api": "anthropic"}}
        }))
        .unwrap();
        let gateway = Gateway::new(&config, signer(), |n| KeySource::Env(n.into())).unwrap();
        let on = serde_json::json!({"models": {"allow": ["*"]}});
        let claude = record("claude-code", on.clone());
        assert_eq!(
            direct_reason(&gateway, &claude, harness_api("claude-code")),
            None
        );
        let oauth = record(
            "claude-code",
            serde_json::json!({"models": {"allow": ["*"]},
                "secrets": [{"name": "CLAUDE_CODE_OAUTH_TOKEN"}]}),
        );
        let why = direct_reason(&gateway, &oauth, harness_api("claude-code")).unwrap();
        assert!(
            why.contains("subscription (CLAUDE_CODE_OAUTH_TOKEN)"),
            "{why}"
        );
        let chatgpt = record(
            "codex",
            serde_json::json!({"models": {"allow": ["*"]}, "auth": "oauth-token"}),
        );
        let why = direct_reason(&gateway, &chatgpt, harness_api("codex")).unwrap();
        assert!(why.contains("--auth oauth-token"), "{why}");
        let codex = record("codex", on.clone());
        let why = direct_reason(&gateway, &codex, harness_api("codex")).unwrap();
        assert!(why.contains("no backend serves the openai API"), "{why}");
        // A harness Branchyard does not know the API of goes through, on
        // both variables.
        let other = record("gemini-cli", on);
        assert_eq!(direct_reason(&gateway, &other, None), None);
        // Direct, its provider's hosts are allowed.
        let hosts = |h: &str| -> Vec<String> {
            direct_hosts(h, harness_api(h))
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        assert_eq!(
            hosts("claude-code"),
            ["api.anthropic.com:443", "console.anthropic.com:443"]
        );
        assert_eq!(
            hosts("codex"),
            [
                "api.openai.com:443",
                "chatgpt.com:443",
                "auth.openai.com:443"
            ]
        );
        assert!(hosts("gemini-cli").is_empty());
    }

    #[test]
    fn a_bad_configuration_is_refused_by_name() {
        let signer = || Signer {
            issuer: "i".into(),
            key_file: "/nonexistent".into(),
            jwks_file: None,
            subject: "s".into(),
            tenant: "t".into(),
        };
        for (config, why) in [
            (
                serde_json::json!({"backends": {"g": {"api": "generic"}}}),
                "needs a url",
            ),
            (
                serde_json::json!({"backends": {"a b": {"api": "openai"}}}),
                "backend name",
            ),
            (
                serde_json::json!({"routes": [{"model": "*", "backends": ["x"]}]}),
                "no backend x",
            ),
            (
                serde_json::json!({"backends": {"a": {"api": "openai"}},
                    "routes": [{"model": "*", "backends": ["a"], "weights": [1, 2]}]}),
                "one weight per backend",
            ),
            (
                serde_json::json!({"backends": {"a": {"api": "openai", "header": "x"}}}),
                "only a generic",
            ),
            (
                serde_json::json!({"backends": {"a": {"api": "openai", "url": "ftp://x"}}}),
                "url",
            ),
            (serde_json::json!({"budget": {"alert_at": 2.0}}), "alert_at"),
            (serde_json::json!({"listen": "localhost"}), "IP address"),
        ] {
            let config: Config = serde_json::from_value(config.clone()).unwrap();
            let error = Gateway::new(&config, signer(), |n| KeySource::Env(n.into())).unwrap_err();
            assert!(error.contains(why), "{config:?}: {error}");
        }
    }
}
