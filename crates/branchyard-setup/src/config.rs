//! The project and user configuration file, `branchyard.toml`.
//!
//! One format at two levels: `~/.config/branchyard/config.toml` for a
//! person's defaults and `branchyard.toml` at a repository's root for the
//! project's. The project file overrides the user file key by key, the
//! `BRANCHYARD_*` variables override both, and command-line flags override
//! everything. [`parse`] reads a file strictly (`deny_unknown_fields`, then
//! the same parsers the flags use for secrets, MCP servers and effort), so a
//! typo is an error naming its line, never a silently ignored key.
//!
//! A file never holds a secret's value: `[secrets]` maps a secret's name to
//! the variable (`"VAR"`) or file (`"@path"`) that holds it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The only file version this build reads.
pub const VERSION: u32 = 1;

/// The published JSON Schema, for editors (`#:schema` in generated files).
pub const SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/vamsiramakrishnan/branchyard/main/schema/branchyard.config.json";

/// The project file's name, at the repository root.
pub const PROJECT_FILE: &str = "branchyard.toml";

/// A `branchyard.toml` or user `config.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(
    title = "Branchyard configuration",
    description = "Defaults for `by`: branchyard.toml at a repository root, or \
                   ~/.config/branchyard/config.toml. The project file overrides the user file, \
                   BRANCHYARD_* variables override both, flags override everything. Never holds a \
                   secret's value. See docs/setup.md."
)]
pub struct ProjectConfig {
    /// The file format's version; `1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Defaults for new branches (`by run`, `by fan`); `permissions` also
    /// applies to `send`, `fork`, `reincarnate` and `spawn`.
    #[serde(default, skip_serializing_if = "Defaults::is_empty")]
    pub defaults: Defaults,
    /// Secrets a new branch's harness is given, by name: `"VAR"` reads the
    /// variable VAR, `"@path"` a file. Never the value itself. Applied only
    /// to isolated or sandboxed branches, which have a private home.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, String>,
    /// Stdio MCP servers for a new branch's harness: `NAME = "/absolute/command args"`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, String>,
    /// Where commands run: a Branchyard server instead of this machine.
    #[serde(default, skip_serializing_if = "Remote::is_empty")]
    pub remote: Remote,
    /// `by serve` defaults.
    #[serde(default, skip_serializing_if = "Serve::is_empty")]
    pub serve: Serve,
    /// Options for `provider = "microsandbox"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microsandbox: Option<Microsandbox>,
    /// Telling you when a branch needs you or ends, from `by watch` and a
    /// waiting `by run`, `by fan`, `by send` or `by fork`.
    #[serde(default, skip_serializing_if = "Notify::is_empty")]
    pub notify: Notify,
}

