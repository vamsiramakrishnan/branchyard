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
//! the next on a refused connection, a 5xx or a 429, unless a control
//! plane such as an AIPlex access point refused it itself, which is final;
//! one call never goes to both a control plane and a plain backend), holds
//! per-route rate limits, the branch's budget and the yard's daily and monthly budgets,
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

mod audit;
pub mod discover;
pub(crate) mod gateway;
mod gemini;
pub mod pricing;
mod translate;
pub(crate) mod upstream;

/// A backend's or server's base URL, parsed: scheme, host, port and path
/// prefix. The one parser for them; `branchyard_client::http::Endpoint`
/// builds on it.
pub use upstream::Target as BaseUrl;
/// How a request reaches a provider through the operator's proxy and root
/// certificates (`by models list`).
pub use upstream::Transport;
mod usage;

use branchyard_support::LockExt as _;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::OsString;
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
#[derive(strum::Display, strum::EnumString, strum::EnumIter, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
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
        self.into()
    }

    /// Whether a request to this API can be translated for a backend
    /// speaking `backend`: Anthropic's Messages API onto OpenAI's chat
    /// completions, nothing else (`translate`).
    pub(crate) fn translates_to(self, backend: Api) -> bool {
        self == Api::Anthropic && backend == Api::Openai
    }

    /// The provider's public API, for a plain backend that names no URL
    /// ([`backend_url`], the one caller: a control-plane backend has no
    /// default).
    fn default_url(&self) -> Option<&'static str> {
        match self {
            Api::Anthropic => Some("https://api.anthropic.com"),
            Api::Openai => Some("https://api.openai.com"),
            Api::Generic => None,
        }
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
    /// What is kept of each call beyond its [`Interception`] (default
    /// `digests`); `contents` also writes each call's request and response
    /// to a file under `.branchyard/models/audit/`.
    #[serde(default, skip_serializing_if = "Audit::is_default")]
    pub audit: Audit,
}

/// What the gateway keeps of the contents of calls (`[models] audit`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Audit {
    /// Per call, an [`Interception`]: sizes, a digest of the request, the
    /// tools offered and called, why it stopped. No prompt or answer text
    /// is kept.
    #[default]
    Digests,
    /// Also each call's request and response, one JSON line in
    /// `.branchyard/models/audit/<branch>/<turn>.jsonl`, never in the event
    /// log.
    Contents,
}

impl Audit {
    fn is_default(&self) -> bool {
        *self == Audit::default()
    }
}

impl fmt::Display for Audit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Audit::Digests => "digests",
            Audit::Contents => "contents",
        })
    }
}

/// One backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub api: Api,
    /// Its base URL, where its calls and its credential go. Default: the
    /// provider's public API, for a plain `anthropic` or `openai` backend
    /// only; a generic backend and a control-plane backend must name one.
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
    /// What the gateway adapts for an endpoint that imitates OpenAI's API
    /// with differences ([`Compat`]). Default: [`Compat::Gemini`] for an
    /// `openai` backend whose URL's host is
    /// `generativelanguage.googleapis.com`, [`Compat::Versioned`] for one
    /// whose host is `api.z.ai`, else [`Compat::Off`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<Compat>,
    /// The control plane this backend is an access point of
    /// ([`ControlPlane`]); absent, a plain backend. A control plane's
    /// refusal is final, and a route that names one names only control
    /// planes (`docs/model-gateway.md`, "AIPlex as the backend").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_plane: Option<ControlPlane>,
    /// How an AIPlex backend is called when not with a `key`
    /// ([`BackendAuth`]); absent, with its `key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<BackendAuth>,
    /// The `aud` of the token [`BackendAuth::TurnToken`] presents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
}

/// How a control-plane backend is called instead of with a stored key
/// (`[models.backends.NAME] auth`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BackendAuth {
    /// A token the yard signs for each turn (`turn-token`), which AIPlex
    /// verifies against the yard's published keys
    /// (`docs/model-gateway.md`, "A token Branchyard signs").
    #[serde(rename = "turn-token")]
    TurnToken,
}

/// The `by_purpose` of the token a turn's gateway presents to an AIPlex
/// backend with [`BackendAuth::TurnToken`]: the model gateway and the
/// connector gateway refuse a token that carries one.
pub const AIPLEX_PURPOSE: &str = "aiplex";

/// Why `issuer` cannot sign tokens AIPlex verifies, if it cannot. AIPlex
/// fetches `{issuer}/.well-known/jwks.json` and compares the issuer byte
/// for byte with the one it was bound to, so it must be `https://host[:port]`,
/// or `http://` on a loopback host (`127.0.0.1`, `[::1]`, `localhost`), with
/// nothing after; an address something can be reached at (not `0.0.0.0` or
/// `[::]`); and in the one spelling Branchyard takes
/// ([`canonical_issuer`]), so the two sides cannot differ by a dot. Each
/// refusal says what is wrong, and what to write where Branchyard can tell.
pub fn federable_issuer(issuer: &str) -> Result<(), String> {
    if issuer.starts_with("branchyard:local:") {
        return Err(format!(
            "this is a local yard (its tokens' issuer is {issuer}), which AIPlex cannot fetch keys \
             from, so it cannot be federated; use key = \"...\" (an AIPlex token) instead"
        ));
    }
    let written = WrittenIssuer::read(issuer)?;
    let host = written.host();
    if host
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.to_canonical().is_unspecified())
    {
        return Err(format!(
            "the issuer {issuer:?} names an unspecified address, which a server listens on and \
             nothing can be reached at, so AIPlex cannot fetch keys from it"
        ));
    }
    let loopback = matches!(host.as_str(), "127.0.0.1" | "::1" | "localhost");
    if !written.tls && !loopback {
        return Err(format!(
            "the issuer {issuer:?} is plain http on a host that is not loopback; AIPlex fetches \
             keys over https (http only from this machine, for a development AIPlex)"
        ));
    }
    let canonical = written.canonical();
    match canonical == issuer {
        true => Ok(()),
        false => Err(format!(
            "the issuer {issuer:?} {}; the issuer must be spelled the same in Branchyard and in \
             AIPlex, so Branchyard takes one spelling (a lower-case host, no trailing dot, no \
             default port): write {canonical:?}",
            written.misspelled()
        )),
    }
}

/// The one spelling Branchyard takes of the issuer `issuer`: its scheme, its
/// host in lower case without a trailing dot (an IPv6 address in brackets),
/// and its port unless it is the scheme's default. `None` when `issuer` is
/// not `scheme://host[:port]` at all. An issuer is spelled the same in
/// Branchyard and in AIPlex, which compares byte for byte, so one spelling
/// leaves nothing to differ.
pub fn canonical_issuer(issuer: &str) -> Option<String> {
    WrittenIssuer::read(issuer).ok().map(|w| w.canonical())
}

/// An issuer of the form `scheme://host[:port]`, as it was written.
struct WrittenIssuer {
    tls: bool,
    /// The host as written, without an IPv6 literal's brackets.
    host: String,
    port: u16,
    /// The port as written, when one was.
    port_written: Option<String>,
}

impl WrittenIssuer {
    /// Take `issuer` apart, or say what an operator wrote that is not
    /// `scheme://host[:port]`: the mistakes one makes by hand are named,
    /// and the rest is the URL parser's to say.
    fn read(issuer: &str) -> Result<WrittenIssuer, String> {
        let rest = issuer
            .strip_prefix("https://")
            .or_else(|| issuer.strip_prefix("http://"))
            .ok_or_else(|| {
                format!(
                    "the issuer {issuer:?} is not an http:// or https:// URL, so AIPlex cannot \
                     fetch keys from it"
                )
            })?;
        let nothing_after = "AIPlex wants scheme://host[:port] and nothing after";
        if rest.contains('/') {
            return Err(format!("the issuer {issuer:?} has a path; {nothing_after}"));
        }
        if rest.contains(['?', '#']) {
            return Err(format!(
                "the issuer {issuer:?} has a query or a fragment; {nothing_after}"
            ));
        }
        // What is left is the authority. An IPv6 address is hex digits and
        // more than one colon; outside brackets its last group would be
        // read as a port.
        let bare_v6 = !rest.starts_with('[')
            && rest.matches(':').count() > 1
            && rest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b":.%".contains(&b));
        if bare_v6 {
            return Err(format!(
                "the issuer {issuer:?} names an IPv6 address without brackets; write the address \
                 in brackets, scheme://[address]:port"
            ));
        }
        let port_written = match rest.strip_prefix('[') {
            Some(v6) => v6
                .split_once(']')
                .and_then(|(_, after)| after.strip_prefix(':')),
            None => rest.split_once(':').map(|(_, port)| port),
        };
        if port_written == Some("") {
            return Err(format!(
                "the issuer {issuer:?} has an empty port; write the port after the colon, or \
                 leave the colon out"
            ));
        }
        let target = upstream::Target::parse(issuer).map_err(|e| {
            format!("the issuer {issuer:?} is not a URL AIPlex can fetch keys from: {e}")
        })?;
        Ok(WrittenIssuer {
            tls: target.tls,
            host: target.host,
            port: target.port,
            port_written: port_written.map(str::to_owned),
        })
    }

    /// The host in the one spelling: lower case, no trailing dot.
    fn host(&self) -> String {
        self.host.trim_end_matches('.').to_ascii_lowercase()
    }

    fn scheme(&self) -> &'static str {
        match self.tls {
            true => "https",
            false => "http",
        }
    }

    fn default_port(&self) -> u16 {
        match self.tls {
            true => 443,
            false => 80,
        }
    }

    /// The issuer in the one spelling ([`canonical_issuer`]).
    fn canonical(&self) -> String {
        let host = self.host();
        let host = match host.contains(':') {
            true => format!("[{host}]"),
            false => host,
        };
        match self.port == self.default_port() {
            true => format!("{}://{host}", self.scheme()),
            false => format!("{}://{host}:{}", self.scheme(), self.port),
        }
    }

    /// How the issuer as written differs from the one spelling.
    fn misspelled(&self) -> String {
        let mut how = Vec::new();
        if self.host.ends_with('.') {
            how.push("ends its host with a dot".to_owned());
        }
        if self.host.bytes().any(|b| b.is_ascii_uppercase()) {
            how.push("has an upper-case letter in its host".to_owned());
        }
        match &self.port_written {
            Some(_) if self.port == self.default_port() => {
                how.push(format!("writes out the default port of {}", self.scheme()))
            }
            Some(port) if *port != self.port.to_string() => {
                how.push("writes its port with a leading zero".to_owned())
            }
            _ => {}
        }
        match how.is_empty() {
            true => "is not in the one spelling Branchyard takes".to_owned(),
            false => how.join(" and "),
        }
    }
}

/// Whether `tenant` can be named in `sub = branchyard:<tenant>`: 1 to 128
/// of `A-Z a-z 0-9 . _ @ + -`, which AIPlex's `branchyard` preset takes.
pub fn federable_tenant(tenant: &str) -> bool {
    (1..=128).contains(&tenant.len())
        && tenant
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._@+-".contains(&b))
}

