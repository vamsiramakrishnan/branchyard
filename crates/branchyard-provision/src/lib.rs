// Partly derived from Scion (https://github.com/GoogleCloudPlatform/scion)
// at d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: the provisioning contract
// of harnesses/authoring-guide.md and harnesses/scion_harness.py (staged
// secrets, MCP servers, instructions and telemetry in; native files and an
// environment overlay out), and _resolve_reasoning_effort of
// harnesses/codex/provision.py. Copyright 2026 Google LLC. Licensed under
// the Apache License, Version 2.0.
//
// Modified for Branchyard: a typed Rust contract whose planning does no
// I/O, instead of a Python script run in the container; secrets resolved by
// the caller instead of staged files; session channels preferred to native
// configuration; writes limited to a home private to the branch.

//! Harness provisioning: prepare a harness's home before it starts.
//!
//! In a sandbox, or with a private home, a harness finds none of your
//! setup: no login, no MCP servers, no settings. A [`Provisioner`] per
//! harness turns a [`Context`] (secrets already resolved, MCP servers,
//! instructions, model, reasoning effort, telemetry, and where the home and
//! workspace are as the harness sees them) into a [`Plan`]: edits of files
//! in the home, variables for the harness process, and what the driver
//! passes in the session itself. Planning does no I/O, so every plan is
//! testable as data; [`apply::apply`] carries a plan out on a directory.
//!
//! The design and the per-harness mapping come from Scion's harness
//! provisioners (GoogleCloudPlatform/scion, Apache-2.0), which do the same
//! inside a container before start. The modules that translate them name
//! their origin and what changed; `docs/provisioning.md` gives the whole
//! mapping and what was not ported.
//!
//! Rules every provisioner follows:
//!
//! - MCP servers and standing instructions go through the driver's session
//!   channel when it has one (Claude Code's `--mcp-config` and plugin,
//!   Codex's thread configuration, ACP's `session/new`). Only a harness
//!   whose driver has none gets them in its native configuration, and only
//!   in a private home.
//! - Nothing is written unless the home is private to the branch
//!   ([`Context::private_home`]); a plan that needs a file there is refused.
//! - Files are edited, not replaced: JSON keys are merged, TOML keys and
//!   tables reconciled, instructions kept between markers, so what a person
//!   or the harness put there stays. A file that cannot be parsed is not
//!   overwritten.
//! - Secret values appear only in [`FileEdit`]s and [`EnvVar`]s marked
//!   `secret`, whose `Debug` output is redacted. Files holding one are
//!   written with mode 0600. Nothing in this crate prints or logs.

pub mod apply;
pub mod auth;
pub mod connectors;
pub mod edit;
pub mod instructions;
pub mod models;
pub mod network;
pub mod toml;

mod antigravity;
mod claude;
mod codex;
mod copilot;
mod gemini;
mod generic;
mod hermes;
mod opencode;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

pub use branchyard_harness::profiles::Protocol;
pub use branchyard_harness::{Instructions, McpServer, RemoteMcpServer, RemoteTransport};
use serde::{Deserialize, Serialize};

pub use edit::{Edit, JsonEdit, TomlEdit};

/// The Scion revision the translated provisioners follow.
pub const SCION_REVISION: &str = "d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338";

/// What a task asks to have provisioned. Stored with the branch, so it
/// holds where secrets come from, never their values; every turn resolves
/// them again.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provisioning {
    /// Credentials by the name the harness knows them by, such as
    /// `ANTHROPIC_API_KEY` or `CODEX_AUTH`. Each harness uses the ones its
    /// provisioner reads; the rest are reported as unused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretSource>,
    /// The authentication method to use when the secrets allow several,
    /// such as `api-key` or `oauth-token`. Unset: the harness's order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// Stdio MCP servers the harness starts, besides Branchyard's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerSpec>,
    /// HTTP and SSE MCP servers the harness connects to, where it can.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_mcp_servers: Vec<RemoteMcpSpec>,
    /// Standing instructions, besides Branchyard's delegation skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// A model name, or a size alias (`small`, `medium`, `large`,
    /// `extra-large`, or `s`, `m`, `l`, `xl`) where the harness defines one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<Telemetry>,
    /// Connectors the harness may call through the connector gateway, and
    /// what it may do with each (`--connector`); see `docs/connectors.md`.
    /// Needs a private home, like secrets. A delegated child's is narrowed
    /// to its parent's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connectors: Vec<connectors::GrantEntry>,
    /// Which hosts the harness may reach (`--network`): `open` (unset),
    /// `none`, or an allowlist, enforced through Branchyard's egress
    /// proxy; see `docs/egress.md`. A connector grant adds the gateway's
    /// host. A delegated child's is narrowed to its parent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<network::Network>,
    /// Whether the harness calls its model provider through Branchyard's
    /// model gateway (`--model-gateway`), and which models it may call
    /// there; see `docs/model-gateway.md`. Unset: the harness calls its
    /// provider itself. A delegated child of a branch on the gateway stays
    /// on it, within its parent's models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<models::ModelAccess>,
}