/// Task defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Harness or profile ID, such as `claude-code` or `codex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// A model name, or a size alias (`small`, `medium`, `large`, `extra-large`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort: `low`, `medium`, `high`, `xhigh`, or 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The authentication method when the secrets allow several:
    /// `api-key`, `oauth-token`, `auth-file`, `vertex-ai`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// Stop once the harness's own cost estimate exceeds this many dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub budget_usd: Option<f64>,
    /// Stop after this many turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_turns: Option<u32>,
    /// Interrupt a turn after this many minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub max_minutes: Option<f64>,
    /// How tool permission requests are answered: `ask` on the terminal,
    /// or `yes` to allow each one. Unset: decided by whether a terminal is
    /// attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionsMode>,
    /// A scrubbed environment and a private HOME for the harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolated: Option<bool>,
    /// The check a candidate must pass before merging, such as `cargo test`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// Where the harness runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderKind>,
    /// A file of standing instructions for the harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl Defaults {
    pub fn is_empty(&self) -> bool {
        self == &Defaults::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PermissionsMode {
    /// Ask on the terminal for each request (`--ask`).
    Ask,
    /// Allow every request (`--yes`).
    Yes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// A local process in its own worktree (the default).
    Local,
    /// A microVM per turn; needs `[microsandbox]` and a build with the
    /// `microsandbox` feature.
    Microsandbox,
}

/// A Branchyard server to run commands against.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// The server's URL, as `--remote`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// A file holding the bearer token, as `--token-file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<String>,
    /// Extra CA certificates for `https`, as `--ca-file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<String>,
    /// The repository's name on the server, as `--repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

impl Remote {
    pub fn is_empty(&self) -> bool {
        self == &Remote::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Serve {
    /// The server configuration `by serve` reads when given no `--config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
}

impl Serve {
    pub fn is_empty(&self) -> bool {
        self == &Serve::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Microsandbox {
    /// OCI image with the harness installed.
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub cpus: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub memory_mib: Option<u32>,
    /// Variables to copy into the sandbox, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pass_env: Vec<String>,
}

/// `[notify]`: when a branch asks for permission or asks a question,
/// stalls, fails, is interrupted or finishes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Notify {
    /// Notify at all. Unset: yes; `--no-notify` turns it off for one command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Also show a desktop notification through `notify-send` (Linux and
    /// other Unix) or `osascript` (macOS). Unset: no.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<bool>,
    /// What to write to the terminal. Unset: `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<NotifyTerminal>,
}

impl Notify {
    pub fn is_empty(&self) -> bool {
        self == &Notify::default()
    }
}

/// The terminal side of a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotifyTerminal {
    /// A bell, and the desktop-notification escape this terminal is known
    /// to take: OSC 777 on foot, urxvt and VTE terminals, OSC 9 elsewhere.
    Auto,
    /// A bell and OSC 9 (iTerm2, WezTerm, kitty, Ghostty, Windows Terminal).
    Osc9,
    /// A bell and OSC 777 (foot, urxvt, VTE terminals such as GNOME Terminal, WezTerm).
    Osc777,
    /// Only a bell.
    Bell,
    /// Nothing on the terminal (the desktop notification, if on, still runs).
    None,
}

/// A refused file: its message names the key, and the line when known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Read a file's text strictly: its shape, then every value the flags
/// would also check.
pub fn parse(text: &str) -> Result<ProjectConfig, ConfigError> {
    // The message and line only: toml's own rendering quotes the source
    // line, which could be a pasted secret.
    let config: ProjectConfig = toml::from_str(text).map_err(|e| {
        let line = e
            .span()
            .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
        let message = e.message().trim_end().to_owned();
        ConfigError(match line {
            Some(line) => format!("line {line}: {message}"),
            None => message,
        })
    })?;
    config.check()?;
    Ok(config)
}

impl ProjectConfig {
    /// The checks beyond the file's shape, with the flags' own parsers.
    pub fn check(&self) -> Result<(), ConfigError> {
        let fail = |key: &str, why: String| Err(ConfigError(format!("{key}: {why}")));
        if let Some(version) = self.version {
            if version != VERSION {
                return fail(
                    "version",
                    format!("this build reads version {VERSION}, not {version}"),
                );
            }
        }
        let d = &self.defaults;
        if let Some(harness) = &d.harness {
            if branchyard_harness::profiles::default_for(harness).is_none()
                && branchyard_harness::profiles::by_id(harness).is_none()
            {
                return fail(
                    "defaults.harness",
                    format!("{harness:?} is not a harness or profile; see `by harnesses`"),
                );
            }
        }
        if let Some(model) = &d.model {
            if model.trim().is_empty() {
                return fail("defaults.model", "must not be empty".into());
            }
        }
        if let Some(effort) = &d.effort {
            branchyard_provision::Effort::parse(effort)
                .map_err(|e| ConfigError(format!("defaults.effort: {e}")))?;
        }
        if let Some(auth) = &d.auth {
            if !AUTH_METHODS.contains(&auth.as_str()) {
                return fail(
                    "defaults.auth",
                    format!("use one of {}, not {auth:?}", AUTH_METHODS.join(", ")),
                );
            }
        }
        if let Some(usd) = d.budget_usd {
            if !(usd.is_finite() && usd > 0.0) {
                return fail(
                    "defaults.budget_usd",
                    format!("must be a positive number, not {usd}"),
                );
            }
        }
        if d.max_turns == Some(0) {
            return fail("defaults.max_turns", "must be at least 1".into());
        }
        if let Some(minutes) = d.max_minutes {
            if !(minutes.is_finite() && minutes > 0.0) {
                return fail(
                    "defaults.max_minutes",
                    format!("must be a positive number, not {minutes}"),
                );
            }
        }
        if let Some(check) = &d.check {
            if split_words(check)
                .map_err(|e| ConfigError(format!("defaults.check: {e}")))?
                .is_empty()
            {
                return fail("defaults.check", "needs a command".into());
            }
        }
        if d.provider == Some(ProviderKind::Microsandbox) && self.microsandbox.is_none() {
            return fail(
                "defaults.provider",
                "microsandbox needs a [microsandbox] table with an image".into(),
            );
        }
        for (name, source) in &self.secrets {
            secret_source(name, source).map_err(|e| ConfigError(format!("secrets.{name}: {e}")))?;
        }
        for (name, command) in &self.mcp {
            branchyard_provision::McpServerSpec::parse(&format!("{name}={command}"))
                .map_err(|e| ConfigError(format!("mcp.{name}: {e}")))?;
        }
        if let Some(url) = &self.remote.url {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return fail(
                    "remote.url",
                    format!("must be an http:// or https:// URL, not {url:?}"),
                );
            }
        }
        for (key, value) in [
            ("remote.token_file", &self.remote.token_file),
            ("remote.ca_file", &self.remote.ca_file),
            ("remote.repo", &self.remote.repo),
            ("serve.config", &self.serve.config),
            ("defaults.instructions", &d.instructions),
        ] {
            if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
                return fail(key, "must not be empty".into());
            }
        }
        if self.notify.enabled == Some(false) && self.notify.desktop == Some(true) {
            return fail(
                "notify.desktop",
                "has no effect while notify.enabled = false; remove one of them".into(),
            );
        }
        if let Some(sandbox) = &self.microsandbox {
            if sandbox.image.trim().is_empty() {
                return fail("microsandbox.image", "must not be empty".into());
            }
            for name in &sandbox.pass_env {
                branchyard_provision::check_variable_name(name)
                    .map_err(|why| ConfigError(format!("microsandbox.pass_env: {name:?} {why}")))?;
            }
        }
        Ok(())
    }

    /// Paths made absolute against `dir`, the directory of the file they
    /// came from, and `~/` against `home`.
    pub fn resolve_paths(&mut self, dir: &Path, home: Option<&Path>) {
        let resolve = |value: &mut Option<String>| {
            if let Some(path) = value {
                *path = resolve_path(path, dir, home).display().to_string();
            }
        };
        resolve(&mut self.remote.token_file);
        resolve(&mut self.remote.ca_file);
        resolve(&mut self.serve.config);
        resolve(&mut self.defaults.instructions);
        for source in self.secrets.values_mut() {
            if let Some(path) = source.strip_prefix('@') {
                *source = format!("@{}", resolve_path(path, dir, home).display());
            }
        }
    }

    /// Every set value by its dotted key, such as `defaults.harness` or
    /// `secrets.ANTHROPIC_API_KEY`.
    pub fn flatten(&self) -> BTreeMap<String, Value> {
        let mut flat = BTreeMap::new();
        if let Ok(Value::Object(map)) = serde_json::to_value(self) {
            flatten_into("", &map, &mut flat);
        }
        flat
    }

    /// The inverse of [`ProjectConfig::flatten`].
    pub fn unflatten(flat: &BTreeMap<String, Value>) -> Result<ProjectConfig, ConfigError> {
        let mut root = Map::new();
        for (key, value) in flat {
            let (table, leaf) = match key.split_once('.') {
                Some((table, leaf)) => (table, Some(leaf)),
                None => (key.as_str(), None),
            };
            match leaf {
                None => {
                    root.insert(table.to_owned(), value.clone());
                }
                Some(leaf) => {
                    let entry = root
                        .entry(table.to_owned())
                        .or_insert_with(|| Value::Object(Map::new()));
                    if let Value::Object(map) = entry {
                        map.insert(leaf.to_owned(), value.clone());
                    }
                }
            }
        }
        serde_json::from_value(Value::Object(root)).map_err(|e| ConfigError(e.to_string()))
    }

    /// The secrets as the flags' `NAME=VAR` or `NAME=@FILE` sources.
    pub fn secret_sources(&self) -> Result<Vec<branchyard_provision::SecretSource>, ConfigError> {
        self.secrets
            .iter()
            .map(|(name, source)| {
                secret_source(name, source).map_err(|e| ConfigError(format!("secrets.{name}: {e}")))
            })
            .collect()
    }
}

/// Where a layer's values came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// The user file.
    User { path: String },
    /// The project file.
    Project { path: String },
    /// A `BRANCHYARD_*` variable.
    Env { var: String },
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::User { path } => write!(f, "user {path}"),
            Source::Project { path } => write!(f, "project {path}"),
            Source::Env { var } => write!(f, "env {var}"),
        }
    }
}