/// What a backend is an access point of (`[models.backends.NAME]
/// control_plane`): a service that decides, for each call, whether it may
/// reach the provider, and records it. The gateway sends it the credential
/// as a bearer token and says which branch, turn and harness is calling;
/// its refusal reaches the harness as it is, and no call a route sends to
/// it goes to a plain backend instead.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlPlane {
    /// An AIPlex access point (`{AIPLEX}/ap/{id}`), which serves Anthropic's
    /// and OpenAI's APIs under that base.
    Aiplex,
}

impl ControlPlane {
    /// Its name in `control_plane = "..."`.
    pub fn as_str(self) -> &'static str {
        match self {
            ControlPlane::Aiplex => "aiplex",
        }
    }
}

impl fmt::Display for ControlPlane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A backend's compatibility layer (`[models.backends.NAME] compat`): what
/// the gateway changes in the chat completions it sends an endpoint that
/// imitates OpenAI's API but differs from it, and what it reads from the
/// answers (`docs/model-gateway.md`, "Gemini through the OpenAI-compatible
/// endpoint").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compat {
    /// Requests go as they are (`none`), even to a host that would
    /// otherwise turn a layer on.
    #[default]
    #[serde(rename = "none")]
    Off,
    /// Gemini's OpenAI-compatible endpoint
    /// (`https://generativelanguage.googleapis.com/v1beta/openai`): the
    /// OpenAI path's `/v1` is dropped (the base URL stands for it), request
    /// fields it rejects are stripped, and the thought signatures Gemini 3
    /// puts on tool calls are kept and put back on the calls a later
    /// request replays.
    Gemini,
    /// An endpoint whose URL already names the API's version, as Z.ai's do
    /// (`https://api.z.ai/api/coding/paas/v4` for chat completions,
    /// `https://api.z.ai/api/v1` for responses): the OpenAI path's `/v1`
    /// is dropped, and nothing else changes.
    Versioned,
}

impl Compat {
    /// The layer's name in `compat = "..."`.
    pub fn as_str(self) -> &'static str {
        match self {
            Compat::Off => "none",
            Compat::Gemini => "gemini",
            Compat::Versioned => "versioned",
        }
    }
}

/// Z.ai's API host, whose OpenAI-compatible endpoints name their version
/// in the URL ([`Compat::Versioned`] by default).
pub const ZAI_HOST: &str = "api.z.ai";

/// Whether `host` is Z.ai's API.
pub fn is_zai_host(host: &str) -> bool {
    host.eq_ignore_ascii_case(ZAI_HOST)
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
    /// The model the route's backends are asked for, when a backend
    /// speaks another API than the request: a request for a Claude model
    /// translated for an OpenAI backend asks it for this model
    /// (`docs/model-gateway.md`, "Translation"). Without it, a route never
    /// crosses APIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_model: Option<String>,
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
    /// Its compatibility layer: as configured, else by its URL's host.
    pub compat: Compat,
    /// The control plane it is an access point of; `None` for a plain
    /// backend.
    pub control_plane: Option<ControlPlane>,
    /// With [`BackendAuth::TurnToken`], the audience of the token the
    /// gateway signs for each turn and presents instead of a key.
    pub audience: Option<String>,
}

/// A route, resolved.
#[derive(Clone, Debug)]
pub struct Route {
    pub model: String,
    /// Backend names and weights.
    pub backends: Vec<(String, u32)>,
    pub fallbacks: Vec<String>,
    pub requests_per_minute: Option<u32>,
    /// The model a backend of another API is asked for; `None`: this
    /// route never translates.
    pub backend_model: Option<String>,
}

impl Route {
    /// Whether a request to `api` may be translated for one of this
    /// route's backends: the route names a `backend_model` and has a
    /// backend speaking an API the request's translates to.
    fn bridges(&self, gateway: &Gateway, api: Api) -> bool {
        self.backend_model.is_some()
            && self
                .backends
                .iter()
                .map(|(name, _)| name)
                .chain(&self.fallbacks)
                .filter_map(|name| gateway.backend(name))
                .any(|b| api.translates_to(b.api))
    }
}