impl Provisioning {
    /// Whether nothing is asked for.
    pub fn is_empty(&self) -> bool {
        self == &Provisioning::default()
    }
}

/// Where a secret's value comes from.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSource {
    /// The name the harness knows it by.
    pub name: String,
    /// Unset: the variable of the same name, in the environment of the
    /// process that runs the turn (on a server, the server's own table of
    /// secrets decides).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<SecretFrom>,
}

/// A variable or a file on the host that runs the turn.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretFrom {
    Env { var: String },
    File { path: PathBuf },
}

impl SecretSource {
    /// Parse `NAME`, `NAME=VAR` or `NAME=@FILE`.
    pub fn parse(text: &str) -> Result<SecretSource, String> {
        let (name, from) = match text.split_once('=') {
            None => (text, None),
            Some((name, source)) => match source.strip_prefix('@') {
                Some("") => return Err(format!("secret {name} names an empty file")),
                Some(path) => (
                    name,
                    Some(SecretFrom::File {
                        path: PathBuf::from(path),
                    }),
                ),
                None => (
                    name,
                    Some(SecretFrom::Env {
                        var: source.to_owned(),
                    }),
                ),
            },
        };
        check_variable_name(name).map_err(|why| format!("secret name {name:?} {why}"))?;
        if let Some(SecretFrom::Env { var }) = &from {
            check_variable_name(var).map_err(|why| format!("variable {var:?} {why}"))?;
        }
        Ok(SecretSource {
            name: name.to_owned(),
            from,
        })
    }
}

/// `[A-Za-z_][A-Za-z0-9_-]*`, as secret and variable names must be.
pub fn check_variable_name(name: &str) -> Result<(), &'static str> {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {}
        _ => return Err("must start with a letter or underscore"),
    }
    if bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        Ok(())
    } else {
        Err("may hold only letters, digits, '_' and '-'")
    }
}

/// A stdio MCP server, in a form that is stored and sent over the API.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerSpec {
    pub name: String,
    /// Absolute path of the executable, as the harness sees it.
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Stored with the branch like the rest of the request: not for
    /// secrets. Use [`McpServerSpec::secret_env`] for those.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Variables set from the task's secrets, by variable and secret name.
    /// Each turn resolves them with the other secrets; only the names are
    /// stored.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_env: BTreeMap<String, String>,
}

impl McpServerSpec {
    /// Parse `NAME=COMMAND ARG...`, the command split on whitespace.
    pub fn parse(text: &str) -> Result<McpServerSpec, String> {
        let (name, command) = text
            .split_once('=')
            .ok_or_else(|| format!("an MCP server is NAME=COMMAND, not {text:?}"))?;
        let mut words = command.split_whitespace().map(str::to_owned);
        let command = words
            .next()
            .ok_or_else(|| format!("MCP server {name} has no command"))?;
        let spec = McpServerSpec {
            name: name.to_owned(),
            command,
            args: words.collect(),
            env: BTreeMap::new(),
            secret_env: BTreeMap::new(),
        };
        spec.check()?;
        Ok(spec)
    }

    /// The same checks the drivers make, so a bad server is refused before
    /// a branch exists.
    pub fn check(&self) -> Result<(), String> {
        let name_ok = !self.name.is_empty()
            && self.name.len() <= 64
            && self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !name_ok {
            return Err(format!(
                "MCP server name {:?} is not [A-Za-z0-9_-]{{1,64}}",
                self.name
            ));
        }
        if !self.command.starts_with('/') {
            return Err(format!(
                "MCP server {} needs an absolute command, not {:?}",
                self.name, self.command
            ));
        }
        for var in self.env.keys().chain(self.secret_env.keys()) {
            check_variable_name(var)
                .map_err(|why| format!("MCP server {} variable {var:?} {why}", self.name))?;
        }
        for (var, secret) in &self.secret_env {
            if self.env.contains_key(var) {
                return Err(format!(
                    "MCP server {} sets {var} both plainly and from a secret",
                    self.name
                ));
            }
            check_variable_name(secret)
                .map_err(|why| format!("MCP server {} secret {secret:?} {why}", self.name))?;
        }
        Ok(())
    }