/// The merged configuration, and where each value came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effective {
    pub config: ProjectConfig,
    pub sources: BTreeMap<String, Source>,
}

/// The variables that override `[remote]`, by key.
pub const REMOTE_VARIABLES: &[(&str, &str)] = &[
    ("remote.url", "BRANCHYARD_REMOTE"),
    ("remote.token_file", "BRANCHYARD_TOKEN_FILE"),
    ("remote.repo", "BRANCHYARD_REPO"),
    ("remote.ca_file", "BRANCHYARD_CA_FILE"),
];

/// Merge layers, later ones winning key by key, then the variables `env`
/// returns (only [`REMOTE_VARIABLES`] are asked for).
pub fn merge(
    layers: &[(Source, ProjectConfig)],
    env: impl Fn(&str) -> Option<String>,
) -> Result<Effective, ConfigError> {
    let mut flat = BTreeMap::new();
    let mut sources = BTreeMap::new();
    for (source, layer) in layers {
        for (key, value) in layer.flatten() {
            flat.insert(key.clone(), value);
            sources.insert(key, source.clone());
        }
    }
    for (key, var) in REMOTE_VARIABLES {
        if let Some(value) = env(var).filter(|v| !v.trim().is_empty()) {
            flat.insert((*key).to_owned(), Value::String(value));
            sources.insert(
                (*key).to_owned(),
                Source::Env {
                    var: (*var).to_owned(),
                },
            );
        }
    }
    Ok(Effective {
        config: ProjectConfig::unflatten(&flat)?,
        sources,
    })
}