/// Who signs the turns' tokens: the connector gateway's keys and issuer
/// when the yard has one, so one token serves both.
#[derive(Clone, Debug)]
pub struct Signer {
    pub issuer: String,
    pub key_file: PathBuf,
    pub jwks_file: Option<PathBuf>,
    /// What signs, when not the key file (the connector gateway's Cloud
    /// KMS key, or a worker's existing key file).
    pub signer: Option<Arc<dyn keys::TokenSigner>>,
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
            signer: None,
            subject: crate::connectors::local_subject(),
            tenant: "local".to_owned(),
        })
    }

    fn keys(&self) -> Result<Arc<dyn keys::TokenSigner>, Error> {
        match &self.signer {
            Some(signer) => Ok(Arc::clone(signer)),
            None => Ok(Arc::new(keys::KeyRing::load_or_create(
                &self.key_file,
                self.jwks_file.as_deref(),
            )?)),
        }
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
    /// Whether calls' contents are written to files too.
    pub audit: Audit,
    state: Mutex<Shared>,
    /// The thought signatures Gemini put on tool calls, by person and
    /// call id, for the requests that replay them ([`Compat::Gemini`]).
    signatures: gemini::Signatures,
    /// How calls reach the backends: through the operator's proxy and
    /// root certificates when the environment names them
    /// ([`upstream::Transport::from_env`]).
    transport: upstream::Transport,
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
            let url = backend_url(name, b)?;
            let target = upstream::Target::parse(&url)
                .map_err(|e| format!("models.backends.{name}.url: {e}"))?;
            if b.header.is_some() && b.api != Api::Generic {
                return Err(format!(
                    "models.backends.{name}.header: only a generic backend takes a header"
                ));
            }
            let compat = match b.compat {
                Some(compat) => compat,
                None if b.api == Api::Openai && gemini::is_host(&target.host) => Compat::Gemini,
                None if b.api == Api::Openai && is_zai_host(&target.host) => Compat::Versioned,
                None => Compat::Off,
            };
            if compat != Compat::Off && b.api != Api::Openai {
                return Err(format!(
                    "models.backends.{name}.compat: {} is for an openai backend \
                     (an endpoint that imitates OpenAI's API), not {}",
                    compat.as_str(),
                    b.api
                ));
            }
            if let Some(plane) = b.control_plane {
                check_control_plane(
                    name,
                    plane,
                    b.api,
                    b.key.is_some() || b.auth.is_some(),
                    compat,
                )?;
            }
            let source = b.key.as_deref().map(&key);
            // A control plane's token in a variable of this process: no
            // child this process starts inherits it (`branchyard_support::child`).
            // So it may not be a variable children need, or Branchyard reads.
            if let (Some(_), Some(KeySource::Env(var))) = (b.control_plane, &source) {
                if shared_variable(var) {
                    return Err(format!(
                        "models.backends.{name}.key: the token would be read from {var}, a \
                         variable that child processes need or Branchyard reads itself, and it \
                         is hidden from every child; give the token a variable of its own, such \
                         as AIPLEX_TOKEN"
                    ));
                }
                branchyard_support::child::hide(var);
            }
            let audience = check_auth(name, b, &signer)?;
            backends.push(Backend {
                name: name.clone(),
                api: b.api,
                url,
                audience,
                key: source,
                header: b.header.clone(),
                compat,
                control_plane: b.control_plane,
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
            let plane_of = |name: &String| {
                config
                    .backends
                    .get(name)
                    .and_then(|b| b.control_plane)
                    .is_some()
            };
            let named = || r.backends.iter().chain(&r.fallbacks);
            if let (Some(plane), Some(plain)) = (
                named().find(|n| plane_of(n)),
                named().find(|n| !plane_of(n)),
            ) {
                return Err(mixed_route(&at, &r.model, plane, plain));
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
                backend_model: r.backend_model.clone(),
            });
        }
        check_unrouted(&backends, &routes)?;
        check_shared_keys(&backends)?;
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
        let gateway = Gateway {
            backends,
            routes,
            budget,
            prices: config.prices.clone(),
            listen,
            sandbox_host: config.sandbox_host.clone(),
            signer,
            branch_scope: None,
            audit: config.audit,
            state: Mutex::new(Shared {
                windows: HashMap::new(),
                rng: branchyard_support::rng::SplitMix64::new(seed),
                alerted: Vec::new(),
            }),
            signatures: gemini::Signatures::default(),
            transport: upstream::Transport::from_env(upstream::QUIET),
        };
        // A control plane's token under another variable's name is hidden
        // as the named one was above, from the children still to start.
        gateway.hide_copies()?;
        Ok(gateway)
    }

    /// The tokens of the control-plane backends that are called with a
    /// stored key, each with the name of a backend it is the key of: what
    /// is sent to that backend's `url` and to nothing else. Read now, each
    /// source once. A key that cannot be read now, or is too short to be
    /// used (`MIN_CONTROL_PLANE_KEY`), is not one.
    pub(crate) fn control_plane_tokens(&self) -> Vec<(String, String)> {
        let mut read: Vec<&KeySource> = Vec::new();
        let mut tokens = Vec::new();
        for backend in self.backends.iter().filter(|b| b.control_plane.is_some()) {
            let Some(source) = &backend.key else {
                continue;
            };
            if read.contains(&source) {
                continue;
            }
            read.push(source);
            if let Ok(token) = source.read() {
                if token.chars().count() >= gateway::MIN_CONTROL_PLANE_KEY {
                    tokens.push((backend.name.clone(), token));
                }
            }
        }
        tokens
    }

    /// The control-plane backend whose token `key` holds, when it holds
    /// one: a plain backend's key that does is never sent, since it would
    /// take the control plane's credential to a provider.
    pub(crate) fn token_held_by(&self, key: &str) -> Option<String> {
        self.control_plane_tokens()
            .into_iter()
            .find(|(_, token)| holds(key, token))
            .map(|(backend, _)| backend)
    }

    /// Hide every variable of this process that holds a control-plane
    /// backend's token under another name ([`copies`]) from every child the
    /// process starts from now on (`branchyard_support::child`), and name
    /// them, for a turn to take them out of its harness's environment. The
    /// variable a backend names is hidden when the gateway is built; this
    /// goes by the value, which is known once the key can be read, so it
    /// runs when the gateway is built and again when each turn's gateway
    /// starts (a key file may not have been there before). The process's
    /// environment does not change, and what is hidden stays hidden.
    ///
    /// # Errors
    ///
    /// When a variable that holds the token cannot be hidden ([`copies`]):
    /// nothing is hidden by this call then.
    pub(crate) fn hide_copies(&self) -> Result<Vec<String>, String> {
        let tokens = self.control_plane_tokens();
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let names = copies(std::env::vars_os(), &tokens)?;
        for name in &names {
            branchyard_support::child::hide(name);
        }
        Ok(names)
    }

    /// The same gateway, giving up on a backend whose answer goes quiet
    /// for `quiet` (by default ten minutes, as long as a model may think
    /// before it answers). A call whose backend went quiet after it was
    /// sent fails `502` and is not sent to another backend: it may have
    /// been billed.
    pub fn quiet_after(mut self, quiet: std::time::Duration) -> Gateway {
        self.transport = upstream::Transport::from_env(quiet);
        self
    }

    fn backend(&self, name: &str) -> Option<&Backend> {
        self.backends.iter().find(|b| b.name == name)
    }

    /// The backends of `order` a call may be sent to: never both kinds,
    /// so a refusal or an outage of one kind never moves the call to the
    /// other. A route that names a control-plane backend names only
    /// control-plane backends ([`Gateway::new`]), and this holds at run time
    /// whatever the routes say: its plain backends are dropped. A call no
    /// route takes (`routed` false) goes to the plain backends of its API
    /// when there are any, as before control planes existed, and to its
    /// control-plane backends only when it has no plain one.
    pub(crate) fn confine<'a>(&self, routed: bool, order: Vec<&'a Backend>) -> Vec<&'a Backend> {
        let planes = order.iter().any(|b| b.control_plane.is_some());
        let plain = order.iter().any(|b| b.control_plane.is_none());
        match planes && plain {
            true => order
                .into_iter()
                .filter(|b| b.control_plane.is_some() == routed)
                .collect(),
            false => order,
        }
    }

    /// Whether some backend speaks `api`.
    pub fn serves(&self, api: Api) -> bool {
        self.backends.iter().any(|b| b.api == api)
    }

    /// Whether a harness speaking `api`, allowed `models` (globs; none
    /// means any), is served: by a backend speaking `api`, or by a route
    /// that translates for one of its models.
    pub(crate) fn serves_for(&self, api: Api, models: &[String]) -> bool {
        self.serves(api)
            || self.routes.iter().any(|r| {
                r.bridges(self, api)
                    && (models.is_empty()
                        || models.iter().any(|m| {
                            branchyard_provision::connectors::glob_match(&r.model, m)
                                || branchyard_provision::connectors::glob_match(m, &r.model)
                        }))
            })
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
    /// `api` in order. Also the route's index, for its rate limit. A
    /// route naming a `backend_model` also offers its backends of an API
    /// the request translates to. Either way [`Gateway::confine`] has the
    /// last word: one call's backends are never of both kinds.
    pub(crate) fn plan(&self, api: Api, model: Option<&str>) -> (Option<usize>, Vec<&Backend>) {
        let (index, order) = self.unconfined_plan(api, model);
        (index, self.confine(index.is_some(), order))
    }

    fn unconfined_plan(&self, api: Api, model: Option<&str>) -> (Option<usize>, Vec<&Backend>) {
        let usable = |r: &Route, b: &Backend| {
            b.api == api || (r.backend_model.is_some() && api.translates_to(b.api))
        };
        let route = model.and_then(|m| {
            self.routes.iter().enumerate().find(|(_, r)| {
                branchyard_provision::connectors::glob_match(&r.model, m)
                    && r.backends
                        .iter()
                        .map(|(name, _)| name)
                        .chain(&r.fallbacks)
                        .filter_map(|name| self.backend(name))
                        .any(|b| usable(r, b))
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
            .filter(|b| usable(route, b))
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

/// Where backend `name`'s calls go, and with them its credential: its `url`
/// as written (a trailing `/` dropped), else, for a plain backend, its API's
/// public address. The one place a backend's address is decided, for the
/// gateway ([`Gateway::new`]) and for listing models
/// ([`discover::sources`]).
///
/// A control-plane backend has no default: without a `url` its calls would
/// go to the provider's public API carrying the control plane's credential
/// (a stored token, or a token the yard signs that is good at the control
/// plane for up to an hour), so it must name its access point. A `url`
/// written out is the operator's choice, whatever it names.
pub(crate) fn backend_url(name: &str, b: &BackendConfig) -> Result<String, String> {
    match (&b.url, b.control_plane, b.api.default_url()) {
        (Some(url), _, _) => Ok(url.trim_end_matches('/').to_owned()),
        (None, Some(plane), _) => Err(format!(
            "models.backends.{name}.url: a control-plane backend ({plane}) has no default \
             address: without a url its calls, and the credential they carry, would go to the \
             provider's public API; write its access point, \
             url = \"https://<aiplex>/ap/<access point>\""
        )),
        (None, None, Some(url)) => Ok(url.to_owned()),
        (None, None, None) => Err(format!(
            "models.backends.{name}: a generic backend needs a url"
        )),
    }
}

/// Whether variable `name` is one child processes need or Branchyard reads
/// itself, so it cannot hold a control plane's token, which is hidden from
/// every child: the shell's (`PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`,
/// `PWD`, `TMPDIR`, `TMP`, `TEMP`, `TERM`, `LANG`, `LANGUAGE`, `LC_*`),
/// `SSH_AUTH_SOCK`, git's (`GIT_*`), the proxies' (`*_PROXY`, `NO_PROXY`),
/// the trust bundles' (`SSL_CERT_*`, `CURL_CA_BUNDLE`,
/// `REQUESTS_CA_BUNDLE`) and Branchyard's (`BRANCHYARD_*`), in any case.
pub fn shared_variable(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    matches!(
        name.as_str(),
        "PATH"
            | "HOME"
            | "USER"
            | "LOGNAME"
            | "SHELL"
            | "PWD"
            | "TMPDIR"
            | "TMP"
            | "TEMP"
            | "TERM"
            | "LANG"
            | "LANGUAGE"
            | "SSH_AUTH_SOCK"
            | "NO_PROXY"
            | "CURL_CA_BUNDLE"
            | "REQUESTS_CA_BUNDLE"
    ) || ["LC_", "GIT_", "SSL_CERT_", "BRANCHYARD_"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || name.ends_with("_PROXY")
}

/// Refuse what a control-plane backend cannot be: one of a generic API
/// (an access point serves Anthropic's and OpenAI's), one without a key
/// (its credential, sent as a bearer token), or one with a compatibility
/// layer (an access point serves each API as the provider does).
fn check_control_plane(
    name: &str,
    plane: ControlPlane,
    api: Api,
    has_key: bool,
    compat: Compat,
) -> Result<(), String> {
    let at = format!("models.backends.{name}");
    if api == Api::Generic {
        return Err(format!(
            "{at}.control_plane: {plane} is for an anthropic or openai backend (an access \
             point serves those APIs), not generic"
        ));
    }
    if !has_key {
        return Err(format!(
            "{at}.key: a control-plane backend ({plane}) needs a key (the token it is called \
             with), or auth = \"turn-token\" with an audience on a server"
        ));
    }
    if compat != Compat::Off {
        return Err(format!(
            "{at}.compat: a control-plane backend ({plane}) takes no compatibility layer, \
             not {}",
            compat.as_str()
        ));
    }
    Ok(())
}

/// The audience of backend `name`'s signed token, when it is called with
/// one, or why its `auth` and `audience` are refused: a signed token goes
/// only to an AIPlex backend, needs an audience and no `key`, and a yard
/// whose tokens' issuer AIPlex cannot fetch keys from, or whose tenant it
/// cannot name, cannot sign one.
fn check_auth(name: &str, b: &BackendConfig, signer: &Signer) -> Result<Option<String>, String> {
    let at = format!("models.backends.{name}");
    match (b.auth, &b.audience) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(format!(
            "{at}.audience: is only for auth = \"turn-token\"; add it, or remove audience"
        )),
        (Some(BackendAuth::TurnToken), _) if b.control_plane.is_none() => Err(format!(
            "{at}.auth: a token Branchyard signs goes only to an AIPlex backend: add \
             control_plane = \"aiplex\", or use key"
        )),
        (Some(BackendAuth::TurnToken), None) => Err(format!(
            "{at}.audience: auth = \"turn-token\" needs audience = \"...\": the audience the \
             AIPlex binding expects"
        )),
        (Some(BackendAuth::TurnToken), Some(audience)) if audience.trim().is_empty() => Err(
            format!("{at}.audience: is empty; write the audience the AIPlex binding expects"),
        ),
        (Some(BackendAuth::TurnToken), Some(_)) if b.key.is_some() => Err(format!(
            "{at}: names both key and auth = \"turn-token\": one credential, not two; remove one"
        )),
        (Some(BackendAuth::TurnToken), Some(audience)) => {
            federable_issuer(&signer.issuer).map_err(|why| format!("{at}.auth: {why}"))?;
            if !federable_tenant(&signer.tenant) {
                return Err(format!(
                    "{at}.auth: the tenant {:?} cannot be named in the token's subject \
                     (branchyard:<tenant>, 1 to 128 of A-Z a-z 0-9 . _ @ + -); use key instead",
                    signer.tenant
                ));
            }
            Ok(Some(audience.clone()))
        }
    }
}

/// Refuse a control-plane backend that no route names when a plain
/// backend speaks its API: a call no route takes goes to the plain one
/// ([`Gateway::confine`]), so it would never be used.
/// Whether `value` holds the control plane's token `token`: it is the token,
/// or has it inside, as `Bearer <token>` has. A token too short to be used
/// (`MIN_CONTROL_PLANE_KEY`) is looked for only as the whole value, since
/// a few characters are inside many values that do not hold them.
pub(crate) fn holds(value: &str, token: &str) -> bool {
    value == token
        || (token.chars().count() >= gateway::MIN_CONTROL_PLANE_KEY && value.contains(token))
}

/// Of `environment` (a process's variables), the names of those whose value
/// holds one of `tokens` (a control plane's token, with a backend it is the
/// key of): equal to it, or with it inside, as `Bearer <token>` has. They
/// are hidden from every child, as the variable the backend names is.
///
/// Refused, naming the variable and never the value, when one of them
/// cannot be hidden: a variable child processes need or Branchyard reads
/// itself ([`shared_variable`]), or one whose name is not UTF-8, which no
/// rule can name.
fn copies(
    environment: impl Iterator<Item = (OsString, OsString)>,
    tokens: &[(String, String)],
) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for (name, value) in environment {
        let value = value.to_string_lossy();
        let Some((backend, _)) = tokens.iter().find(|(_, token)| holds(&value, token)) else {
            continue;
        };
        let Some(name) = name.to_str() else {
            return Err(format!(
                "models.backends.{backend}.key: a variable whose name is not UTF-8 holds the \
                 token too, and it cannot be hidden from child processes; take the token out \
                 of it"
            ));
        };
        if shared_variable(name) {
            return Err(format!(
                "models.backends.{backend}.key: the variable {name} holds the token too, and \
                 it cannot be hidden from child processes, which need {name} (or Branchyard \
                 reads it itself); take the token out of {name}"
            ));
        }
        names.push(name.to_owned());
    }
    names.sort();
    Ok(names)
}

/// Refuse a plain backend whose key is read from where a control-plane
/// backend's token is read: every call to it would carry the control
/// plane's credential to a provider.
fn check_shared_keys(backends: &[Backend]) -> Result<(), String> {
    for plain in backends.iter().filter(|b| b.control_plane.is_none()) {
        let Some(source) = &plain.key else {
            continue;
        };
        let plane = backends
            .iter()
            .find(|b| b.control_plane.is_some() && b.key.as_ref() == Some(source));
        if let Some(plane) = plane {
            return Err(format!(
                "models.backends.{plain}.key: it is read from {source}, where the token of the \
                 control-plane backend {plane} is read from, and a control plane's token is \
                 sent only to its own url; give {plain} a key of its own",
                plain = plain.name,
                plane = plane.name
            ));
        }
    }
    Ok(())
}

fn check_unrouted(backends: &[Backend], routes: &[Route]) -> Result<(), String> {
    for backend in backends.iter().filter(|b| b.control_plane.is_some()) {
        let named = routes.iter().any(|r| {
            r.backends.iter().any(|(n, _)| *n == backend.name)
                || r.fallbacks.contains(&backend.name)
        });
        let plain = backends
            .iter()
            .find(|b| b.api == backend.api && b.control_plane.is_none());
        if let (false, Some(plain)) = (named, plain) {
            return Err(format!(
                "models.backends.{name}: no route names this control-plane backend, and a call \
                 no route takes goes to the plain {api} backend {plain}, so it would never be \
                 used; add a route that names it ([[models.routes]] with backends = [\"{name}\"])",
                name = backend.name,
                api = backend.api,
                plain = plain.name,
            ));
        }
    }
    Ok(())
}

/// Why route `at` (for `model`), which names the control-plane backend
/// `plane` and the plain backend `plain`, is refused.
fn mixed_route(at: &str, model: &str, plane: &str, plain: &str) -> String {
    format!(
        "{at} ({model}): names the control-plane backend {plane} and the plain backend \
         {plain}; a route that names a control-plane backend names only control-plane \
         backends, or a refusal or an outage of the control plane would send its call to \
         {plain} directly"
    )
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
    /// `unauthorized`, `refused` (a control-plane backend refused it: its
    /// answer carried an error code of the control plane's own) or
    /// `failed` (no backend answered).
    pub decision: String,
    /// The backend that answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Why it was refused, or what each backend that failed did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The status the harness got.
    pub status: u16,
    /// The model the backend was asked for when the request was
    /// translated for an API the harness does not speak; usage is priced
    /// by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translated: Option<String>,
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
    /// What the call asked and what came back, as counts and digests, for
    /// a call forwarded to a backend; `None` for one the gateway refused.
    // Boxed so that `ModelActivity::Call` stays within Clippy's
    // `large_enum_variant` of the other variants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interception: Option<Box<Interception>>,
    /// With `[models] audit = "contents"`, the file holding this call's
    /// request and response, relative to the repository root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_file: Option<String>,
    /// For a call sent to a control-plane backend, its id and what the
    /// control plane answered about it; `None` for any other call.
    // Boxed, as `interception` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_plane: Option<Box<ControlPlaneCall>>,
}

/// A call sent to a control-plane backend, as its `model` event records it
/// (`docs/model-gateway.md`, "What is recorded").
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlPlaneCall {
    /// The control plane.
    pub plane: ControlPlane,
    /// The call's own id: the `X-Client-Request-Id` it was sent (the same
    /// on every control-plane backend tried) and the id of its usage
    /// record, when it has one.
    pub call_id: String,
    /// The control plane's own id of the call (`X-Aiplex-Request-Id`): the
    /// `request_id` of its record. Kept only when it is 1 to 128 of
    /// `A-Z a-z 0-9 - _ . :`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The control plane's code for a refusal it made itself
    /// (`X-Aiplex-Error-Code`), such as `QUOTA_BUDGET_EXHAUSTED`. Kept
    /// only when it is 1 to 64 of `A-Z 0-9 _`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// The record of one call that reached a backend, for an audit: who asked
/// is the call's branch and turn, and this is what it asked and what came
/// back, without the text of either. See `docs/model-gateway.md`
/// ("Interception records").
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interception {
    /// Bytes of the request body the harness sent.
    pub request_bytes: u64,
    /// Bytes of the response body the backend sent, in the backend's own
    /// shape (a translated call's, before it is turned back).
    pub response_bytes: u64,
    /// The BLAKE3 hash, in lowercase hex, of the request body exactly as
    /// the harness sent it: the bytes after the HTTP framing (a chunked
    /// coding) is removed and before the gateway changes anything (it adds
    /// `stream_options.include_usage` to an OpenAI chat stream, or
    /// translates the body). The same request always has the same digest;
    /// a request with no body has the hash of the empty input.
    pub prompt_digest: String,
    /// Items in the request's `messages` (Anthropic and OpenAI chat) or
    /// `input` (OpenAI responses; a string counts as one).
    pub messages: u32,
    /// Names of the tools in the request's `tools`, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_offered: Vec<String>,
    /// Names of the tools or functions the response called, in order, once
    /// per call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_uses: Vec<String>,
    /// Why the response stopped, in the provider's own word: Anthropic's
    /// `stop_reason`, OpenAI's `finish_reason` (chat) or `status`
    /// (responses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

impl Interception {
    pub(crate) fn new(asked: usage::Asked, seen: usage::Observed) -> Interception {
        Interception {
            request_bytes: asked.bytes,
            response_bytes: seen.bytes,
            prompt_digest: asked.digest,
            messages: asked.messages,
            tools_offered: asked.tools_offered,
            tool_uses: seen.tool_uses,
            stop_reason: seen.stop_reason,
        }
    }
}

impl ModelCall {
    /// `claude-sonnet-4-6 allowed via anthropic (200, 1200 in / 80 out,
    /// $0.0048, tools: Read, Edit, stop: tool_use, 812 ms)`.
    pub fn describe(&self) -> String {
        let model = match self.model.is_empty() {
            true => "(no model)",
            false => self.model.as_str(),
        };
        let mut out = format!("{model} {}", self.decision);
        if let Some(backend) = &self.backend {
            out.push_str(&format!(" via {backend}"));
        }
        if let Some(as_model) = &self.translated {
            out.push_str(&format!(" as {as_model}"));
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
        if let Some(interception) = &self.interception {
            // Each tool once, in the order first called.
            let mut used: Vec<&str> = Vec::new();
            for tool in &interception.tool_uses {
                if !used.contains(&tool.as_str()) {
                    used.push(tool);
                }
            }
            if !used.is_empty() {
                detail.push(format!("tools: {}", used.join(", ")));
            }
            if let Some(stop) = &interception.stop_reason {
                detail.push(format!("stop: {stop}"));
            }
        }
        detail.push(format!("{} ms", self.latency_ms));
        if let Some(plane) = &self.control_plane {
            if let Some(id) = &plane.request_id {
                detail.push(format!("{} request {id}", plane.plane));
            }
            // The id AIPlex's ledger holds as `client_request_id`.
            detail.push(format!("call {}", plane.call_id));
        }
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
        /// credentials it must not hold ([`scrub`]).
        scrub: Vec<String>,
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

/// The variables taken out of a turn's harness's environment on the
/// gateway: [`SCRUB`], the variable each control-plane backend's key is
/// read from, and every variable that holds such a key under another name
/// ([`Gateway::hide_copies`]), so a harness never holds a control plane's
/// credential. A plain backend's key variable is left as it was, unless it
/// holds that credential: a harness may read its provider's own variable
/// itself. This is a scrub of the environment, not a
/// boundary: a local harness runs as the same user, and can read a key file
/// or its parent's environment (`docs/model-gateway.md`, "The credential").
pub(crate) fn scrub(gateway: &Gateway) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = SCRUB.iter().map(|n| (*n).to_owned()).collect();
    for backend in gateway
        .backends
        .iter()
        .filter(|b| b.control_plane.is_some())
    {
        if let Some(KeySource::Env(var)) = &backend.key {
            if !names.contains(var) {
                names.push(var.clone());
            }
        }
    }
    // And every variable that holds such a token under another name, by
    // its value as it is now.
    for name in gateway.hide_copies()? {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names)
}

/// The secrets that log `harness` in by subscription, which the gateway
/// cannot stand in for: Claude Code's OAuth token, Codex's login file. Only
/// the harness that reads a secret is taken off the gateway by it: a fan
/// hands every candidate the same secrets, and one meant for another
/// harness (which its provisioner reports unused) leaves this one on the
/// gateway. Until 16 October 2026 any of them did, and a fan with
/// `--secret CODEX_AUTH` ran Oh My Pi and DeepSeek Harness direct.
fn subscription_secrets(harness: &str) -> &'static [&'static str] {
    match harness {
        "claude-code" => &["CLAUDE_CODE_OAUTH_TOKEN"],
        "codex" => &["CODEX_AUTH"],
        _ => &[],
    }
}
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
        .find(|s| subscription_secrets(&record.info.harness).contains(&s.name.as_str()))
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
    let models = spec
        .models
        .as_ref()
        .map(|m| m.allow.as_slice())
        .unwrap_or_default();
    match api {
        Some(api) if !gateway.serves_for(api, models) => Some(format!(
            "no backend serves the {api} API {} speaks, and no route translates for it",
            record.info.harness
        )),
        None if gateway.backends.is_empty() => Some("the gateway has no backends".to_owned()),
        _ => None,
    }
}

/// What the engine knows of a turn that its gateway needs beyond the
/// branch's record.
pub(crate) struct TurnInputs<'a> {
    /// The connector token, when the turn has one: it carries the model
    /// scope too. Otherwise the gateway's own is minted.
    pub token: Option<&'a str>,
    pub deadline_ms: Option<u64>,
    pub scopes: &'a TokenScopes,
    pub budget: crate::Budget,
    /// What the branch's children hold of its limit.
    pub reserved: f64,
    /// The turn's W3C trace context ([`crate::TaskOptions::trace_parent`]),
    /// which a control-plane backend is sent as `traceparent`.
    pub trace_parent: Option<&'a str>,
}

/// Prepare the turn's model access: `None` when the branch is not on the
/// gateway. A failure is the turn's, by name.
pub(crate) fn prepare(
    yard: &Yard,
    record: &Record,
    inputs: TurnInputs<'_>,
) -> Result<Option<Prepared>, String> {
    let TurnInputs {
        token,
        deadline_ms,
        scopes,
        budget,
        reserved,
        trace_parent,
    } = inputs;
    let Some(access) = record.provision.as_ref().and_then(|p| p.models.clone()) else {
        return Ok(None);
    };
    let gateway = yard.models().ok_or(
        "it is on the model gateway, but this yard has no [models] backends (docs/model-gateway.md)",
    )?;
    let spec = record
        .provision
        .as_ref()
        .ok_or("models imply provisioning, but this branch has none")?;
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
    // What the harness starts without, before anything is started for it.
    let scrub = scrub(&gateway)?;
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
            let claims = crate::connectors::turn_claims(crate::connectors::TurnToken {
                issuer: &gateway.signer.issuer,
                audience: &url,
                actor,
                by_branch: gateway.by_branch(&record.info.name),
                turn: record.info.turns + 1,
                window: (now, exp),
                grants: Vec::new(),
                scopes,
                manifest: None,
            })?;
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
        .filter(|a| gateway.serves_for(*a, &access.allow))
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
        harness: record.info.harness.clone(),
        trace_parent: trace_parent.map(str::to_owned),
        signer: Arc::clone(&keys),
        issuer: gateway.signer.issuer.clone(),
        tenant: record
            .actor
            .as_ref()
            .map_or_else(|| gateway.signer.tenant.clone(), |a| a.tenant.clone()),
        deadline_ms,
        signed: gateway::SignedTokens::default(),
        max_usd: budget.max_usd,
        cost_before: record.info.cost_usd.unwrap_or(0.0),
        held: reserved,
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
    if served.contains(&Api::Anthropic) {
        env.push(("ANTHROPIC_BASE_URL".to_owned(), format!("{url}/anthropic")));
        env.push(("ANTHROPIC_API_KEY".to_owned(), token.clone()));
    }
    if served.contains(&Api::Openai) {
        env.push(("OPENAI_BASE_URL".to_owned(), format!("{url}/openai/v1")));
        env.push(("OPENAI_API_KEY".to_owned(), token.clone()));
    }
    env.push((ENV_GATEWAY.to_owned(), url.clone()));
    Ok(Some(Prepared::Gateway {
        gateway: Box::new(running),
        env,
        scrub,
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
    use serde_json::{json, Value};

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
            signer: None,
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
            signer: None,
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
        // A subscription secret meant for another harness (a fan hands
        // every candidate the same ones) does not take this one off.
        let shared = serde_json::json!({"models": {"allow": ["*"]},
            "secrets": [{"name": "CODEX_AUTH"}, {"name": "CLAUDE_CODE_OAUTH_TOKEN"}]});
        for harness in ["oh-my-pi", "deepseek-harness", "gemini-cli"] {
            let other = record(harness, shared.clone());
            assert_eq!(direct_reason(&gateway, &other, None), None, "{harness}");
        }
        let codex = record("codex", shared.clone());
        let why = direct_reason(&gateway, &codex, harness_api("codex")).unwrap();
        assert!(why.contains("subscription (CODEX_AUTH)"), "{why}");
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
    fn a_route_naming_a_backend_model_translates_for_an_openai_backend() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "backends": {
                "o": {"api": "openai", "url": "http://127.0.0.1:4"},
                "g": {"api": "generic", "url": "http://127.0.0.1:5"}
            },
            "routes": [
                {"model": "claude-*", "backends": ["o"], "backend_model": "gpt-5"},
                {"model": "mistral-*", "backends": ["g"], "backend_model": "mistral"},
                {"model": "gpt-*", "backends": ["o"]}
            ]
        }))
        .unwrap();
        let gateway = Gateway::new(&config, signer(), |n| KeySource::Env(n.into())).unwrap();
        // The route's OpenAI backend serves the Anthropic request, asked for
        // the route's model.
        let (route, order) = gateway.plan(Api::Anthropic, Some("claude-sonnet-4-6"));
        assert_eq!(route, Some(0));
        assert_eq!(
            order.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            ["o"]
        );
        assert_eq!(gateway.routes[0].backend_model.as_deref(), Some("gpt-5"));
        // A generic backend is never translated to; without a route,
        // nothing crosses APIs.
        assert!(gateway
            .plan(Api::Anthropic, Some("mistral-large"))
            .1
            .is_empty());
        assert!(gateway.plan(Api::Anthropic, Some("llama-3")).1.is_empty());
        assert!(gateway.plan(Api::Anthropic, None).1.is_empty());
        // A harness stays on the gateway for the models the route covers,
        // and runs direct for others.
        for allow in [["claude-*"], ["*"], ["claude-opus-*"]] {
            let record = record(
                "claude-code",
                serde_json::json!({"models": {"allow": allow}}),
            );
            assert_eq!(
                direct_reason(&gateway, &record, harness_api("claude-code")),
                None,
                "{allow:?}"
            );
        }
        let mistral = record(
            "claude-code",
            serde_json::json!({"models": {"allow": ["mistral-*"]}}),
        );
        let why = direct_reason(&gateway, &mistral, harness_api("claude-code")).unwrap();
        assert!(why.contains("no route translates"), "{why}");
        assert!(gateway.serves_for(Api::Anthropic, &["claude-*".into()]));
        assert!(!gateway.serves_for(Api::Anthropic, &["mistral-*".into()]));
        assert!(gateway.serves_for(Api::Openai, &[]));
        assert!(!gateway.serves(Api::Anthropic));
        // Without a backend model, a route never bridges.
        let plain: Config = serde_json::from_value(serde_json::json!({
            "backends": {"o": {"api": "openai", "url": "http://127.0.0.1:4"}},
            "routes": [{"model": "claude-*", "backends": ["o"]}]
        }))
        .unwrap();
        let gateway = Gateway::new(&plain, signer(), |n| KeySource::Env(n.into())).unwrap();
        assert!(gateway
            .plan(Api::Anthropic, Some("claude-sonnet-4-6"))
            .1
            .is_empty());
        assert!(!gateway.serves_for(Api::Anthropic, &["claude-*".into()]));
        let claude = record(
            "claude-code",
            serde_json::json!({"models": {"allow": ["claude-*"]}}),
        );
        assert!(direct_reason(&gateway, &claude, harness_api("claude-code")).is_some());
        // The call says what the backend was asked for.
        let call = ModelCall {
            model: "claude-sonnet-4-6".into(),
            api: Api::Anthropic,
            decision: "allowed".into(),
            backend: Some("o".into()),
            reason: None,
            status: 200,
            translated: Some("gpt-5".into()),
            tokens: None,
            cost_usd: None,
            latency_ms: 12,
            streamed: false,
            failed_over: Vec::new(),
            interception: None,
            audit_file: None,
            control_plane: None,
        };
        assert_eq!(
            call.describe(),
            "claude-sonnet-4-6 allowed via o as gpt-5 (200, 12 ms)"
        );
    }

    fn answered() -> ModelCall {
        ModelCall {
            model: "claude-sonnet-4-6".into(),
            api: Api::Anthropic,
            decision: "allowed".into(),
            backend: Some("anthropic".into()),
            reason: None,
            status: 200,
            translated: None,
            tokens: Some(Tokens {
                input: 1000,
                output: 80,
                ..Tokens::default()
            }),
            cost_usd: Some(0.0048),
            latency_ms: 812,
            streamed: false,
            failed_over: Vec::new(),
            interception: Some(Box::new(Interception {
                request_bytes: 321,
                response_bytes: 654,
                prompt_digest: "0".repeat(64),
                messages: 3,
                tools_offered: vec!["Read".into(), "Edit".into(), "Bash".into()],
                tool_uses: vec!["Read".into(), "Edit".into(), "Read".into()],
                stop_reason: Some("tool_use".into()),
            })),
            audit_file: None,
            control_plane: None,
        }
    }

    #[test]
    fn a_call_describes_the_tools_it_used_and_why_it_stopped() {
        let mut call = answered();
        assert_eq!(
            call.describe(),
            "claude-sonnet-4-6 allowed via anthropic \
             (200, 1000 in / 80 out, $0.0048, tools: Read, Edit, stop: tool_use, 812 ms)"
        );
        // Nothing used and no reason: the line is what it was without an
        // interception.
        if let Some(interception) = call.interception.as_mut() {
            interception.tool_uses.clear();
            interception.stop_reason = None;
        }
        let plain = ModelCall {
            interception: None,
            ..call.clone()
        };
        assert_eq!(call.describe(), plain.describe());
        assert_eq!(
            plain.describe(),
            "claude-sonnet-4-6 allowed via anthropic (200, 1000 in / 80 out, $0.0048, 812 ms)"
        );
    }

    #[test]
    fn an_interception_round_trips_and_an_old_event_still_reads() {
        let call = answered();
        let text = serde_json::to_string(&call).unwrap();
        assert_eq!(serde_json::from_str::<ModelCall>(&text).unwrap(), call);
        // Absent fields are not written, and an event recorded before
        // interceptions existed reads with none.
        let mut value = serde_json::to_value(&call).unwrap();
        assert_eq!(value.get("audit_file"), None);
        value.as_object_mut().unwrap().remove("interception");
        let old: ModelCall = serde_json::from_value(value).unwrap();
        assert_eq!(old.interception, None);
        let bare = ModelCall {
            interception: Some(Box::new(Interception {
                tools_offered: Vec::new(),
                tool_uses: Vec::new(),
                stop_reason: None,
                ..*call.interception.clone().unwrap()
            })),
            ..call
        };
        let value = serde_json::to_value(&bare).unwrap();
        let written = value["interception"].as_object().unwrap();
        let mut keys: Vec<&str> = written.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "messages",
                "prompt_digest",
                "request_bytes",
                "response_bytes"
            ]
        );
    }

    #[test]
    fn audit_defaults_to_digests_and_refuses_other_values_by_name() {
        let config: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(config.audit, Audit::Digests);
        // The default is not written back.
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::json!({})
        );
        let config: Config =
            serde_json::from_value(serde_json::json!({"audit": "contents"})).unwrap();
        assert_eq!(config.audit, Audit::Contents);
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::json!({"audit": "contents"})
        );
        let gateway = Gateway::new(&config, signer(), |n| KeySource::Env(n.into())).unwrap();
        assert_eq!(gateway.audit, Audit::Contents);
        for bad in ["full", "Contents", ""] {
            let error = serde_json::from_value::<Config>(serde_json::json!({"audit": bad}))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(&format!("`{bad}`"))
                    && error.contains("`digests`")
                    && error.contains("`contents`"),
                "{bad:?}: {error}"
            );
        }
    }

    fn built(config: Value) -> Result<Gateway, String> {
        let config: Config = serde_json::from_value(config).map_err(|e| e.to_string())?;
        Gateway::new(&config, signer(), |n| KeySource::Env(n.into()))
    }

    fn names(order: &[&Backend]) -> Vec<String> {
        order.iter().map(|b| b.name.clone()).collect()
    }

    #[test]
    fn a_variable_that_holds_a_control_planes_token_is_named_or_refused() {
        const TOKEN: &str = "aiplex-token-0123456789";
        let tokens = [("ap".to_owned(), TOKEN.to_owned())];
        let environment = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value)))
                .collect::<Vec<_>>()
                .into_iter()
        };
        // As `aiplex dev env` leaves a shell: the token under the SDKs'
        // names too. Equal to it or with it inside; in name order.
        let bearer = format!("Bearer {TOKEN}");
        let shell = [
            ("GEMINI_API_KEY", TOKEN),
            ("AIPLEX_TOKEN", TOKEN),
            ("OPENAI_API_KEY", TOKEN),
            ("CUSTOM_HEADERS", bearer.as_str()),
            ("GOOGLE_GEMINI_BASE_URL", "http://127.0.0.1:7400/ap/ide"),
            ("GOOGLE_API_KEY", "another-key-0123456789"),
            ("ALMOST", "aiplex-token-012345678"),
            ("EMPTY", ""),
            ("PATH", "/usr/bin:/bin"),
        ];
        assert_eq!(
            copies(environment(&shell), &tokens).unwrap(),
            [
                "AIPLEX_TOKEN",
                "CUSTOM_HEADERS",
                "GEMINI_API_KEY",
                "OPENAI_API_KEY"
            ]
        );
        // Nothing to look for: nothing is named, whatever the variables.
        assert_eq!(
            copies(environment(&shell), &[]).unwrap(),
            Vec::<String>::new()
        );
        // A value that is not UTF-8 is still read.
        use std::os::unix::ffi::OsStringExt as _;
        let mut bytes = vec![0xff];
        bytes.extend_from_slice(TOKEN.as_bytes());
        let odd = vec![(OsString::from("ODD_VALUE"), OsString::from_vec(bytes))];
        assert_eq!(copies(odd.into_iter(), &tokens).unwrap(), ["ODD_VALUE"]);
        // A variable every child needs cannot be hidden: refused, hiding
        // none, naming the variable and the backend and never the value.
        let proxy = format!("http://user:{TOKEN}@proxy.example:3128");
        for (name, value) in [
            ("PATH", TOKEN),
            ("HOME", TOKEN),
            ("LC_ALL", TOKEN),
            ("GIT_DIR", TOKEN),
            ("HTTPS_PROXY", proxy.as_str()),
            ("https_proxy", proxy.as_str()),
            ("BRANCHYARD_ROOT", TOKEN),
        ] {
            let error = copies(
                environment(&[("GEMINI_API_KEY", TOKEN), (name, value)]),
                &tokens,
            )
            .unwrap_err();
            assert_eq!(
                error,
                format!(
                    "models.backends.ap.key: the variable {name} holds the token too, and it \
                     cannot be hidden from child processes, which need {name} (or Branchyard \
                     reads it itself); take the token out of {name}"
                )
            );
            assert!(!error.contains(TOKEN), "{error}");
        }
        // Nor can one whose name no rule can spell.
        let unnamed = vec![(OsString::from_vec(vec![0xff, b'K']), OsString::from(TOKEN))];
        let error = copies(unnamed.into_iter(), &tokens).unwrap_err();
        assert_eq!(
            error,
            "models.backends.ap.key: a variable whose name is not UTF-8 holds the token too, \
             and it cannot be hidden from child processes; take the token out of it"
        );
    }

    #[test]
    fn a_plain_backend_may_not_read_its_key_where_a_control_plane_reads_its_token() {
        let config = |plain_key: &str| {
            json!({
                "backends": {
                    "ap": {"api": "anthropic", "url": "https://aiplex.example.com/ap/ide",
                        "key": "AIPLEX_TOKEN_OF_THE_TEST", "control_plane": "aiplex"},
                    "ap-openai": {"api": "openai", "url": "https://aiplex.example.com/ap/ide",
                        "key": "AIPLEX_TOKEN_OF_THE_TEST", "control_plane": "aiplex"},
                    "plain": {"api": "anthropic", "key": plain_key}
                },
                "routes": [
                    {"model": "claude-*", "backends": ["ap"]},
                    {"model": "direct-*", "backends": ["plain"]}
                ]
            })
        };
        // Two control-plane backends share a token; a plain backend has
        // its own key.
        built(config("ANTHROPIC_KEY_OF_THE_TEST")).unwrap();
        assert_eq!(
            built(config("AIPLEX_TOKEN_OF_THE_TEST")).unwrap_err(),
            "models.backends.plain.key: it is read from $AIPLEX_TOKEN_OF_THE_TEST, where the \
             token of the control-plane backend ap is read from, and a control plane's token \
             is sent only to its own url; give plain a key of its own"
        );
    }

    #[test]
    fn the_tokens_a_gateway_guards_are_its_control_planes_stored_keys() {
        const TOKEN: &str = "aiplex-token-9876543210";
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("aiplex"), format!("{TOKEN}\n")).unwrap();
        std::fs::write(dir.path().join("short"), "fifteen-chars-x").unwrap();
        std::fs::write(dir.path().join("provider"), "sk-provider-0123456789").unwrap();
        let at = "https://aiplex.example.com/ap/ide";
        let config: Config = serde_json::from_value(json!({
            "backends": {
                "ap-a": {"api": "anthropic", "url": at, "key": "aiplex", "control_plane": "aiplex"},
                "ap-b": {"api": "openai", "url": at, "key": "aiplex", "control_plane": "aiplex"},
                "ap-gone": {"api": "anthropic", "url": at, "key": "gone", "control_plane": "aiplex"},
                "ap-short": {"api": "anthropic", "url": at, "key": "short", "control_plane": "aiplex"},
                "plain": {"api": "anthropic", "key": "provider"}
            },
            "routes": [
                {"model": "claude-*", "backends": ["ap-a", "ap-gone", "ap-short"]},
                {"model": "gpt-*", "backends": ["ap-b"]},
                {"model": "direct-*", "backends": ["plain"]}
            ]
        }))
        .unwrap();
        let root = dir.path().to_path_buf();
        let gateway =
            Gateway::new(&config, signer(), |name| KeySource::File(root.join(name))).unwrap();
        // One source read once; a key that is missing, too short to be
        // used, or a plain backend's is not a control plane's token.
        assert_eq!(
            gateway.control_plane_tokens(),
            [("ap-a".to_owned(), TOKEN.to_owned())]
        );
        assert_eq!(gateway.token_held_by(TOKEN).as_deref(), Some("ap-a"));
        assert_eq!(
            gateway.token_held_by(&format!("Bearer {TOKEN}")).as_deref(),
            Some("ap-a")
        );
        for key in [
            "sk-provider-0123456789",
            "fifteen-chars-x",
            "",
            "aiplex-token-987654321",
        ] {
            assert_eq!(gateway.token_held_by(key), None, "{key}");
        }
        // A gateway without a control plane reads nothing and guards nothing.
        let plain: Config = serde_json::from_value(json!({
            "backends": {"plain": {"api": "anthropic", "key": "aiplex"}}
        }))
        .unwrap();
        let root = dir.path().to_path_buf();
        let gateway =
            Gateway::new(&plain, signer(), |name| KeySource::File(root.join(name))).unwrap();
        assert!(gateway.control_plane_tokens().is_empty());
        assert_eq!(gateway.token_held_by(TOKEN), None);
    }

    /// [`built`], as a yard whose tokens' issuer is `issuer`.
    fn built_as(issuer: &str, config: Value) -> Result<Gateway, String> {
        let config: Config = serde_json::from_value(config).map_err(|e| e.to_string())?;
        let signer = Signer {
            issuer: issuer.into(),
            tenant: "local".into(),
            ..signer()
        };
        Gateway::new(&config, signer, |n| KeySource::Env(n.into()))
    }

    #[test]
    fn a_control_plane_backend_has_no_default_address() {
        let credentials = [
            json!({"key": "AIPLEX_TOKEN"}),
            json!({"auth": "turn-token", "audience": "https://aiplex.example.com"}),
        ];
        let backend = |api: &str, credential: &Value, url: Option<&str>| {
            let mut backend = json!({"api": api, "control_plane": "aiplex"});
            for (key, value) in credential.as_object().unwrap() {
                backend[key] = value.clone();
            }
            if let Some(url) = url {
                backend["url"] = json!(url);
            }
            json!({"backends": {"ap": backend}})
        };
        for api in ["anthropic", "openai"] {
            for credential in &credentials {
                // Without a url the provider's public API would be sent the
                // credential: refused, saying what a url looks like.
                let error =
                    built_as("https://by.example.com", backend(api, credential, None)).unwrap_err();
                assert_eq!(
                    error,
                    "models.backends.ap.url: a control-plane backend (aiplex) has no default \
                     address: without a url its calls, and the credential they carry, would go \
                     to the provider's public API; write its access point, \
                     url = \"https://<aiplex>/ap/<access point>\"",
                    "{api} {credential}"
                );
                // A url that is empty, blank or names no host is no address
                // either.
                for url in [
                    "",
                    " ",
                    "/",
                    "https://",
                    "https:///ap/ide",
                    "https://:8443/ap/ide",
                    "aiplex.example.com/ap/ide",
                    "https://aiplex.example.com/ap/ide ",
                    "https://token@aiplex.example.com/ap/ide",
                ] {
                    let error = built_as(
                        "https://by.example.com",
                        backend(api, credential, Some(url)),
                    )
                    .unwrap_err();
                    assert!(
                        error.starts_with("models.backends.ap.url: "),
                        "{url:?}: {error}"
                    );
                }
                // Written out, the url is the operator's choice, and it is
                // where the credential goes.
                for url in [
                    "https://aiplex.example.com/ap/ide",
                    "https://api.anthropic.com",
                ] {
                    let gateway = built_as(
                        "https://by.example.com",
                        backend(api, credential, Some(url)),
                    )
                    .unwrap();
                    assert_eq!(gateway.backends[0].url, url, "{api} {credential}");
                }
            }
        }
        // A plain backend keeps its API's public address.
        for (api, url) in [
            ("anthropic", "https://api.anthropic.com"),
            ("openai", "https://api.openai.com"),
        ] {
            let gateway = built(json!({"backends": {"p": {"api": api, "key": "K"}}})).unwrap();
            assert_eq!(gateway.backends[0].url, url);
        }
    }

    #[test]
    fn a_control_plane_backend_is_refused_where_it_could_be_bypassed() {
        let plane = |api: &str| {
            json!({"api": api, "url": "https://aiplex.test/ap/ide",
            "key": "aiplex", "control_plane": "aiplex"})
        };
        let plain = |api: &str| json!({"api": api, "url": "http://127.0.0.1:1", "key": "k"});
        for (config, needles) in [
            // A route that mixes the two, in its backends or its fallbacks,
            // is refused naming the route and both backends.
            (
                json!({"backends": {"ap": plane("anthropic"), "direct": plain("anthropic")},
                    "routes": [{"model": "claude-*", "backends": ["ap", "direct"]}]}),
                vec![
                    "models.routes[0] (claude-*)",
                    "control-plane backend ap",
                    "plain backend direct",
                ],
            ),
            (
                json!({"backends": {"ap": plane("anthropic"), "direct": plain("anthropic")},
                    "routes": [{"model": "gpt-*", "backends": ["direct"]},
                        {"model": "claude-*", "backends": ["ap"], "fallbacks": ["direct"]}]}),
                vec!["models.routes[1] (claude-*)", "ap", "direct"],
            ),
            (
                json!({"backends": {"ap": plane("anthropic"), "direct": plain("anthropic")},
                    "routes": [{"model": "claude-*", "backends": ["direct"], "fallbacks": ["ap"]}]}),
                vec!["models.routes[0] (claude-*)", "ap", "direct"],
            ),
            // A generic backend cannot be one.
            (
                json!({"backends": {"g": {"api": "generic", "url": "https://aiplex.test/ap/x",
                    "key": "aiplex", "control_plane": "aiplex"}}}),
                vec!["models.backends.g.control_plane", "not generic"],
            ),
            // One that no route names, beside a plain backend of its API,
            // would never be used: refused, saying what to add.
            (
                json!({"backends": {"ap": plane("openai"), "direct": plain("openai")}}),
                vec![
                    "models.backends.ap",
                    "no route names",
                    "plain openai backend direct",
                    "add a route that names it",
                    "backends = [\"ap\"]",
                ],
            ),
            (
                json!({"backends": {"ap": plane("anthropic"), "direct": plain("anthropic")},
                    "routes": [{"model": "claude-*", "backends": ["direct"]}]}),
                vec!["models.backends.ap", "no route names"],
            ),
            // It needs its token, and takes no compatibility layer.
            (
                json!({"backends": {"ap": {"api": "openai", "url": "https://aiplex.test/ap/x",
                    "control_plane": "aiplex"}}}),
                vec!["models.backends.ap.key", "needs a key"],
            ),
            (
                json!({"backends": {"ap": {"api": "openai", "url": "https://aiplex.test/ap/x",
                    "key": "aiplex", "control_plane": "aiplex", "compat": "versioned"}}}),
                vec!["models.backends.ap.compat", "versioned"],
            ),
            // The one value there is.
            (
                json!({"backends": {"ap": {"api": "openai", "key": "k",
                    "control_plane": "anvil"}}}),
                vec!["`anvil`", "`aiplex`"],
            ),
        ] {
            let error = built(config.clone()).unwrap_err();
            for needle in needles {
                assert!(
                    error.contains(needle),
                    "{needle:?} not in {error} ({config})"
                );
            }
        }
        // What the contract shows is accepted: one backend per API, both
        // control planes, a route each, and a plain route beside them.
        let gateway = built(json!({
            "backends": {"aiplex-anthropic": plane("anthropic"), "aiplex-openai": plane("openai"),
                "spare": plane("anthropic"), "direct": plain("openai")},
            "routes": [{"model": "claude-*", "backends": ["aiplex-anthropic"],
                    "fallbacks": ["spare"]},
                {"model": "gpt-*", "backends": ["aiplex-openai"]},
                {"model": "o3*", "backends": ["direct"]}]
        }))
        .unwrap();
        assert_eq!(
            gateway.backend("aiplex-openai").unwrap().control_plane,
            Some(ControlPlane::Aiplex)
        );
        assert_eq!(gateway.backend("direct").unwrap().control_plane, None);
        // Alone on its API, or beside a plain backend of another API, one
        // that no route names is used by calls no route takes.
        for config in [
            json!({"backends": {"ap": plane("openai")}}),
            json!({"backends": {"ap": plane("openai"), "direct": plain("anthropic")}}),
        ] {
            assert!(built(config.clone()).is_ok(), "{config}");
        }
        // Written back as it was read.
        let config: Config =
            serde_json::from_value(json!({"backends": {"ap": plane("openai")}})).unwrap();
        assert_eq!(
            serde_json::to_value(&config).unwrap()["backends"]["ap"]["control_plane"],
            "aiplex"
        );
        let config: Config =
            serde_json::from_value(json!({"backends": {"p": plain("openai")}})).unwrap();
        assert_eq!(
            serde_json::to_value(&config).unwrap()["backends"]["p"].get("control_plane"),
            None
        );
    }

    #[test]
    fn one_calls_backends_are_never_of_both_kinds() {
        let plane = |api: &str| {
            json!({"api": api, "url": "https://aiplex.test/ap/ide",
            "key": "aiplex", "control_plane": "aiplex"})
        };
        let plain = |api: &str| json!({"api": api, "url": "http://127.0.0.1:1", "key": "k"});
        let mut gateway = built(json!({
            "backends": {
                "ap1": plane("anthropic"), "ap2": plane("anthropic"), "ap3": plane("anthropic"),
                "apo": plane("openai"), "direct-a": plain("anthropic"), "direct-b": plain("anthropic")
            },
            "routes": [
                {"model": "claude-haiku-*", "backends": ["apo"], "backend_model": "gpt-5-mini"},
                {"model": "claude-*", "backends": ["ap1", "ap2"], "weights": [3, 1],
                    "fallbacks": ["ap3"]},
                {"model": "local-*", "backends": ["direct-b"]}
            ],
            "seed": 7
        }))
        .unwrap();
        // The weighted choice, the route's other backends and its fallbacks:
        // control planes only, every time.
        for _ in 0..200 {
            let (route, order) = gateway.plan(Api::Anthropic, Some("claude-sonnet-4-6"));
            assert_eq!(route, Some(1));
            let order = names(&order);
            assert!(
                order == ["ap1", "ap2", "ap3"] || order == ["ap2", "ap1", "ap3"],
                "{order:?}"
            );
        }
        // A translated route to a control plane offers only it.
        let (route, order) = gateway.plan(Api::Anthropic, Some("claude-haiku-4-5"));
        assert_eq!((route, names(&order)), (Some(0), vec!["apo".to_owned()]));
        // A route the operator sends to a plain backend stays as it is.
        let (route, order) = gateway.plan(Api::Anthropic, Some("local-7b"));
        assert_eq!(
            (route, names(&order)),
            (Some(2), vec!["direct-b".to_owned()])
        );
        // No route, on an API with plain backends: only those, in name
        // order, as before control planes existed; a model or none.
        let (route, order) = gateway.plan(Api::Anthropic, Some("llama-3"));
        assert_eq!(
            (route, names(&order)),
            (None, vec!["direct-a".into(), "direct-b".into()])
        );
        assert_eq!(
            names(&gateway.plan(Api::Anthropic, None).1),
            ["direct-a", "direct-b"]
        );
        // No route, on an API with only control planes: those.
        let (route, order) = gateway.plan(Api::Openai, Some("o3"));
        assert_eq!((route, names(&order)), (None, vec!["apo".to_owned()]));
        assert_eq!(names(&gateway.plan(Api::Openai, None).1), ["apo"]);
        // An order of both kinds is cut to one at run time: to the control
        // planes for a route (a route changed after the configuration was
        // checked still sends nothing to a plain backend), to the plain
        // backends for a call no route takes.
        let both: Vec<&Backend> = ["ap1", "direct-a", "ap2"]
            .iter()
            .map(|n| gateway.backend(n).unwrap())
            .collect();
        assert_eq!(names(&gateway.confine(true, both.clone())), ["ap1", "ap2"]);
        assert_eq!(names(&gateway.confine(false, both)), ["direct-a"]);
        gateway.routes[1].fallbacks.push("direct-a".into());
        gateway.routes[1].backends.push(("direct-a".into(), 5));
        for _ in 0..50 {
            let (_, order) = gateway.plan(Api::Anthropic, Some("claude-opus-4"));
            assert!(
                order.iter().all(|b| b.control_plane.is_some()),
                "{:?}",
                names(&order)
            );
            assert_eq!(order.len(), 3);
        }
        // Without a control plane, nothing changes: every backend of the
        // API, in name order.
        let gateway = built(json!({
            "backends": {"b": plain("anthropic"), "a": plain("anthropic")}
        }))
        .unwrap();
        assert_eq!(
            names(&gateway.plan(Api::Anthropic, Some("x")).1),
            ["a", "b"]
        );
        let a = gateway.backend("a").unwrap();
        assert_eq!(names(&gateway.confine(false, vec![a])), ["a"]);
    }

    #[test]
    fn a_control_planes_refusal_and_its_id_read_in_by_log() {
        let call = ModelCall {
            model: "claude-sonnet-4-6".into(),
            api: Api::Anthropic,
            decision: "refused".into(),
            backend: Some("aiplex-anthropic".into()),
            reason: Some("refused by aiplex (QUOTA_BUDGET_EXHAUSTED)".into()),
            status: 429,
            translated: None,
            tokens: None,
            cost_usd: None,
            latency_ms: 4,
            streamed: false,
            failed_over: Vec::new(),
            interception: None,
            audit_file: None,
            control_plane: Some(Box::new(ControlPlaneCall {
                plane: ControlPlane::Aiplex,
                call_id: "AAECAwQFBgcICQoLDA0ODw".into(),
                request_id: Some("req-17".into()),
                error_code: Some("QUOTA_BUDGET_EXHAUSTED".into()),
            })),
        };
        assert_eq!(
            call.describe(),
            "claude-sonnet-4-6 refused via aiplex-anthropic \
             (429, refused by aiplex (QUOTA_BUDGET_EXHAUSTED), 4 ms, aiplex request req-17, \
             call AAECAwQFBgcICQoLDA0ODw)"
        );
        let value = serde_json::to_value(&call).unwrap();
        assert_eq!(
            value["control_plane"],
            json!({"plane": "aiplex", "call_id": "AAECAwQFBgcICQoLDA0ODw",
                "request_id": "req-17", "error_code": "QUOTA_BUDGET_EXHAUSTED"})
        );
        assert_eq!(serde_json::from_value::<ModelCall>(value).unwrap(), call);
        // A call of a plain backend writes neither, and reads back without.
        let value = serde_json::to_value(answered()).unwrap();
        assert_eq!(value.get("control_plane"), None);
    }

    #[test]
    fn a_control_planes_token_may_not_be_a_variable_children_need() {
        let plane = |var: &str| {
            json!({"backends": {"ap": {"api": "openai", "url": "https://aiplex.test/ap/x",
                "key": var, "control_plane": "aiplex"}}})
        };
        for var in [
            "PATH",
            "path",
            "HOME",
            "LC_ALL",
            "HTTPS_PROXY",
            "https_proxy",
            "NO_PROXY",
            "GIT_DIR",
            "SSL_CERT_FILE",
            "REQUESTS_CA_BUNDLE",
            "SSH_AUTH_SOCK",
            "BRANCHYARD_TOKEN",
        ] {
            let error = built(plane(var)).unwrap_err();
            assert!(
                error.contains("models.backends.ap.key")
                    && error.contains(var)
                    && error.contains("a variable of its own"),
                "{var}: {error}"
            );
            assert!(shared_variable(var), "{var}");
        }
        // An ordinary name works, and is hidden from children from then on.
        assert!(built(plane("BY_TEST_UNIT_AIPLEX_TOKEN")).is_ok());
        assert!(
            branchyard_support::child::hidden().contains(&"BY_TEST_UNIT_AIPLEX_TOKEN".to_owned())
        );
        for var in ["AIPLEX_TOKEN", "MY_PATH_TOKEN", "PROXY_TOKEN"] {
            assert!(!shared_variable(var), "{var}");
        }
        // A plain backend's key is not hidden, whatever its name.
        assert!(built(json!({"backends": {"p": {"api": "openai", "key": "HOME"}}})).is_ok());
    }

    #[test]
    fn a_signed_token_needs_an_aiplex_backend_an_audience_and_an_issuer_aiplex_can_reach() {
        let signed = |issuer: &str, tenant: &str, backend: Value| {
            let config: Config = serde_json::from_value(json!({"backends": {"ap": backend}}))
                .map_err(|e| e.to_string())?;
            let signer = Signer {
                issuer: issuer.into(),
                tenant: tenant.into(),
                ..signer()
            };
            Gateway::new(&config, signer, |n| KeySource::Env(n.into()))
        };
        let plane = json!({"api": "anthropic", "url": "https://aiplex.example.com/ap/ide",
            "control_plane": "aiplex", "auth": "turn-token",
            "audience": "https://aiplex.example.com"});
        // A public https issuer, and plain http on loopback only.
        for issuer in [
            "https://by.example.com",
            "https://by.example.com:8443",
            "http://127.0.0.1:7400",
            "http://[::1]:7400",
            "http://localhost:7400",
        ] {
            let gateway = signed(issuer, "local", plane.clone()).unwrap();
            assert_eq!(
                gateway.backends[0].audience.as_deref(),
                Some("https://aiplex.example.com")
            );
            assert_eq!(gateway.backends[0].key, None);
        }
        for (issuer, needle) in [
            ("branchyard:local:abc", "a local yard"),
            ("http://10.0.0.5:7400", "not loopback"),
            ("https://by.example.com/", "has a path"),
            ("https://by.example.com/branchyard", "has a path"),
            ("by.example.com", "not an http:// or https:// URL"),
            ("https://by.example.com.", "dot"),
            ("https://By.example.com", "upper-case"),
            ("https://by.example.com:443", "default port"),
            ("http://127.0.0.1:80", "default port"),
            ("https://by.example.com:", "empty port"),
            ("https://0.0.0.0:8443", "unspecified address"),
        ] {
            let error = signed(issuer, "local", plane.clone()).unwrap_err();
            assert!(
                error.contains("models.backends.ap.auth") && error.contains(needle),
                "{issuer}: {error}"
            );
        }
        let error = signed(issuer_ok(), "acme corp", plane.clone()).unwrap_err();
        assert!(
            error.contains("tenant") && error.contains("use key"),
            "{error}"
        );
        let local = signed("branchyard:local:abc", "local", plane.clone()).unwrap_err();
        assert!(local.contains("use key"), "{local}");
        // The shape of the backend, by name.
        let with = |extra: Value| {
            let mut backend = json!({"api": "anthropic", "url": "https://aiplex.example.com/ap/x"});
            for (k, v) in extra.as_object().unwrap() {
                backend[k] = v.clone();
            }
            signed(issuer_ok(), "local", backend)
        };
        for (extra, needles) in [
            (
                json!({"auth": "turn-token", "audience": "a", "key": "k"}),
                vec!["models.backends.ap.auth", "control_plane"],
            ),
            (
                json!({"control_plane": "aiplex", "auth": "turn-token"}),
                vec!["models.backends.ap.audience", "needs audience"],
            ),
            (
                json!({"control_plane": "aiplex", "auth": "turn-token", "audience": ""}),
                vec!["models.backends.ap.audience", "is empty"],
            ),
            (
                json!({"control_plane": "aiplex", "auth": "turn-token", "audience": "a",
                    "key": "AIPLEX_TOKEN"}),
                vec!["one credential, not two"],
            ),
            (
                json!({"control_plane": "aiplex", "key": "AIPLEX_TOKEN", "audience": "a"}),
                vec!["models.backends.ap.audience", "only for auth"],
            ),
            (
                json!({"control_plane": "aiplex", "auth": "oidc"}),
                vec!["`oidc`", "`turn-token`"],
            ),
        ] {
            let error = with(extra.clone()).unwrap_err();
            for needle in needles {
                assert!(
                    error.contains(needle),
                    "{needle:?} not in {error} ({extra})"
                );
            }
        }
        assert!(federable_tenant("acme.eu_1@x+y-z"));
        assert!(!federable_tenant(""));
        assert!(!federable_tenant(&"a".repeat(129)));
        assert!(!federable_tenant("a/b"));
    }

    #[test]
    fn an_issuer_is_refused_for_what_is_wrong_with_it_saying_what_to_write() {
        for issuer in [
            "https://by.example.com",
            "https://by.example.com:8443",
            "https://127.0.0.1",
            "http://127.0.0.1",
            "http://127.0.0.1:7400",
            "http://[::1]:7400",
            "http://[::1]",
            "http://localhost:7400",
            "https://[2001:db8::1]:8443",
        ] {
            assert_eq!(federable_issuer(issuer), Ok(()), "{issuer}");
        }
        let spelled = |issuer: &str, how: &str, write: &str| {
            (
                issuer.to_owned(),
                format!(
                    "the issuer {issuer:?} {how}; the issuer must be spelled the same in \
                     Branchyard and in AIPlex, so Branchyard takes one spelling (a lower-case \
                     host, no trailing dot, no default port): write {write:?}"
                ),
            )
        };
        let refused = [
            (
                "branchyard:local:abc".to_owned(),
                "this is a local yard (its tokens' issuer is branchyard:local:abc), which AIPlex \
                 cannot fetch keys from, so it cannot be federated; use key = \"...\" (an AIPlex \
                 token) instead"
                    .to_owned(),
            ),
            (
                "by.example.com".to_owned(),
                "the issuer \"by.example.com\" is not an http:// or https:// URL, so AIPlex \
                 cannot fetch keys from it"
                    .to_owned(),
            ),
            (
                "HTTPS://by.example.com".to_owned(),
                "the issuer \"HTTPS://by.example.com\" is not an http:// or https:// URL, so \
                 AIPlex cannot fetch keys from it"
                    .to_owned(),
            ),
            (
                "https://by.example.com/".to_owned(),
                "the issuer \"https://by.example.com/\" has a path; AIPlex wants \
                 scheme://host[:port] and nothing after"
                    .to_owned(),
            ),
            (
                "https://by.example.com/branchyard".to_owned(),
                "the issuer \"https://by.example.com/branchyard\" has a path; AIPlex wants \
                 scheme://host[:port] and nothing after"
                    .to_owned(),
            ),
            (
                "https://by.example.com?x=1".to_owned(),
                "the issuer \"https://by.example.com?x=1\" has a query or a fragment; AIPlex \
                 wants scheme://host[:port] and nothing after"
                    .to_owned(),
            ),
            // The two an operator may type: named, not left to the URL
            // parser's "has an invalid port".
            (
                "http://::1:7400".to_owned(),
                "the issuer \"http://::1:7400\" names an IPv6 address without brackets; write \
                 the address in brackets, scheme://[address]:port"
                    .to_owned(),
            ),
            (
                "https://2001:db8::1".to_owned(),
                "the issuer \"https://2001:db8::1\" names an IPv6 address without brackets; \
                 write the address in brackets, scheme://[address]:port"
                    .to_owned(),
            ),
            (
                "https://by.example.com:".to_owned(),
                "the issuer \"https://by.example.com:\" has an empty port; write the port \
                 after the colon, or leave the colon out"
                    .to_owned(),
            ),
            (
                "http://[::1]:".to_owned(),
                "the issuer \"http://[::1]:\" has an empty port; write the port after the \
                 colon, or leave the colon out"
                    .to_owned(),
            ),
            (
                "https://by.example.com:port".to_owned(),
                "the issuer \"https://by.example.com:port\" is not a URL AIPlex can fetch keys \
                 from: \"https://by.example.com:port\" has an invalid port"
                    .to_owned(),
            ),
            (
                "https://user@by.example.com".to_owned(),
                "the issuer \"https://user@by.example.com\" is not a URL AIPlex can fetch keys \
                 from: \"https://user@by.example.com\" has no usable host"
                    .to_owned(),
            ),
            // An address a server listens on, which nothing reaches.
            (
                "https://0.0.0.0:8443".to_owned(),
                "the issuer \"https://0.0.0.0:8443\" names an unspecified address, which a \
                 server listens on and nothing can be reached at, so AIPlex cannot fetch keys \
                 from it"
                    .to_owned(),
            ),
            (
                "https://[::]:8443".to_owned(),
                "the issuer \"https://[::]:8443\" names an unspecified address, which a server \
                 listens on and nothing can be reached at, so AIPlex cannot fetch keys from it"
                    .to_owned(),
            ),
            (
                "http://0.0.0.0:7400".to_owned(),
                "the issuer \"http://0.0.0.0:7400\" names an unspecified address, which a \
                 server listens on and nothing can be reached at, so AIPlex cannot fetch keys \
                 from it"
                    .to_owned(),
            ),
            (
                "https://[::ffff:0.0.0.0]".to_owned(),
                "the issuer \"https://[::ffff:0.0.0.0]\" names an unspecified address, which a \
                 server listens on and nothing can be reached at, so AIPlex cannot fetch keys \
                 from it"
                    .to_owned(),
            ),
            (
                "http://10.0.0.5:7400".to_owned(),
                "the issuer \"http://10.0.0.5:7400\" is plain http on a host that is not \
                 loopback; AIPlex fetches keys over https (http only from this machine, for a \
                 development AIPlex)"
                    .to_owned(),
            ),
            // One spelling, so the two sides cannot differ by a dot.
            spelled(
                "https://by.example.com.",
                "ends its host with a dot",
                "https://by.example.com",
            ),
            spelled(
                "https://By.Example.com:8443",
                "has an upper-case letter in its host",
                "https://by.example.com:8443",
            ),
            spelled(
                "https://by.example.com:443",
                "writes out the default port of https",
                "https://by.example.com",
            ),
            spelled(
                "http://127.0.0.1:80",
                "writes out the default port of http",
                "http://127.0.0.1",
            ),
            spelled(
                "http://LOCALHOST:7400",
                "has an upper-case letter in its host",
                "http://localhost:7400",
            ),
            spelled(
                "https://by.example.com:08443",
                "writes its port with a leading zero",
                "https://by.example.com:8443",
            ),
            spelled(
                "https://BY.example.com.:443",
                "ends its host with a dot and has an upper-case letter in its host and writes \
                 out the default port of https",
                "https://by.example.com",
            ),
            spelled(
                "https://[2001:DB8::1]",
                "has an upper-case letter in its host",
                "https://[2001:db8::1]",
            ),
        ];
        for (issuer, message) in &refused {
            assert_eq!(federable_issuer(issuer).as_ref(), Err(message), "{issuer}");
        }
        // The one spelling, for whoever has to say what to write: the
        // spelling alone, whatever else is wrong with the issuer.
        for (issuer, canonical) in [
            ("https://by.example.com", Some("https://by.example.com")),
            (
                "https://BY.example.com.:443",
                Some("https://by.example.com"),
            ),
            ("http://127.0.0.1:80", Some("http://127.0.0.1")),
            ("http://127.0.0.1:7400", Some("http://127.0.0.1:7400")),
            ("https://[2001:DB8::1]:443", Some("https://[2001:db8::1]")),
            ("https://0.0.0.0:8443", Some("https://0.0.0.0:8443")),
            ("https://by.example.com/", None),
            ("https://by.example.com:", None),
            ("http://::1:7400", None),
            ("branchyard:local:abc", None),
            ("", None),
        ] {
            assert_eq!(canonical_issuer(issuer).as_deref(), canonical, "{issuer}");
        }
        // What is written the one way and is refused is refused for
        // something else, and what is accepted is its own spelling.
        for issuer in ["https://by.example.com", "http://[::1]:7400"] {
            assert_eq!(canonical_issuer(issuer).as_deref(), Some(issuer));
        }
    }

    fn issuer_ok() -> &'static str {
        "https://by.example.com"
    }

    #[test]
    fn a_bad_configuration_is_refused_by_name() {
        let signer = || Signer {
            issuer: "i".into(),
            key_file: "/nonexistent".into(),
            jwks_file: None,
            signer: None,
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