    /// The server with its plain variables only.
    pub fn server(&self) -> McpServer {
        McpServer {
            name: self.name.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            env: self
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    /// The server with its variables from `secrets` too, and which secret
    /// each came from. Fails naming a secret that is not given.
    pub fn resolve(&self, secrets: &[Secret]) -> Result<(McpServer, Vec<McpSecret>), String> {
        let mut server = self.server();
        let mut used = Vec::new();
        for (var, name) in &self.secret_env {
            let secret = secrets.iter().find(|s| &s.name == name).ok_or_else(|| {
                format!(
                    "MCP server {} needs the secret {name}, which the task does not give",
                    self.name
                )
            })?;
            server.env.push((var.clone(), secret.value.clone()));
            used.push(McpSecret {
                secret: name.clone(),
                via: Via::McpServer {
                    server: self.name.clone(),
                    var: var.clone(),
                },
            });
        }
        Ok((server, used))
    }
}

/// An HTTP or SSE MCP server, in a form that is stored and sent over the
/// API. Every header's value is one of the task's secrets, named here and
/// resolved each turn, so no header value is stored.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteMcpSpec {
    pub name: String,
    /// `http` (streamable HTTP, the default) or `sse`.
    #[serde(default, skip_serializing_if = "is_http")]
    pub transport: RemoteMcpTransport,
    pub url: String,
    /// Header name to the name of the secret holding its value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

/// [`RemoteMcpSpec::transport`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteMcpTransport {
    #[default]
    Http,
    Sse,
}

fn is_http(transport: &RemoteMcpTransport) -> bool {
    *transport == RemoteMcpTransport::Http
}

impl RemoteMcpSpec {
    /// The same checks the drivers make, so a bad server is refused before
    /// a branch exists.
    pub fn check(&self) -> Result<(), String> {
        let probe = McpServerSpec {
            name: self.name.clone(),
            command: "/".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            secret_env: BTreeMap::new(),
        };
        probe.check()?;
        branchyard_harness::check_remote_url(&self.url)
            .map_err(|why| format!("MCP server {}: {why}", self.name))?;
        for (header, secret) in &self.headers {
            branchyard_harness::check_header(header, "")
                .map_err(|why| format!("MCP server {}: {why}", self.name))?;
            check_variable_name(secret)
                .map_err(|why| format!("MCP server {} secret {secret:?} {why}", self.name))?;
        }
        Ok(())
    }

    /// The server with its headers from `secrets`, and which secret each
    /// came from. Fails naming a secret that is not given.
    pub fn resolve(&self, secrets: &[Secret]) -> Result<(RemoteMcpServer, Vec<McpSecret>), String> {
        let mut headers = Vec::new();
        let mut used = Vec::new();
        for (header, name) in &self.headers {
            let secret = secrets.iter().find(|s| &s.name == name).ok_or_else(|| {
                format!(
                    "MCP server {} needs the secret {name}, which the task does not give",
                    self.name
                )
            })?;
            branchyard_harness::check_header(header, &secret.value).map_err(|_| {
                format!("the secret {name} spans lines and cannot be an HTTP header")
            })?;
            headers.push((header.clone(), secret.value.clone()));
            used.push(McpSecret {
                secret: name.clone(),
                via: Via::McpHeader {
                    server: self.name.clone(),
                    header: header.clone(),
                },
            });
        }
        let transport = match self.transport {
            RemoteMcpTransport::Http => RemoteTransport::Http,
            RemoteMcpTransport::Sse => RemoteTransport::Sse,
        };
        Ok((
            RemoteMcpServer {
                name: self.name.clone(),
                transport,
                url: self.url.clone(),
                headers,
            },
            used,
        ))
    }
}

/// How hard the model reasons, where the harness can be told.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
}

impl Effort {
    pub fn parse(text: &str) -> Result<Effort, String> {
        match text {
            "low" => Ok(Effort::Low),
            "medium" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            "xhigh" => Ok(Effort::Xhigh),
            other => match other.parse::<i64>() {
                Ok(level) => Ok(Effort::from_level(level)),
                Err(_) => Err(format!(
                    "effort is low, medium, high, xhigh or a level from 0 to 100, not {other:?}"
                )),
            },
        }
    }

    /// Scion's thinking level (0 to 100, clamped) as a Codex reasoning
    /// effort; from `_resolve_reasoning_effort` in its Codex provisioner.
    pub fn from_level(level: i64) -> Effort {
        match level.clamp(0, 100) {
            76.. => Effort::Xhigh,
            51.. => Effort::High,
            26.. => Effort::Medium,
            _ => Effort::Low,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
        }
    }
}