/// The methods `--auth` takes.
pub const AUTH_METHODS: &[&str] = &["api-key", "oauth-token", "auth-file", "vertex-ai"];

/// `NAME` with its `VAR` or `@FILE` source, parsed as `--secret` does,
/// after [`check_secret_reference`]. No error quotes the source.
pub fn secret_source(
    name: &str,
    source: &str,
) -> Result<branchyard_provision::SecretSource, String> {
    branchyard_provision::check_variable_name(name).map_err(|why| format!("secret name {why}"))?;
    check_secret_reference(source)?;
    branchyard_provision::SecretSource::parse(&format!("{name}={source}"))
        .map_err(|_| "is not a variable name or @file".to_owned())
}

/// Whether `source` names where a secret is (a variable, `VAR`, or a file,
/// `@path`) rather than being one. Refuses what looks like a pasted
/// credential; the message never quotes it.
pub fn check_secret_reference(source: &str) -> Result<(), String> {
    const REFUSED: &str = "must name a variable (such as ANTHROPIC_API_KEY) or a file (@path) \
                           that holds the secret, never the secret itself; the value is not shown";
    if let Some(path) = source.strip_prefix('@') {
        return match path.trim().is_empty() {
            true => Err("names an empty file after '@'".into()),
            false => Ok(()),
        };
    }
    let name_like = !source.is_empty()
        && source.len() <= 64
        && source.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && source
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    let lower = source.to_ascii_lowercase();
    let credential_like = [
        "sk-",
        "sk_",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "xox",
        "akia",
        "aiza",
        "glpat-",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
        || (source.len() >= 24
            && source.chars().any(|c| c.is_ascii_lowercase())
            && source.chars().any(|c| c.is_ascii_uppercase())
            && source.chars().any(|c| c.is_ascii_digit()));
    match name_like && !credential_like {
        true => Ok(()),
        false => Err(REFUSED.into()),
    }
}

fn resolve_path(path: &str, dir: &Path, home: Option<&Path>) -> PathBuf {
    if let (Some(rest), Some(home)) = (path.strip_prefix("~/"), home) {
        return home.join(rest);
    }
    let path = PathBuf::from(path);
    match path.is_absolute() {
        true => path,
        false => dir.join(path),
    }
}

fn flatten_into(prefix: &str, map: &Map<String, Value>, flat: &mut BTreeMap<String, Value>) {
    for (key, value) in map {
        let key = match prefix.is_empty() {
            true => key.clone(),
            false => format!("{prefix}.{key}"),
        };
        match value {
            // One level of tables: `[defaults]`, `[secrets]`, ...; a
            // table's own values (`microsandbox.pass_env`) stay whole.
            Value::Object(inner) if prefix.is_empty() => flatten_into(&key, inner, flat),
            Value::Null => {}
            other => {
                flat.insert(key, other.clone());
            }
        }
    }
}

/// Split a command line like a shell does for `--check`: whitespace,
/// single and double quotes, backslash escapes.
pub fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c) => word.push(c),
                            None => return Err("unterminated double quote".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(c) => word.push(c),
                    None => return Err("trailing backslash".into()),
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// A TOML string literal.
pub fn toml_string(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

/// A number as TOML writes it: integers without a fraction.
pub fn toml_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Render a configuration as a commented file, with the `#:schema` hint
/// editors (taplo, Even Better TOML) use for completion. Parsing the result
/// gives `config` back.
pub fn render(config: &ProjectConfig, heading: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("#:schema {SCHEMA_URL}\n"));
    for line in heading.lines() {
        out.push_str(format!("# {line}").trim_end());
        out.push('\n');
    }
    out.push_str("# Every key: docs/setup.md#configuration. Check with `by config validate`.\n");
    out.push_str(&format!(
        "version = {}\n",
        config.version.unwrap_or(VERSION)
    ));
    let d = &config.defaults;
    if !d.is_empty() {
        out.push_str(
            "\n# Defaults for new branches (by run, by fan); flags override them.\n[defaults]\n",
        );
        let mut line = |key: &str, value: Option<String>| {
            if let Some(value) = value {
                out.push_str(&format!("{key} = {value}\n"));
            }
        };
        line("harness", d.harness.as_deref().map(toml_string));
        line("model", d.model.as_deref().map(toml_string));
        line("effort", d.effort.as_deref().map(toml_string));
        line("auth", d.auth.as_deref().map(toml_string));
        line("budget_usd", d.budget_usd.map(toml_number));
        line("max_turns", d.max_turns.map(|n| n.to_string()));
        line("max_minutes", d.max_minutes.map(toml_number));
        line(
            "permissions",
            d.permissions.map(|p| {
                toml_string(match p {
                    PermissionsMode::Ask => "ask",
                    PermissionsMode::Yes => "yes",
                })
            }),
        );
        line("isolated", d.isolated.map(|b| b.to_string()));
        line("check", d.check.as_deref().map(toml_string));
        line(
            "provider",
            d.provider.map(|p| {
                toml_string(match p {
                    ProviderKind::Local => "local",
                    ProviderKind::Microsandbox => "microsandbox",
                })
            }),
        );
        line("instructions", d.instructions.as_deref().map(toml_string));
    }
    if !config.secrets.is_empty() {
        out.push_str(
            "\n# Secrets by name: \"VAR\" reads that variable, \"@path\" a file. Never a value.\n[secrets]\n",
        );
        for (name, source) in &config.secrets {
            out.push_str(&format!("{name} = {}\n", toml_string(source)));
        }
    }
    if !config.mcp.is_empty() {
        out.push_str("\n# Stdio MCP servers: NAME = \"/absolute/command args\".\n[mcp]\n");
        for (name, command) in &config.mcp {
            out.push_str(&format!("{name} = {}\n", toml_string(command)));
        }
    }
    if let Some(sandbox) = &config.microsandbox {
        out.push_str("\n[microsandbox]\n");
        out.push_str(&format!("image = {}\n", toml_string(&sandbox.image)));
        if let Some(cpus) = sandbox.cpus {
            out.push_str(&format!("cpus = {cpus}\n"));
        }
        if let Some(mib) = sandbox.memory_mib {
            out.push_str(&format!("memory_mib = {mib}\n"));
        }
        if !sandbox.pass_env.is_empty() {
            let names: Vec<String> = sandbox.pass_env.iter().map(|n| toml_string(n)).collect();
            out.push_str(&format!("pass_env = [{}]\n", names.join(", ")));
        }
    }
    if !config.remote.is_empty() {
        out.push_str("\n# A Branchyard server to run commands against; BRANCHYARD_REMOTE and friends override it.\n[remote]\n");
        let r = &config.remote;
        for (key, value) in [
            ("url", &r.url),
            ("token_file", &r.token_file),
            ("ca_file", &r.ca_file),
            ("repo", &r.repo),
        ] {
            if let Some(value) = value {
                out.push_str(&format!("{key} = {}\n", toml_string(value)));
            }
        }
    }
    if let Some(path) = &config.serve.config {
        out.push_str(&format!("\n[serve]\nconfig = {}\n", toml_string(path)));
    }
    let n = &config.notify;
    if !n.is_empty() {
        out.push_str(
            "\n# When a branch needs you or ends: by watch and a waiting by run.\n[notify]\n",
        );
        if let Some(enabled) = n.enabled {
            out.push_str(&format!("enabled = {enabled}\n"));
        }
        if let Some(desktop) = n.desktop {
            out.push_str(&format!("desktop = {desktop}\n"));
        }
        if let Some(terminal) = n.terminal {
            let name = match terminal {
                NotifyTerminal::Auto => "auto",
                NotifyTerminal::Osc9 => "osc9",
                NotifyTerminal::Osc777 => "osc777",
                NotifyTerminal::Bell => "bell",
                NotifyTerminal::None => "none",
            };
            out.push_str(&format!("terminal = {}\n", toml_string(name)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
version = 1
[defaults]
harness = "codex"
model = "large"
effort = "high"
budget_usd = 5
max_turns = 20
max_minutes = 30.5
permissions = "ask"
isolated = true
check = "cargo test --workspace"
provider = "microsandbox"
[secrets]
OPENAI_API_KEY = "OPENAI_API_KEY"
CODEX_AUTH = "@~/.codex/auth.json"
[mcp]
docs = "/usr/local/bin/docs-mcp --stdio"
[remote]
url = "https://by.example:8421"
token_file = "tokens/me.token"
[serve]
config = ".branchyard/server.json"
[microsandbox]
image = "ghcr.io/you/codex:1"
pass_env = ["OPENAI_API_KEY"]
[notify]
desktop = true
terminal = "osc777"
"#;

    #[test]
    fn a_full_file_parses_and_renders_back_to_itself() {
        let config = parse(FULL).unwrap();
        assert_eq!(config.defaults.harness.as_deref(), Some("codex"));
        assert_eq!(config.notify.desktop, Some(true));
        assert_eq!(config.notify.terminal, Some(NotifyTerminal::Osc777));
        assert_eq!(config.flatten()["notify.terminal"], "osc777");
        assert_eq!(config.defaults.budget_usd, Some(5.0));
        let rendered = render(&config, "test");
        assert!(rendered.starts_with("#:schema https://"), "{rendered}");
        assert_eq!(parse(&rendered).unwrap(), config);
    }

    #[test]
    fn unknown_keys_and_bad_values_are_refused_by_name() {
        for (text, needle) in [
            ("[defaults]\nharnes = \"codex\"", "harnes"),
            ("[defaults]\nharness = \"nope\"", "defaults.harness"),
            ("[defaults]\neffort = \"extreme\"", "defaults.effort"),
            ("[defaults]\nbudget_usd = -1", "defaults.budget_usd"),
            (
                "[defaults]\npermissions = \"always\"",
                "line 2: unknown variant `always`",
            ),
            ("[secrets]\nKEY = \"sk-ant-api03 value\"", "secrets.KEY"),
            ("[mcp]\ndocs = \"\"", "mcp.docs"),
            ("[remote]\nurl = \"ftp://x\"", "remote.url"),
            ("version = 2", "version"),
            ("[defaults]\nprovider = \"microsandbox\"", "[microsandbox]"),
            ("[notify]\nsound = true", "unknown field `sound`"),
            ("[notify]\nterminal = \"osc99\"", "unknown variant `osc99`"),
            ("[notify]\nenabled = \"no\"", "line 2"),
            (
                "[notify]\nenabled = false\ndesktop = true",
                "notify.desktop: has no effect",
            ),
        ] {
            let error = parse(text).unwrap_err().to_string();
            assert!(error.contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn a_secret_error_never_echoes_the_value() {
        for value in [
            "sk-ant-api03-SECRETVALUE",
            "ghp_SECRETVALUE0123",
            "Ab3dEf6hIj9kLm2nOp5qRs8t",
            "has space SECRETVALUE",
        ] {
            let error = parse(&format!("[secrets]\nKEY = \"{value}\""))
                .unwrap_err()
                .to_string();
            assert!(error.contains("secrets.KEY"), "{error}");
            assert!(
                !error.contains(value) && !error.contains("SECRETVALUE"),
                "{error}"
            );
        }
        assert!(check_secret_reference("ANTHROPIC_API_KEY").is_ok());
        assert!(check_secret_reference("@~/.codex/auth.json").is_ok());
    }

    #[test]
    fn layers_merge_key_by_key_with_sources() {
        let user =
            parse("[defaults]\nharness = \"codex\"\nmax_turns = 5\n[secrets]\nA = \"A\"").unwrap();
        let project = parse("[defaults]\nharness = \"claude-code\"\n[secrets]\nB = \"B\"").unwrap();
        let merged = merge(
            &[
                (Source::User { path: "u".into() }, user),
                (Source::Project { path: "p".into() }, project),
            ],
            |name| (name == "BRANCHYARD_REMOTE").then(|| "http://127.0.0.1:1".to_owned()),
        )
        .unwrap();
        assert_eq!(
            merged.config.defaults.harness.as_deref(),
            Some("claude-code")
        );
        assert_eq!(merged.config.defaults.max_turns, Some(5));
        assert_eq!(merged.config.secrets.len(), 2);
        assert_eq!(
            merged.config.remote.url.as_deref(),
            Some("http://127.0.0.1:1")
        );
        assert_eq!(
            merged.sources["defaults.harness"],
            Source::Project { path: "p".into() }
        );
        assert_eq!(
            merged.sources["defaults.max_turns"],
            Source::User { path: "u".into() }
        );
        assert_eq!(
            merged.sources["remote.url"],
            Source::Env {
                var: "BRANCHYARD_REMOTE".into()
            }
        );
    }

    #[test]
    fn paths_resolve_against_the_file_and_home() {
        let mut config = parse(FULL).unwrap();
        config.resolve_paths(Path::new("/repo"), Some(Path::new("/home/me")));
        assert_eq!(
            config.remote.token_file.as_deref(),
            Some("/repo/tokens/me.token")
        );
        assert_eq!(config.secrets["CODEX_AUTH"], "@/home/me/.codex/auth.json");
        assert_eq!(config.secrets["OPENAI_API_KEY"], "OPENAI_API_KEY");
    }

    #[test]
    fn words_split_like_the_check_flag() {
        assert_eq!(
            split_words("cargo test -- 'a b'").unwrap(),
            ["cargo", "test", "--", "a b"]
        );
        assert!(split_words("'open").is_err());
    }
}