/// The harness's own OpenTelemetry export.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telemetry {
    /// `false` turns the harness's export off where it can be told to.
    pub enabled: bool,
    /// OTLP/gRPC collector, as the harness reaches it. Unset:
    /// [`Telemetry::DEFAULT_ENDPOINT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl Telemetry {
    /// Scion's local receiver address.
    pub const DEFAULT_ENDPOINT: &'static str = "http://127.0.0.1:4317";

    /// `off`, or an `http://` or `https://` collector endpoint.
    pub fn parse(text: &str) -> Result<Telemetry, String> {
        if text == "off" {
            return Ok(Telemetry {
                enabled: false,
                endpoint: None,
            });
        }
        let telemetry = Telemetry {
            enabled: true,
            endpoint: Some(text.to_owned()),
        };
        telemetry.check()?;
        Ok(telemetry)
    }

    pub fn check(&self) -> Result<(), String> {
        if let Some(endpoint) = &self.endpoint {
            let rest = endpoint
                .strip_prefix("http://")
                .or_else(|| endpoint.strip_prefix("https://"));
            if rest.is_none_or(|r| r.is_empty() || r.contains(char::is_whitespace)) {
                return Err(format!(
                    "a telemetry endpoint is http:// or https://HOST:PORT, not {endpoint:?}"
                ));
            }
        }
        Ok(())
    }

    pub fn endpoint(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(Self::DEFAULT_ENDPOINT)
    }
}

/// An MCP server variable or header set from a secret, by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpSecret {
    pub secret: String,
    /// [`Via::McpServer`] or [`Via::McpHeader`].
    pub via: Via,
}

/// A secret with its value. Never serialized; `Debug` shows only the name.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret {
    pub name: String,
    pub value: String,
}

impl Secret {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Secret {
        Secret {
            name: name.into(),
            value: value.into(),
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({}: <redacted>)", self.name)
    }
}

/// Everything a provisioner plans from. Built by the engine for one turn.
#[derive(Clone, Debug)]
pub struct Context {
    /// Branchyard's harness ID, such as `claude-code`.
    pub harness: String,
    /// The profile's protocol, which decides the session channels.
    pub protocol: Protocol,
    /// `HOME` as the harness sees it.
    pub home: String,
    /// The working directory as the harness sees it.
    pub workspace: String,
    /// Whether the home belongs to this branch alone (an isolated branch
    /// or a sandbox), so files may be written there.
    pub private_home: bool,
    /// Whether the harness runs in a sandbox rather than on this host.
    pub sandbox: bool,
    pub secrets: Vec<Secret>,
    /// Explicit authentication method.
    pub auth: Option<String>,
    /// Every MCP server for the turn: the task's and Branchyard's own,
    /// with their variables, secrets included.
    pub mcp_servers: Vec<McpServer>,
    /// HTTP and SSE MCP servers, with their headers.
    pub remote_mcp_servers: Vec<RemoteMcpServer>,
    /// Which of the servers' variables and headers hold secrets.
    pub mcp_secrets: Vec<McpSecret>,
    /// The task's instructions and Branchyard's delegation skill, joined.
    pub instructions: Option<Instructions>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub telemetry: Option<Telemetry>,
    /// What an earlier provisioning of this home installed in native
    /// configuration, from [`apply::installed`].
    pub installed: Installed,
}

impl Context {
    /// A context with nothing asked for.
    pub fn new(
        harness: impl Into<String>,
        protocol: Protocol,
        home: impl Into<String>,
        workspace: impl Into<String>,
    ) -> Context {
        Context {
            harness: harness.into(),
            protocol,
            home: home.into(),
            workspace: workspace.into(),
            private_home: false,
            sandbox: false,
            secrets: Vec::new(),
            auth: None,
            mcp_servers: Vec::new(),
            remote_mcp_servers: Vec::new(),
            mcp_secrets: Vec::new(),
            instructions: None,
            model: None,
            effort: None,
            telemetry: None,
            installed: Installed::default(),
        }
    }

    pub fn secret(&self, name: &str) -> Option<&str> {
        self.secrets
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.value.as_str())
    }

    /// Whether the driver passes MCP servers and instructions in the
    /// session itself.
    pub fn session_channel(&self) -> bool {
        matches!(
            self.protocol,
            Protocol::ClaudeStreamJson | Protocol::CodexAppServer | Protocol::Acp
        )
    }

    /// The home as the harness sees it, joined with a relative path.
    pub fn home_path(&self, relative: &str) -> String {
        format!("{}/{relative}", self.home.trim_end_matches('/'))
    }
}

/// What Branchyard installed in a home, recorded there at
/// [`INSTALLED_PATH`]: native MCP entries, so a later provisioning removes
/// the ones no longer asked for and nothing else, and the credentials it
/// wrote, so removing the branch removes them
/// ([`apply::remove_credentials`]).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<Credential>,
}

/// A credential Branchyard wrote in a home: a file holding a secret, by
/// path relative to the home, and which part of it is Branchyard's.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Credential {
    /// The whole file.
    File { path: String },
    /// These variables of a dotenv file.
    Dotenv { path: String, keys: Vec<String> },
    /// These key paths of a JSON file.
    Json {
        path: String,
        keys: Vec<Vec<String>>,
    },
}

impl Credential {
    pub fn path(&self) -> &str {
        match self {
            Credential::File { path }
            | Credential::Dotenv { path, .. }
            | Credential::Json { path, .. } => path,
        }
    }

    /// What a secret file's edits leave of Branchyard's: `None` when they
    /// remove it or leave nothing that could be undone.
    fn of(file: &FileEdit) -> Option<Credential> {
        let path = file.path.clone();
        let credential = match file.edits.first()? {
            Edit::Put(_) => Some(Credential::File { path }),
            Edit::Dotenv(_) => Some(Credential::Dotenv {
                path,
                keys: file
                    .edits
                    .iter()
                    .flat_map(|e| match e {
                        Edit::Dotenv(pairs) => pairs.iter().map(|(k, _)| k.clone()).collect(),
                        _ => Vec::new(),
                    })
                    .collect(),
            }),
            Edit::Json { .. } => Some(Credential::Json {
                path,
                keys: file
                    .edits
                    .iter()
                    .flat_map(|e| match e {
                        Edit::Json { edits, .. } => edits
                            .iter()
                            .filter_map(|j| match j {
                                JsonEdit::Set(key, _) | JsonEdit::Push(key, _) => Some(key.clone()),
                                _ => None,
                            })
                            .collect(),
                        _ => Vec::new(),
                    })
                    .collect(),
            }),
            _ => None,
        }?;
        match &credential {
            Credential::Dotenv { keys, .. } if keys.is_empty() => None,
            Credential::Json { keys, .. } if keys.is_empty() => None,
            _ => Some(credential),
        }
    }

    /// This credential and `other`, for the same path, as one.
    fn merge(&mut self, other: Credential) {
        match (self, other) {
            (Credential::Dotenv { keys, .. }, Credential::Dotenv { keys: more, .. }) => {
                for key in more {
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
            }
            (Credential::Json { keys, .. }, Credential::Json { keys: more, .. }) => {
                for key in more {
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
            }
            (this, other) => *this = other,
        }
    }
}

/// Where [`Installed`] is kept, relative to the home.
pub const INSTALLED_PATH: &str = ".branchyard/provisioned.json";

/// One file in the home and how to change it.
#[derive(Clone, PartialEq)]
pub struct FileEdit {
    /// Relative to the home; never absolute, never with `..`.
    pub path: String,
    /// Holds a secret: written 0600, and redacted in `Debug`.
    pub secret: bool,
    /// Applied in order to the current content.
    pub edits: Vec<Edit>,
}

impl fmt::Debug for FileEdit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("FileEdit");
        s.field("path", &self.path).field("secret", &self.secret);
        match self.secret {
            true => s.field("edits", &"<redacted>"),
            false => s.field("edits", &self.edits),
        };
        s.finish()
    }
}

/// A variable for the harness process.
#[derive(Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
    pub secret: bool,
}

impl EnvVar {
    pub fn plain(name: &str, value: impl Into<String>) -> EnvVar {
        EnvVar {
            name: name.into(),
            value: value.into(),
            secret: false,
        }
    }

    pub fn secret(name: &str, value: impl Into<String>) -> EnvVar {
        EnvVar {
            name: name.into(),
            value: value.into(),
            secret: true,
        }
    }
}

impl fmt::Debug for EnvVar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.secret {
            true => write!(f, "{}=<redacted>", self.name),
            false => write!(f, "{}={:?}", self.name, self.value),
        }
    }
}

/// What the driver passes in the session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Session {
    pub mcp_servers: Vec<McpServer>,
    pub remote_mcp_servers: Vec<RemoteMcpServer>,
    pub instructions: Option<Instructions>,
    pub model: Option<String>,
    /// For a driver that would otherwise put the MCP servers on its command
    /// line (Claude Code's stream-json), the file it is given instead.
    pub mcp_config: Option<McpConfigFile>,
}

/// The MCP configuration as a file the harness reads, mode 0600: in the
/// private home, where the plan also writes it, or, without one, a file
/// the caller writes for the turn outside the working tree and removes
/// after it.
#[derive(Clone, PartialEq, Eq)]
pub struct McpConfigFile {
    /// As the harness sees it, when the plan writes it in the home; `None`
    /// when the caller must place it.
    pub path: Option<String>,
    /// The file's content. Server variables may hold tokens, so `Debug`
    /// redacts it.
    pub content: String,
}

impl fmt::Debug for McpConfigFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpConfigFile")
            .field("path", &self.path)
            .field("content", &"<redacted>")
            .finish()
    }
}

/// How one secret reaches the harness. Names and paths only.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    /// The secret's name, as the task gave it.
    pub secret: String,
    #[serde(flatten)]
    pub via: Via,
    /// Whether the value is in the harness's own environment, which the
    /// commands its tools run (its shell, the MCP servers it starts)
    /// inherit, so any of them sees it without looking for it. A file in
    /// the home is not, though a tool running as the same user could still
    /// read it.
    pub tool_env: bool,
}

/// Where a secret's value is.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum Via {
    /// A variable of the harness process.
    Env { var: String },
    /// A file in the home, relative to it, that the harness reads.
    File { path: String },
    /// A file in the home that a command named in the harness's settings
    /// prints when the harness needs it, such as Claude Code's
    /// `apiKeyHelper`.
    Helper { path: String, setting: String },
    /// A variable of an MCP server the harness starts, given in the
    /// server's configuration: over stdin, or in a 0600 file. Only that
    /// server's process has it, so `tool_env` is false.
    McpServer { server: String, var: String },
    /// A header the harness sends to an HTTP or SSE MCP server, given in
    /// the same way.
    McpHeader { server: String, header: String },
}

/// A provisioning plan: data only, applied by [`apply::apply`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    /// In order; at most one per path.
    pub files: Vec<FileEdit>,
    pub env: Vec<EnvVar>,
    pub session: Session,
    /// The authentication method chosen, when secrets chose one.
    pub auth: Option<String>,
    /// How each secret the harness reads reaches it, one entry per secret.
    pub secrets: Vec<Delivery>,
    /// Secrets given that this harness does not read.
    pub unused_secrets: Vec<String>,
    /// The native MCP entries now installed, when the provisioner manages
    /// them; recorded in the home by [`plan`].
    pub installed_mcp_servers: Option<Vec<String>>,
}

impl Plan {
    /// Add an edit, combining it with an earlier one for the same file.
    pub fn edit(&mut self, path: &str, secret: bool, edit: Edit) {
        if let Some(existing) = self.files.iter_mut().find(|f| f.path == path) {
            existing.secret |= secret;
            let rest = match existing.edits.last_mut() {
                Some(last) => last.absorb(edit),
                None => Some(edit),
            };
            existing.edits.extend(rest);
            return;
        }
        self.files.push(FileEdit {
            path: path.to_owned(),
            secret,
            edits: vec![edit],
        });
    }

    pub fn set_env(&mut self, var: EnvVar) {
        self.env.retain(|e| e.name != var.name);
        self.env.push(var);
    }

    /// Record how `secret` reaches the harness.
    pub fn deliver(&mut self, secret: &str, via: Via, tool_env: bool) {
        self.secrets.retain(|d| d.secret != secret || d.via != via);
        self.secrets.push(Delivery {
            secret: secret.to_owned(),
            via,
            tool_env,
        });
    }

    /// Set `var` from `secret` in the harness's environment, and record it.
    /// `tool_env`: whether the harness passes it on to its tool commands.
    pub(crate) fn secret_env(&mut self, secret: &str, var: &str, value: &str, tool_env: bool) {
        self.set_env(EnvVar::secret(var, value));
        self.deliver(secret, Via::Env { var: var.into() }, tool_env);
    }

    /// The secrets in the environment of the harness's tool commands.
    pub fn tool_env_secrets(&self) -> Vec<String> {
        self.secrets
            .iter()
            .filter(|d| d.tool_env)
            .map(|d| d.secret.clone())
            .collect()
    }

    /// Whether the plan changes anything outside the session.
    pub fn touches_home_or_env(&self) -> bool {
        !self.files.is_empty() || !self.env.is_empty()
    }
}

/// Why a context cannot be provisioned. Never holds a secret value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// Plans provisioning for one harness.
pub trait Provisioner: Sync {
    /// Branchyard's harness ID.
    fn harness(&self) -> &'static str;
    /// The Scion harness directory it was translated from, if any.
    fn scion(&self) -> Option<&'static str>;
    /// Plan without I/O.
    fn plan(&self, context: &Context) -> Result<Plan, Refused>;
}

static PROVISIONERS: &[&dyn Provisioner] = &[
    &claude::Claude,
    &codex::Codex,
    &gemini::Gemini,
    &opencode::OpenCode,
    &copilot::Copilot,
    &hermes::Hermes,
    &antigravity::Antigravity,
];

/// Every translated provisioner.
pub fn provisioners() -> &'static [&'static dyn Provisioner] {
    PROVISIONERS
}

/// Scion harnesses that were not translated, and why.
pub const NOT_PORTED: &[(&str, &str)] = &[
    (
        "grok-build",
        "Branchyard has no Grok Build profile or driver; a provisioner with nothing to launch \
         could not be tested",
    ),
    (
        "muse-code",
        "Branchyard has no Muse Code profile or driver; a provisioner with nothing to launch \
         could not be tested",
    ),
];

/// The provisioner for a harness: a translated one, or one that only
/// passes session items through and refuses anything else.
pub fn for_harness(harness: &str) -> &'static dyn Provisioner {
    PROVISIONERS
        .iter()
        .copied()
        .find(|p| p.harness() == harness)
        .unwrap_or(&generic::Generic)
}

/// Plan `context` with its harness's provisioner.
///
/// Secrets are refused without a private home: they are written there or
/// set for a harness that would otherwise run with your own login.
pub fn plan(context: &Context) -> Result<Plan, Refused> {
    if !context.secrets.is_empty() && !context.private_home {
        return Err(Refused(format!(
            "secrets are provisioned only into a home private to the branch; run {} \
             --isolated or in a sandbox provider",
            context.harness
        )));
    }
    if let Some(telemetry) = &context.telemetry {
        telemetry.check().map_err(Refused)?;
    }
    if let Some(server) = context.remote_mcp_servers.first() {
        if !matches!(context.protocol, Protocol::ClaudeStreamJson | Protocol::Acp) {
            return Err(unsupported(
                context,
                &format!("the HTTP or SSE MCP server {}", server.name),
                "only Claude Code's stream-json driver and ACP agents that advertise them \
                 take one",
            ));
        }
    }
    let mut plan = for_harness(&context.harness).plan(context)?;
    // Secrets given to MCP servers are used, and delivered with them.
    for used in &context.mcp_secrets {
        plan.unused_secrets.retain(|name| name != &used.secret);
        plan.deliver(&used.secret, used.via.clone(), false);
    }
    if context.private_home {
        record_installed(context, &mut plan);
    }
    Ok(plan)
}

/// Record in the home what the plan installs: its native MCP entries and,
/// with what earlier turns recorded, every credential file it writes.
fn record_installed(context: &Context, plan: &mut Plan) {
    let mut credentials = context.installed.credentials.clone();
    for file in &plan.files {
        if file.edits.iter().any(|e| matches!(e, Edit::Remove)) {
            credentials.retain(|c| c.path() != file.path);
            continue;
        }
        // Keys this plan removes are no longer Branchyard's.
        for edit in &file.edits {
            for credential in credentials.iter_mut().filter(|c| c.path() == file.path) {
                match (credential, edit) {
                    (Credential::Json { keys, .. }, Edit::Json { edits, .. }) => {
                        keys.retain(|key| {
                            !edits.iter().any(|e| {
                                matches!(e, JsonEdit::Remove(k) | JsonEdit::RemoveIf(k, _) if k == key)
                            })
                        })
                    }
                    (Credential::Dotenv { keys, .. }, Edit::DotenvUnset(names)) => {
                        keys.retain(|key| !names.contains(key))
                    }
                    _ => {}
                }
            }
        }
        credentials.retain(|c| match c {
            Credential::File { .. } => true,
            Credential::Dotenv { keys, .. } => !keys.is_empty(),
            Credential::Json { keys, .. } => !keys.is_empty(),
        });
        let Some(credential) = file.secret.then(|| Credential::of(file)).flatten() else {
            continue;
        };
        match credentials.iter_mut().find(|c| c.path() == file.path) {
            Some(existing) => existing.merge(credential),
            None => credentials.push(credential),
        }
    }
    let installed = Installed {
        mcp_servers: plan
            .installed_mcp_servers
            .clone()
            .unwrap_or_else(|| context.installed.mcp_servers.clone()),
        credentials,
    };
    if installed != context.installed || plan.installed_mcp_servers.is_some() {
        let record = serde_json::to_value(&installed).expect("serializes");
        plan.edit(INSTALLED_PATH, false, Edit::Put(json_text(&record)));
    }
}

// Shared by the provisioners.

/// Refuse a plan that writes files without a private home.
pub(crate) fn needs_private_home(context: &Context, what: &str) -> Result<(), Refused> {
    match context.private_home {
        true => Ok(()),
        false => Err(Refused(format!(
            "{what} for {} is written into its home, which is yours in local mode; run it \
             --isolated or in a sandbox provider",
            context.harness
        ))),
    }
}

/// Session items for a driver with a session channel: every MCP server and
/// the instructions, as they are.
pub(crate) fn pass_session(context: &Context, plan: &mut Plan) {
    plan.session.mcp_servers = context.mcp_servers.clone();
    plan.session.remote_mcp_servers = context.remote_mcp_servers.clone();
    plan.session.instructions = context.instructions.clone();
}

/// Secrets in the context that `used` does not name.
pub(crate) fn unused(context: &Context, used: &[&str]) -> Vec<String> {
    context
        .secrets
        .iter()
        .filter(|s| !used.contains(&s.name.as_str()))
        .map(|s| s.name.clone())
        .collect()
}

/// Refuse a request this harness has no place for.
pub(crate) fn unsupported(context: &Context, what: &str, why: &str) -> Refused {
    Refused(format!(
        "{what} cannot be provisioned for {}: {why}",
        context.harness
    ))
}

/// A JSON document rendered with sorted keys, two-space indentation and a
/// trailing newline, as Scion's `atomic_write_json` writes it.
pub(crate) fn json_text(value: &serde_json::Value) -> String {
    let mut text =
        serde_json::to_string_pretty(&edit::sorted(value)).expect("JSON values serialize");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_sources_parse_names_variables_and_files() {
        assert_eq!(
            SecretSource::parse("ANTHROPIC_API_KEY").unwrap(),
            SecretSource {
                name: "ANTHROPIC_API_KEY".into(),
                from: None
            }
        );
        assert_eq!(
            SecretSource::parse("OPENAI_API_KEY=MY_KEY").unwrap().from,
            Some(SecretFrom::Env {
                var: "MY_KEY".into()
            })
        );
        assert_eq!(
            SecretSource::parse("CODEX_AUTH=@/run/auth.json")
                .unwrap()
                .from,
            Some(SecretFrom::File {
                path: "/run/auth.json".into()
            })
        );
        for bad in ["", "1X", "A B", "X=", "X=@", "X=a b"] {
            assert!(SecretSource::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn secrets_never_show_their_values() {
        let secret = Secret::new("ANTHROPIC_API_KEY", "sk-very-secret");
        assert!(!format!("{secret:?}").contains("very-secret"));
        let env = EnvVar::secret("ANTHROPIC_API_KEY", "sk-very-secret");
        assert!(!format!("{env:?}").contains("very-secret"));
        let file = FileEdit {
            path: ".codex/auth.json".into(),
            secret: true,
            edits: vec![Edit::Put("sk-very-secret".into())],
        };
        assert!(!format!("{file:?}").contains("very-secret"));
    }

    #[test]
    fn mcp_servers_parse_and_are_checked_like_the_drivers() {
        let spec = McpServerSpec::parse("docs=/usr/bin/docs-mcp --stdio").unwrap();
        assert_eq!(spec.command, "/usr/bin/docs-mcp");
        assert_eq!(spec.args, ["--stdio"]);
        assert_eq!(spec.server().name, "docs");
        for bad in ["docs", "docs=", "docs=relative", "bad name=/x"] {
            assert!(McpServerSpec::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn effort_accepts_names_and_scion_levels() {
        assert_eq!(Effort::parse("xhigh").unwrap(), Effort::Xhigh);
        assert_eq!(Effort::parse("30").unwrap(), Effort::Medium);
        assert!(Effort::parse("max").is_err());
    }

    #[test]
    fn telemetry_is_off_or_an_endpoint() {
        assert!(!Telemetry::parse("off").unwrap().enabled);
        let on = Telemetry::parse("http://127.0.0.1:14317").unwrap();
        assert_eq!(on.endpoint(), "http://127.0.0.1:14317");
        assert!(Telemetry::parse("collector:4317").is_err());
        assert!(Telemetry::parse("http://").is_err());
    }

    #[test]
    fn provisioning_round_trips_and_refuses_unknown_fields() {
        let spec = Provisioning {
            secrets: vec![SecretSource::parse("CODEX_AUTH=@/a").unwrap()],
            effort: Some(Effort::High),
            telemetry: Some(Telemetry {
                enabled: false,
                endpoint: None,
            }),
            ..Provisioning::default()
        };
        let text = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<Provisioning>(&text).unwrap(), spec);
        assert!(serde_json::from_str::<Provisioning>(r#"{"secret": []}"#).is_err());
        assert!(Provisioning::default().is_empty());
    }
}
