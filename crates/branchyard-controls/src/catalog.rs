//! What Branchyard knows about harness CLIs and connectors beyond the ones
//! it drives: `catalog/harnesses.toml` and `catalog/connectors.toml` at the
//! repository root, embedded here.
//!
//! Both files are generated from pinned upstream sources and checked in;
//! this module's tests regenerate them from the vendored files and fail
//! when a checked-in file differs (run them with `BRANCHYARD_BLESS=1` to
//! rewrite it, then review the diff):
//!
//! - harnesses: emdash's agent plugins (`vendor/emdash/packages/plugins/
//!   src/agents/impl/*/index.ts`: name, homepage, executables, Linux install
//!   commands, CLI login, API-key variables, model options, model and resume
//!   flags) and Orca's agent table (`vendor/orca/src/shared/
//!   tui-agent-config.ts` and `tui-agent.ts`: executables and display
//!   names), joined through [`crate::harness::HARNESSES`];
//! - connectors: emdash's MCP catalog (`vendor/emdash/apps/emdash-desktop/
//!   src/core/primitives/mcp/api/catalog.ts`);
//! - pricing (`catalog/pricing.toml`, read by `by usage`): Orca's Claude
//!   and Codex price tables (`vendor/orca/src/main/claude-usage/
//!   claude-model-pricing.ts`, `vendor/orca/src/main/codex-usage/
//!   codex-model-pricing.ts`).
//!
//! A few harness entries are corrected after generation (`curated`, in the
//! tests that generate the file) where the CLIs themselves were checked and
//! the upstream sources describe them differently or not at all.
//!
//! An entry is knowledge, not support: whether Branchyard can drive a
//! harness is its profile's business (`branchyard_harness::profiles`), and a
//! connector is used only once Anvil compiles it (`docs/connectors.md`).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// One harness CLI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessEntry {
    /// The ID in [`crate::harness::HARNESSES`].
    pub id: String,
    /// The harness's display name.
    pub name: String,
    /// Executable names, the usual one first.
    #[serde(default)]
    pub binaries: Vec<String>,
    /// The homepage URL, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Commands that install it on Linux, the recommended one first.
    #[serde(default)]
    pub install: Vec<String>,
    /// The command that signs in through the CLI itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// Variables that carry an API key instead of a login, by name.
    #[serde(default)]
    pub auth_env: Vec<String>,
    /// Model names its model flag takes.
    #[serde(default)]
    pub models: Vec<String>,
    /// The flag that selects a model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_flag: Option<String>,
    /// The flag that resumes a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_flag: Option<String>,
    /// Where the entry came from: `emdash:<plugin id>`, `orca:<agent key>`.
    #[serde(default)]
    pub sources: Vec<String>,
}

/// How `by harnesses` reads one harness's login, by name only: a table of
/// `catalog/logins.toml`, which is written by hand. The file's header
/// explains each field; `branchyard::inventory` makes the detection
/// script's lines from it and turns what they print into a login.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginEntry {
    /// Its own status command, when it has one Branchyard reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusCheck>,
    /// Key variables besides the catalog's `auth_env`, offered like it as
    /// the key to set.
    #[serde(default)]
    pub env: Vec<String>,
    /// Variables that count as a key when set but are not offered as the
    /// one to set (Qwen Code's `OPENAI_API_KEY`).
    #[serde(default)]
    pub quiet_env: Vec<String>,
    /// Credential files whose existence is a login.
    #[serde(default)]
    pub files: Vec<CredentialFile>,
    /// On macOS the login is in the keychain, so no credential file there
    /// proves nothing.
    #[serde(default)]
    pub keychain_on_macos: bool,
    /// Looks inside its configuration, in the order they are consulted.
    #[serde(default, rename = "probe")]
    pub probes: Vec<Probe>,
    /// Key variables that count only beside a fact a probe printed.
    #[serde(default, rename = "gate")]
    pub gates: Vec<Gate>,
    /// The interpreter its executable is a script for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<Runtime>,
    /// The detail when nothing is found, a likely logout; `{vars}` and
    /// `{vars_or}` are its key variables joined with commas and with "or".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logged_out: Option<String>,
    /// What else would tell, added to the detail of a login that cannot be
    /// read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown_also: Option<String>,
}

/// A harness's own login status command, and how its output is read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusCheck {
    /// The arguments after the executable: `["login", "status"]`.
    pub args: Vec<String>,
    /// Phrases that mean logged out, checked first, ignoring case.
    #[serde(default)]
    pub logged_out: Vec<String>,
    /// Phrases that mean logged in, ignoring case.
    #[serde(default)]
    pub logged_in: Vec<String>,
    /// A JSON boolean field that says whether it is logged in, read line by
    /// line, in place of the phrases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<String>,
    /// A JSON string field shown in brackets after the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Variables the harness sends instead of the login when they are set.
    #[serde(default)]
    pub overridden_by: Vec<String>,
}

/// A credential file: its path as an `sh` word expanded on the machine,
/// and how it is shown to people.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialFile {
    /// `${CODEX_HOME:-$HOME/.codex}/auth.json`.
    pub path: String,
    /// `~/.codex/auth.json`.
    pub shown: String,
}

/// A look inside a harness's configuration, by name only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Probe {
    /// An extended regular expression found in a file is a fact.
    Grep {
        /// The file, as an `sh` word.
        file: String,
        /// The pattern, for `grep -E`.
        pattern: String,
        /// The fact's name.
        fact: String,
        /// The detail when the fact alone is a likely login.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        login: Option<String>,
        /// Added to a logged-in detail when the fact was found.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note_found: Option<String>,
        /// Added to a logged-in detail when it was not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note_missing: Option<String>,
    },
    /// A variable equal to a value is a fact.
    Flag {
        /// The variable.
        var: String,
        /// The value it must have.
        value: String,
        /// The fact's name.
        fact: String,
        /// The detail when the fact alone is a likely login.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        login: Option<String>,
        /// Added to a logged-in detail when the fact was found.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note_found: Option<String>,
        /// Added to a logged-in detail when it was not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note_missing: Option<String>,
    },
    /// The model provider a TOML file selects at its top level, and the
    /// variable its table's key names (Codex's `config.toml`).
    Provider {
        /// The file, as an `sh` word.
        file: String,
        /// The file, for people.
        shown: String,
        /// The top-level key that selects the provider: `model_provider`.
        key: String,
        /// The provider the status command already speaks of: `openai`.
        default: String,
        /// The table providers are defined under: `model_providers`.
        table: String,
        /// The key in a provider's table naming its variable: `env_key`.
        env_key: String,
        /// What the status command speaks of: `the OpenAI login`.
        status_covers: String,
    },
    /// YAML files whose `section` key configures providers, a provider's
    /// key written inline (`inline_key`), and the variables `key_env`
    /// lines name.
    Providers {
        /// The directory, as an `sh` word.
        dir: String,
        /// The directory, for people.
        shown: String,
        /// The files under it.
        files: Vec<String>,
        /// The key that configures providers: `providers`.
        section: String,
        /// The key that holds a key inline: `apiKey`.
        inline_key: String,
        /// The key that names a key variable: `apiKeyEnv`.
        key_env: String,
        /// The detail of a configured provider; `{file}` is the file.
        configured: String,
        /// The detail of a provider whose variable is unset; `{vars}` are
        /// the unset variables.
        unset: String,
    },
    /// The `NAME_API_KEY=value` lines of `.env` files the harness loads, by
    /// name.
    EnvFile {
        /// The files, as `sh` words.
        files: Vec<String>,
        /// The detail; `{var}` and `{file}` are the first line's.
        login: String,
    },
    /// Provider names from a login database, read with `sqlite3` or
    /// `python3`.
    Sqlite {
        /// The database, as an `sh` word.
        file: String,
        /// The query that lists the providers with a login.
        query: String,
        /// The detail when there are some; `{names}` lists them.
        login: String,
        /// The detail when neither reader is on the machine; `{vars}` are
        /// the key variables.
        unread: String,
    },
}

/// A key variable that counts only beside a fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    /// The variable: `GEMINI_API_KEY`.
    pub var: String,
    /// The fact it needs.
    pub fact: String,
    /// The detail when it is set and the fact found.
    pub used: String,
    /// The detail when it is set and the fact is not found.
    pub unused: String,
    /// Added to the detail of a likely logout.
    pub absent: String,
}

/// The interpreter a harness's executable is a script for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    /// Its executable: `bun`.
    pub program: String,
    /// Its name: `Bun`.
    pub name: String,
    /// The oldest release that runs the harness.
    pub min: String,
    /// The command that installs or upgrades it.
    pub install: String,
}

/// A credential a connector's upstream takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    /// The header or variable that carries it.
    pub name: String,
    /// Whether the credential must be present.
    pub required: bool,
}

/// One connector Anvil can adopt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorEntry {
    /// The connector's ID in the catalog.
    pub id: String,
    /// The connector's display name.
    pub name: String,
    /// What the connector gives a harness access to.
    pub description: String,
    /// `remote-mcp` (a Streamable HTTP MCP server at `url`),
    /// `stdio-package` (a package run over stdio, such as `npx -y pkg`) or
    /// `stdio-command` (a CLI's own MCP mode, such as `gt mcp`).
    pub kind: String,
    /// The URL of a `remote-mcp` connector's server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The command a `stdio-command` connector runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The arguments the command takes.
    #[serde(default)]
    pub args: Vec<String>,
    /// The package a `stdio-package` connector runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// `server` (the remote server runs its own authorization, as the MCP
    /// authorization specification describes), `header` (a credential in
    /// an HTTP header), `env` (a credential in a variable) or `none`.
    pub auth: String,
    /// The credentials the upstream takes, if any.
    #[serde(default)]
    pub credentials: Vec<Credential>,
    /// The connector's homepage URL.
    pub homepage: String,
    /// `emdash:<catalog key>`.
    pub source: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessFile {
    harness: Vec<HarnessEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectorFile {
    connector: Vec<ConnectorEntry>,
}

const HARNESSES_TOML: &str = include_str!("../../../catalog/harnesses.toml");
const CONNECTORS_TOML: &str = include_str!("../../../catalog/connectors.toml");

/// Every harness CLI in `catalog/harnesses.toml`, in registry order.
#[allow(clippy::expect_used)] // ratchet: branchyard-controls
pub fn harnesses() -> &'static [HarnessEntry] {
    static PARSED: OnceLock<Vec<HarnessEntry>> = OnceLock::new();
    PARSED.get_or_init(|| {
        toml::from_str::<HarnessFile>(HARNESSES_TOML)
            .expect("catalog/harnesses.toml is checked by its tests")
            .harness
    })
}

/// The harness CLI with this registry ID.
pub fn harness(id: &str) -> Option<&'static HarnessEntry> {
    harnesses().iter().find(|h| h.id == id)
}

const LOGINS_TOML: &str = include_str!("../../../catalog/logins.toml");

/// How each harness's login is read, from `catalog/logins.toml`, by
/// catalog ID. The file is embedded and its tests parse it, so it cannot
/// fail here; if it ever did, every harness would be read by its
/// `auth_env` alone.
pub fn logins() -> &'static BTreeMap<String, LoginEntry> {
    static PARSED: OnceLock<BTreeMap<String, LoginEntry>> = OnceLock::new();
    PARSED.get_or_init(|| toml::from_str(LOGINS_TOML).unwrap_or_default())
}

/// How the harness with this catalog ID's login is read, when
/// `catalog/logins.toml` has a table for it.
pub fn login(id: &str) -> Option<&'static LoginEntry> {
    logins().get(id)
}

/// Every connector in `catalog/connectors.toml`, by ID.
#[allow(clippy::expect_used)] // ratchet: branchyard-controls
pub fn connectors() -> &'static [ConnectorEntry] {
    static PARSED: OnceLock<Vec<ConnectorEntry>> = OnceLock::new();
    PARSED.get_or_init(|| {
        toml::from_str::<ConnectorFile>(CONNECTORS_TOML)
            .expect("catalog/connectors.toml is checked by its tests")
            .connector
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::harness::HARNESSES;
    use crate::upstream::{self, EmdashAgent};
    use serde_json::Value;
    use std::collections::BTreeSet;

    const EMDASH_COMMIT: &str = "873a3e2067f4abc136272ed2b61abea3a2c07bcf";
    const ORCA_COMMIT: &str = "280733273545f0b3eeedc1be54b14d406239030e";
    const AGENTAPI_COMMIT: &str = "7c468d5b25ec9d3d76c41313cf4e08904ff5968d";
    const CODER_REGISTRY_COMMIT: &str = "a1600a8fd9f9274ceb0162115e44a35aa61a707e";

    fn text(value: &Value) -> Option<String> {
        value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
    }

    fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(text)
            .collect()
    }

    /// `npmDependency({...})`'s options, or the descriptor itself.
    fn host(agent: &EmdashAgent) -> (Option<&Value>, &Value) {
        let dependency = &agent.capabilities["hostDependency"];
        match dependency["$call"].as_str() {
            Some("npmDependency") => (Some(&dependency["$args"][0]), &Value::Null),
            Some(other) => panic!("unknown host dependency helper {other}"),
            None => (None, dependency),
        }
    }

    /// The executables an emdash plugin probes for.
    pub(crate) fn emdash_binaries(agent: &EmdashAgent) -> Vec<String> {
        match host(agent) {
            (Some(npm), _) => match strings(&npm["binaryNames"]) {
                names if names.is_empty() => vec![text(&npm["id"]).unwrap()],
                names => names,
            },
            (None, descriptor) => strings(&descriptor["binaryNames"]),
        }
    }

    /// One install option as a command line: `homebrewOption` and
    /// `npmDependency` as emdash's `host-dependency.ts` expands them.
    fn install_command(agent: &EmdashAgent, option: &Value) -> String {
        if option["$call"] == "homebrewOption" {
            let args = &option["$args"][0];
            let cask = if args["cask"] == true { " --cask" } else { "" };
            return format!("brew install{cask} {}", text(&args["formula"]).unwrap());
        }
        match &option["command"] {
            Value::String(command) => command.clone(),
            other => {
                let name = other["$ident"].as_str().expect("a command or a constant");
                agent.constants[name].clone()
            }
        }
    }

    fn emdash_install(agent: &EmdashAgent) -> Vec<String> {
        let mut out = Vec::new();
        let options = match host(agent) {
            (Some(npm), _) => {
                let flags = text(&npm["installFlags"])
                    .map(|f| format!(" {f}"))
                    .unwrap_or_default();
                out.push(format!(
                    "npm install -g {}{flags}",
                    text(&npm["package"]).unwrap()
                ));
                npm["extraOptions"]["linux"].clone()
            }
            (None, descriptor) => descriptor["installCommands"]["linux"].clone(),
        };
        for option in options.as_array().into_iter().flatten() {
            out.push(install_command(agent, option));
        }
        out
    }

    /// Corrections to the generated entries, from running the CLIs in
    /// October 2026; they replace what the upstream sources gave for the
    /// field. Antigravity also needs `modelProvider` set to `gemini` in its
    /// settings before it reads the key (the catalog has no field for that;
    /// `branchyard::inventory` reports it and the provisioner sets it).
    fn curated(e: &mut HarnessEntry) {
        let strings = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect();
        match e.id.as_str() {
            // DeepSeek Harness 0.2.0-rc.2: one `dsh` binary boots a profile
            // (Orca detects its `dsh-tui` launcher, which Branchyard does not
            // use: `dsh --profile acp` is the ACP profile).
            "deepseek-harness" => {
                e.binaries = strings(&["dsh"]);
                e.homepage = Some("https://github.com/deepseek-ai/deepseek-harness".into());
                e.install = strings(&["npm install -g @deepseek-ai/dsh"]);
                e.auth_env = strings(&["DEEPSEEK_API_KEY"]);
            }
            // Claude Code 2.1.310 reads a gateway's token from
            // ANTHROPIC_AUTH_TOKEN (with ANTHROPIC_BASE_URL), as Z.ai's
            // Anthropic-compatible endpoint asks; the vendored sources
            // record only ANTHROPIC_API_KEY.
            "claude-code" => {
                e.auth_env = strings(&["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]);
            }
            // Gemini CLI 0.63.0: no emdash plugin describes it. `gemini`
            // signs in on its first interactive run.
            "gemini-cli" => {
                e.homepage = Some("https://geminicli.com/docs/cli/cli-reference/".into());
                e.install = strings(&["npm install -g @google/gemini-cli"]);
                e.login = Some("gemini".into());
                e.auth_env = strings(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]);
            }
            // Antigravity CLI 1.3.3: `agy` signs in with Google on its first
            // interactive run and reads a key from GEMINI_API_KEY only.
            "antigravity" => {
                e.login = Some("agy".into());
                e.auth_env = strings(&["GEMINI_API_KEY"]);
            }
            // Oh My Pi 18.10.3: `omp` is a Bun script and needs Bun 1.3.14
            // or newer on PATH (its installer installs or upgrades Bun;
            // `bun install -g` needs it there already). `omp login PROVIDER`
            // stores an OAuth login; provider keys are read from the
            // environment, `ZAI_API_KEY` for its `zai` provider (GLM) among
            // them. Its resume flag is `--resume` (`-r`), not the
            // `--session` the vendored sources recorded.
            "oh-my-pi" => {
                e.login = Some("omp login".into());
                e.auth_env = strings(&[
                    "GEMINI_API_KEY",
                    "OPENAI_API_KEY",
                    "ANTHROPIC_API_KEY",
                    "OPENROUTER_API_KEY",
                    "ZAI_API_KEY",
                ]);
                e.resume_flag = Some("--resume".into());
            }
            // OpenCode 1.19.4: `opencode providers list` shows a Google
            // provider for `GEMINI_API_KEY`, but its Google SDK does not
            // get it: `opencode run` and `opencode acp` fail with "Google
            // Generative AI API key is missing" until the key is in
            // `GOOGLE_GENERATIVE_AI_API_KEY` (checked 25 October 2026), so
            // the vendored list's `GEMINI_API_KEY` is replaced. The
            // Anthropic and OpenAI SDKs read their variables themselves.
            "opencode" => {
                e.auth_env = strings(&[
                    "ANTHROPIC_API_KEY",
                    "OPENAI_API_KEY",
                    "GOOGLE_GENERATIVE_AI_API_KEY",
                    "ORCAROUTER_API_KEY",
                ]);
            }
            // Pi 0.73.1: no login command (`/login` inside pi stores a key
            // or an OAuth login in `~/.pi/agent/auth.json`, which pi
            // creates as `{}` on its first start), and a provider key is
            // read from the environment (`docs/providers.md` of the
            // package lists about twenty; `pi --list-models` showed the
            // google and zai providers for `GEMINI_API_KEY` and
            // `ZAI_API_KEY`). The vendored sources record none.
            "pi" => {
                e.auth_env = strings(&[
                    "ANTHROPIC_API_KEY",
                    "OPENAI_API_KEY",
                    "GEMINI_API_KEY",
                    "OPENROUTER_API_KEY",
                    "ZAI_API_KEY",
                ]);
            }
            // Cursor CLI: its driver profile launches `agent` (`cursor-acp`
            // runs `agent acp`), the name the Cursor CLI installs; Orca and
            // emdash know only the older `cursor-agent`. Detection looks for
            // both, so a machine with only `agent` has Cursor.
            "cursor" => {
                if !e.binaries.iter().any(|b| b == "agent") {
                    e.binaries.push("agent".into());
                }
            }
            _ => {}
        }
    }

    fn entry(harness: &crate::harness::Harness) -> HarnessEntry {
        let emdash = upstream_emdash();
        let orca = upstream::orca_agents();
        let orca_names = upstream::orca_agent_names();
        let agent = harness.emdash.map(|id| &emdash[id]);
        let mut binaries: Vec<String> = agent.map(emdash_binaries).unwrap_or_default();
        for key in harness.orca {
            let config = &orca[*key];
            let mut detect = vec![text(&config["detectCmd"]).unwrap()];
            detect.extend(strings(&config["detectCmdAliases"]));
            // Orca's Agent Teams mode is detected through Orca's own CLI.
            if text(&config["launchCmd"]).is_some_and(|l| l.starts_with("orca ")) {
                continue;
            }
            for binary in detect {
                if !binaries.contains(&binary) {
                    binaries.push(binary);
                }
            }
        }
        let orca_name = harness.orca.iter().find_map(|key| orca_names[*key].clone());
        let name = harness
            .target
            .map(str::to_owned)
            .or_else(|| agent.and_then(|a| text(&a.meta["name"])))
            .or(orca_name)
            .unwrap_or_else(|| harness.id.to_owned());
        let methods = agent
            .map(|a| a.capabilities["auth"]["methods"].clone())
            .unwrap_or_default();
        let methods = methods.as_array().cloned().unwrap_or_default();
        let login = methods.iter().find(|m| m["kind"] == "cli-login").map(|m| {
            let mut line = vec![binaries[0].clone()];
            line.extend(strings(&m["args"]));
            line.join(" ")
        });
        let mut auth_env: Vec<String> = Vec::new();
        for method in methods.iter().filter(|m| m["kind"] == "api-key") {
            for var in method["envVars"].as_array().into_iter().flatten() {
                let name = text(&var["name"]).unwrap();
                if !auth_env.contains(&name) {
                    auth_env.push(name);
                }
            }
        }
        let models: BTreeSet<String> = agent
            .and_then(|a| {
                a.capabilities["models"]["modelOptions"]
                    .as_object()
                    .cloned()
            })
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let command = agent.map(|a| a.command.clone()).unwrap_or_default();
        let mut sources: Vec<String> = harness
            .emdash
            .map(|id| format!("emdash:{id}"))
            .into_iter()
            .collect();
        sources.extend(harness.orca.iter().map(|key| format!("orca:{key}")));
        sources.extend(harness.agentapi.map(|name| format!("agentapi:{name}")));
        sources.extend(harness.coder_module.map(|dir| format!("coder:{dir}")));
        let mut e = HarnessEntry {
            id: harness.id.to_owned(),
            name,
            binaries,
            homepage: agent.and_then(|a| text(&a.meta["websiteUrl"])),
            install: agent.map(emdash_install).unwrap_or_default(),
            login,
            auth_env,
            models: models.into_iter().collect(),
            model_flag: text(&command["modelFlag"]),
            resume_flag: text(&command["resumeFlag"]),
            sources,
        };
        curated(&mut e);
        e
    }

    fn upstream_emdash() -> &'static std::collections::BTreeMap<String, EmdashAgent> {
        static AGENTS: OnceLock<std::collections::BTreeMap<String, EmdashAgent>> = OnceLock::new();
        AGENTS.get_or_init(upstream::emdash_agents)
    }

    /// A TOML basic string.
    fn q(text: &str) -> String {
        let mut out = String::from("\"");
        for c in text.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    fn list(items: &[String]) -> String {
        let quoted: Vec<String> = items.iter().map(|i| q(i)).collect();
        format!("[{}]", quoted.join(", "))
    }

    pub(crate) fn harnesses_toml() -> String {
        let mut out = format!(
            "# Generated from pinned upstream sources by branchyard-controls' catalog tests;\n\
             # do not edit. Regenerate with\n\
             #   BRANCHYARD_BLESS=1 cargo test -p branchyard-controls catalog\n\
             # and review the diff. See crates/branchyard-controls/src/catalog.rs.\n\
             #\n\
             # Derived from generalaction/emdash at {EMDASH_COMMIT}\n\
             # (packages/plugins/src/agents/impl/*/index.ts, with the install helpers of\n\
             # packages/core/src/services/agent-plugins/api/plugins/helpers/host-dependency.ts),\n\
             # Copyright 2026 General Action, Inc., licensed under the Apache License,\n\
             # Version 2.0 (vendor/emdash/LICENSE.md),\n\
             # and stablyai/orca at {ORCA_COMMIT}\n\
             # (src/shared/tui-agent-config.ts, src/shared/tui-agent.ts), Copyright (c) 2026\n\
             # Lovecast Inc., MIT License (vendor/orca/LICENSE);\n\
             # coder/agentapi at {AGENTAPI_COMMIT}\n\
             # (lib/msgfmt/msgfmt.go, cmd/server/server.go: its agent types and their\n\
             # aliases), Copyright (c) 2025 Coder Technologies, Inc., MIT License\n\
             # (vendor/agentapi/LICENSE); and coder/registry at {CODER_REGISTRY_COMMIT}\n\
             # (registry/coder/modules/*/README.md, registry/coder/modules/*/main.tf: its\n\
             # agent modules), Apache License, Version 2.0 (vendor/coder-registry/LICENSE;\n\
             # no copyright notice at the repository root).\n\
             # Modified for Branchyard: the TypeScript plugin definitions were reduced to\n\
             # data (Linux install commands with npm and Homebrew helpers expanded, CLI\n\
             # login, API-key variables, model names, model and resume flags) and joined\n\
             # with Orca's executables, agentapi's type names and the registry's module\n\
             # names under Branchyard's harness IDs. DeepSeek Harness, Gemini CLI and\n\
             # Antigravity are corrected where their CLIs were run (`curated` in\n\
             # crates/branchyard-controls/src/catalog.rs).\n"
        );
        for harness in HARNESSES {
            let e = entry(harness);
            out.push_str("\n[[harness]]\n");
            out.push_str(&format!("id = {}\nname = {}\n", q(&e.id), q(&e.name)));
            out.push_str(&format!("binaries = {}\n", list(&e.binaries)));
            if let Some(homepage) = &e.homepage {
                out.push_str(&format!("homepage = {}\n", q(homepage)));
            }
            out.push_str(&format!("install = {}\n", list(&e.install)));
            if let Some(login) = &e.login {
                out.push_str(&format!("login = {}\n", q(login)));
            }
            out.push_str(&format!("auth_env = {}\n", list(&e.auth_env)));
            out.push_str(&format!("models = {}\n", list(&e.models)));
            if let Some(flag) = &e.model_flag {
                out.push_str(&format!("model_flag = {}\n", q(flag)));
            }
            if let Some(flag) = &e.resume_flag {
                out.push_str(&format!("resume_flag = {}\n", q(flag)));
            }
            out.push_str(&format!("sources = {}\n", list(&e.sources)));
        }
        out
    }

    fn connector(key: &str, value: &Value) -> ConnectorEntry {
        let config = &value["config"];
        let credentials: Vec<Credential> = value["credentialKeys"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| Credential {
                name: text(&c["key"]).unwrap(),
                required: c["required"] == true,
            })
            .collect();
        let in_headers = credentials
            .iter()
            .any(|c| config["headers"].get(&c.name).is_some());
        let in_env = credentials
            .iter()
            .any(|c| config["env"].get(&c.name).is_some());
        let remote = config["type"] == "http";
        let auth = match (remote, in_headers, in_env) {
            (_, true, _) => "header",
            (_, _, true) => "env",
            (true, false, false) => "server",
            (false, false, false) => "none",
        };
        assert!(
            credentials.is_empty() || auth != "none" && auth != "server",
            "{key}: credentials {credentials:?} are neither headers nor variables"
        );
        let command = text(&config["command"]);
        let args = strings(&config["args"]);
        let package = match command.as_deref() {
            Some("npx" | "bunx" | "uvx" | "pnpx") => {
                args.iter().find(|a| !a.starts_with('-')).cloned()
            }
            _ => None,
        };
        let kind = match (remote, &package) {
            (true, _) => "remote-mcp",
            (false, Some(_)) => "stdio-package",
            (false, None) => "stdio-command",
        };
        ConnectorEntry {
            id: key.replace('_', "-"),
            name: text(&value["name"]).unwrap(),
            description: text(&value["description"]).unwrap(),
            kind: kind.to_owned(),
            url: text(&config["url"]),
            command: (!remote).then_some(command).flatten(),
            args: if remote { Vec::new() } else { args },
            package,
            auth: auth.to_owned(),
            credentials,
            homepage: text(&value["docsUrl"]).unwrap(),
            source: format!("emdash:{key}"),
        }
    }

    pub(crate) fn connectors_toml() -> String {
        let mut out = format!(
            "# Generated from a pinned upstream source by branchyard-controls' catalog tests;\n\
             # do not edit. Regenerate with\n\
             #   BRANCHYARD_BLESS=1 cargo test -p branchyard-controls catalog\n\
             # and review the diff. See crates/branchyard-controls/src/catalog.rs and\n\
             # docs/connectors.md (the starting list Anvil adopts connectors from).\n\
             #\n\
             # Derived from generalaction/emdash at {EMDASH_COMMIT}\n\
             # (apps/emdash-desktop/src/core/primitives/mcp/api/catalog.ts), Copyright 2026\n\
             # General Action, Inc., licensed under the Apache License, Version 2.0\n\
             # (vendor/emdash/LICENSE.md).\n\
             # Modified for Branchyard: each server's configuration was reduced to its kind,\n\
             # URL or command, the package it runs, how it authenticates and the names of\n\
             # its credentials (placeholder values dropped); entries are sorted by ID.\n"
        );
        let mut entries: Vec<ConnectorEntry> = upstream::emdash_mcp()
            .iter()
            .map(|(key, value)| connector(key, value))
            .collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        for e in entries {
            out.push_str("\n[[connector]]\n");
            out.push_str(&format!(
                "id = {}\nname = {}\ndescription = {}\nkind = {}\n",
                q(&e.id),
                q(&e.name),
                q(&e.description),
                q(&e.kind)
            ));
            if let Some(url) = &e.url {
                out.push_str(&format!("url = {}\n", q(url)));
            }
            if let Some(command) = &e.command {
                out.push_str(&format!(
                    "command = {}\nargs = {}\n",
                    q(command),
                    list(&e.args)
                ));
            }
            if let Some(package) = &e.package {
                out.push_str(&format!("package = {}\n", q(package)));
            }
            out.push_str(&format!("auth = {}\n", q(&e.auth)));
            let credentials: Vec<String> = e
                .credentials
                .iter()
                .map(|c| format!("{{ name = {}, required = {} }}", q(&c.name), c.required))
                .collect();
            out.push_str(&format!("credentials = [{}]\n", credentials.join(", ")));
            out.push_str(&format!(
                "homepage = {}\nsource = {}\n",
                q(&e.homepage),
                q(&e.source)
            ));
        }
        out
    }

    pub const ORCA_CLAUDE_PRICING: &str =
        "vendor/orca/src/main/claude-usage/claude-model-pricing.ts";
    pub const ORCA_CODEX_PRICING: &str = "vendor/orca/src/main/codex-usage/codex-model-pricing.ts";

    /// The value assigned to `const NAME` in `source`.
    fn constant(source: &str, name: &str) -> Value {
        let at = source
            .find(&format!("const {name}"))
            .unwrap_or_else(|| panic!("no const {name}"));
        let eq = at + source[at..].find('=').unwrap() + 1;
        crate::tsdata::value_at(source, eq).unwrap().0
    }

    fn price(value: &Value) -> String {
        let n = value
            .as_f64()
            .unwrap_or_else(|| panic!("not a number: {value}"));
        match n.fract() == 0.0 {
            true => format!("{n:.1}"),
            false => format!("{n}"),
        }
    }

    /// An object literal with its spreads and identifiers resolved from
    /// `consts`.
    fn resolved(
        value: &Value,
        consts: &std::collections::BTreeMap<&str, Value>,
    ) -> serde_json::Map<String, Value> {
        let mut out = serde_json::Map::new();
        for (key, v) in value.as_object().unwrap() {
            let v = match v["$ident"].as_str() {
                Some(name) => consts[name].clone(),
                None => v.clone(),
            };
            match key.starts_with("$spread") {
                true => out.extend(resolved(&v, consts)),
                false => {
                    out.insert(key.clone(), v);
                }
            }
        }
        out
    }

    pub(crate) fn pricing_toml() -> String {
        let claude = upstream::read(ORCA_CLAUDE_PRICING);
        let codex = upstream::read(ORCA_CODEX_PRICING);
        let mut consts = std::collections::BTreeMap::new();
        consts.insert(
            "LONG_CONTEXT_THRESHOLD_TOKENS",
            constant(&claude, "LONG_CONTEXT_THRESHOLD_TOKENS"),
        );
        let sonnet = constant(&claude, "SONNET_LONG_CONTEXT_PRICING");
        consts.insert(
            "SONNET_LONG_CONTEXT_PRICING",
            Value::Object(resolved(&sonnet, &consts)),
        );
        let mut out = format!(
            "# Generated from pinned upstream sources by branchyard-controls' catalog tests;\n\
             # do not edit. Regenerate with\n\
             #   BRANCHYARD_BLESS=1 cargo test -p branchyard-controls catalog\n\
             # and review the diff. Read by `by usage` (crates/branchyard-cli/src/usage.rs;\n\
             # docs/usage.md) to estimate what the tokens in a window cost.\n\
             #\n\
             # Derived from stablyai/orca at {ORCA_COMMIT}\n\
             # (src/main/claude-usage/claude-model-pricing.ts,\n\
             # src/main/codex-usage/codex-model-pricing.ts), Copyright (c) 2026 Lovecast Inc.,\n\
             # MIT License (vendor/orca/LICENSE).\n\
             # Modified for Branchyard: the TypeScript tables were reduced to data, dollars\n\
             # per million tokens; Claude's long-context tier is written out on each model\n\
             # it applies to, and entries are sorted by model.\n\n\
             codex_long_context_threshold = {}\n",
            constant(&codex, "LONG_CONTEXT_THRESHOLD_TOKENS")
        );
        out.push_str("\n[claude_aliases]\n");
        let aliases = constant(&claude, "MODEL_ALIASES");
        let mut aliases: Vec<(&String, &Value)> = aliases.as_object().unwrap().iter().collect();
        aliases.sort_by(|a, b| a.0.cmp(b.0));
        for (from, to) in aliases {
            out.push_str(&format!("{} = {}\n", q(from), q(to.as_str().unwrap())));
        }
        let table = constant(&claude, "MODEL_PRICING");
        let mut models: Vec<(&String, &Value)> = table.as_object().unwrap().iter().collect();
        models.sort_by(|a, b| a.0.cmp(b.0));
        for (model, value) in models {
            let p = resolved(value, &consts);
            out.push_str(&format!("\n[[claude]]\nmodel = {}\n", q(model)));
            for (ts, toml) in [
                ("input", "input"),
                ("output", "output"),
                ("cacheRead", "cache_read"),
                ("cacheWrite", "cache_write"),
                ("cacheWrite1h", "cache_write_1h"),
                ("thresholdTokens", "threshold_tokens"),
                ("inputAboveThreshold", "input_above"),
                ("outputAboveThreshold", "output_above"),
                ("cacheReadAboveThreshold", "cache_read_above"),
                ("cacheWriteAboveThreshold", "cache_write_above"),
                ("cacheWrite1hAboveThreshold", "cache_write_1h_above"),
            ] {
                if let Some(v) = p.get(ts) {
                    out.push_str(&format!("{toml} = {}\n", price(v)));
                }
            }
            let known = [
                "input",
                "output",
                "cacheRead",
                "cacheWrite",
                "cacheWrite1h",
                "thresholdTokens",
                "inputAboveThreshold",
                "outputAboveThreshold",
                "cacheReadAboveThreshold",
                "cacheWriteAboveThreshold",
                "cacheWrite1hAboveThreshold",
            ];
            for key in p.keys() {
                assert!(
                    known.contains(&key.as_str()),
                    "Orca's Claude pricing gained {key}"
                );
            }
        }
        let table = {
            let at = codex.find("const MODEL_PRICING").unwrap();
            let brace = at + codex[at..].find("= {").unwrap() + 2;
            crate::tsdata::value_at(&codex, brace).unwrap().0
        };
        let mut models: Vec<(&String, &Value)> = table.as_object().unwrap().iter().collect();
        models.sort_by(|a, b| a.0.cmp(b.0));
        for (model, p) in models {
            out.push_str(&format!(
                "\n[[codex]]\nmodel = {}\ninput = {}\ncached_input = {}\noutput = {}\n",
                q(model),
                price(&p["input"]),
                price(&p["cachedInput"]),
                price(&p["output"])
            ));
            if let Some(long) = p.get("longContext") {
                out.push_str(&format!(
                    "long_context = {{ input = {}, cached_input = {}, output = {} }}\n",
                    price(&long["input"]),
                    price(&long["cachedInput"]),
                    price(&long["output"])
                ));
            }
            for key in p.as_object().unwrap().keys() {
                assert!(
                    ["input", "cachedInput", "output", "longContext"].contains(&key.as_str()),
                    "Orca's Codex pricing gained {key}"
                );
            }
        }
        out
    }

    #[test]
    fn pricing_catalog_matches_the_vendored_sources() {
        fresh("catalog/pricing.toml", pricing_toml());
        let text = upstream::read("catalog/pricing.toml");
        assert!(text.contains("model = \"claude-sonnet-4-5\"\ninput = 3.0"));
        assert!(text.contains("threshold_tokens = 200000.0"));
        assert!(text.contains("codex_long_context_threshold = 272000"));
    }

    /// The checked-in file is what the vendored sources generate.
    fn fresh(relative: &str, expected: String) {
        let path = upstream::repository().join(relative);
        if std::env::var_os("BRANCHYARD_BLESS").is_some() {
            std::fs::write(&path, &expected).unwrap();
        }
        let actual = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            actual == expected,
            "{relative} differs from what the vendored sources generate; regenerate with \
             BRANCHYARD_BLESS=1 cargo test -p branchyard-controls catalog and review the diff"
        );
    }

    #[test]
    fn harness_catalog_matches_the_vendored_sources() {
        fresh("catalog/harnesses.toml", harnesses_toml());
        let entries = harnesses();
        assert_eq!(entries.len(), HARNESSES.len());
        for (entry, harness) in entries.iter().zip(HARNESSES) {
            assert_eq!(entry.id, harness.id, "catalog order is registry order");
        }
        let claude = harness("claude-code").unwrap();
        assert_eq!(claude.binaries, ["claude"]);
        assert_eq!(claude.login.as_deref(), Some("claude auth login"));
        assert_eq!(
            claude.auth_env,
            ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]
        );
        assert_eq!(
            claude.install[0],
            "curl -fsSL https://claude.ai/install.sh | bash"
        );
        let codex = harness("codex").unwrap();
        assert_eq!(codex.install[0], "npm install -g @openai/codex");
        assert!(codex
            .install
            .contains(&"brew install --cask codex".to_owned()));
        assert_eq!(codex.login.as_deref(), Some("codex login --device-auth"));
        assert!(!codex.models.is_empty());
        let muse = harness("muse-code").unwrap();
        assert!(muse.install[0].starts_with("curl"), "{:?}", muse.install);
        // Named by Coder's sources too.
        assert!(
            claude.sources.contains(&"agentapi:claude".to_owned()),
            "{:?}",
            claude.sources
        );
        assert!(claude.sources.contains(&"coder:claude-code".to_owned()));
        let q = harness("amazon-q").unwrap();
        assert_eq!(q.sources, ["agentapi:amazonq"]);
        assert!(q.binaries.is_empty());
        // Known about, from Orca alone.
        let trae = harness("trae").unwrap();
        assert_eq!(trae.binaries, ["traecli"]);
        assert!(trae.install.is_empty());
        // Corrected where the CLIs were run (DeepSeek Harness 0.2.0-rc.2,
        // Gemini CLI 0.63.0, Antigravity CLI 1.3.3).
        let dsh = harness("deepseek-harness").unwrap();
        assert_eq!(dsh.binaries, ["dsh"]);
        assert_eq!(dsh.install, ["npm install -g @deepseek-ai/dsh"]);
        assert_eq!(dsh.auth_env, ["DEEPSEEK_API_KEY"]);
        let gemini = harness("gemini-cli").unwrap();
        assert_eq!(gemini.binaries, ["gemini"]);
        assert_eq!(gemini.install, ["npm install -g @google/gemini-cli"]);
        assert_eq!(gemini.login.as_deref(), Some("gemini"));
        assert_eq!(gemini.auth_env, ["GEMINI_API_KEY", "GOOGLE_API_KEY"]);
        let agy = harness("antigravity").unwrap();
        assert_eq!(agy.binaries, ["agy", "antigravity"]);
        assert_eq!(agy.login.as_deref(), Some("agy"));
        assert_eq!(agy.auth_env, ["GEMINI_API_KEY"]);
        let known = entries.iter().filter(|e| !e.sources.is_empty()).count();
        assert!(known >= 40, "only {known} harnesses described upstream");
    }

    /// Orca's resume guard strips `--resume` selectors from a persisted
    /// Claude command; the catalog's Claude resume flag must be one it
    /// recognizes, or the two disagree about Claude's CLI.
    #[test]
    fn claude_resume_flag_is_one_orca_recognizes() {
        let source = upstream::read(upstream::ORCA_RESUME);
        let flag = harness("claude-code").unwrap().resume_flag.clone().unwrap();
        assert!(
            source.contains(&format!("token === '{flag}'")),
            "Orca's resume guard does not recognize {flag}"
        );
    }

    #[test]
    fn connector_catalog_matches_the_vendored_source() {
        fresh("catalog/connectors.toml", connectors_toml());
        let entries = connectors();
        assert_eq!(entries.len(), 55);
        assert_eq!(upstream::emdash_mcp().len(), entries.len());
        let ids: BTreeSet<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), entries.len());
        let github_like = entries.iter().find(|e| e.id == "context7").unwrap();
        assert_eq!(github_like.kind, "remote-mcp");
        assert_eq!(github_like.auth, "header");
        assert_eq!(
            github_like.credentials,
            [Credential {
                name: "CONTEXT7_API_KEY".into(),
                required: false
            }]
        );
        let resend = entries.iter().find(|e| e.id == "resend").unwrap();
        assert_eq!(
            (resend.kind.as_str(), resend.auth.as_str()),
            ("stdio-package", "env")
        );
        assert_eq!(resend.package.as_deref(), Some("resend-mcp"));
        let graphite = entries.iter().find(|e| e.id == "graphite").unwrap();
        assert_eq!(graphite.kind, "stdio-command");
        assert_eq!(graphite.command.as_deref(), Some("gt"));
        // No placeholder credential value reaches the catalog.
        for file in [CONNECTORS_TOML, HARNESSES_TOML] {
            assert!(!file.contains("YOUR_"), "a placeholder value was copied");
        }
        for entry in entries {
            assert!(entry.homepage.starts_with("https://"), "{}", entry.id);
            match entry.kind.as_str() {
                "remote-mcp" => assert!(entry
                    .url
                    .as_deref()
                    .is_some_and(|u| u.starts_with("https://"))),
                _ => assert!(entry.command.is_some()),
            }
        }
    }

    /// `catalog/logins.toml` parses (the runtime falls back to nothing if it
    /// did not), and names only harnesses detection looks for.
    #[test]
    fn login_catalog_parses_and_names_detectable_harnesses() {
        let parsed: BTreeMap<String, LoginEntry> = toml::from_str(LOGINS_TOML).unwrap();
        assert_eq!(&parsed, logins());
        for (id, entry) in &parsed {
            let harness = harness(id).unwrap_or_else(|| panic!("{id} is not in the catalog"));
            assert!(
                !harness.binaries.is_empty(),
                "{id} has no executable, so detection never reads its login"
            );
            for var in entry.env.iter().chain(&entry.quiet_env) {
                assert!(
                    !harness.auth_env.contains(var),
                    "{id}: {var} is already the catalog's auth_env"
                );
            }
        }
        // What the file says it covers.
        let codex = login("codex").unwrap();
        assert_eq!(
            codex.status.as_ref().unwrap().args,
            ["login", "status"],
            "emdash's checkStatus"
        );
        assert!(matches!(codex.probes[0], Probe::Provider { .. }));
        assert_eq!(login("pi").unwrap().probes.len(), 1);
        assert!(login("goose").is_none(), "read by its auth_env alone");
        // An unknown field is refused, so a typo cannot silently drop a rule.
        assert!(toml::from_str::<BTreeMap<String, LoginEntry>>("[x]\nenv_vars = []\n").is_err());
        assert!(toml::from_str::<BTreeMap<String, LoginEntry>>(
            "[[x.probe]]\nkind = \"grep\"\nfile = \"f\"\npattern = \"p\"\nfact = \"f\"\nlogn = \"\"\n"
        )
        .is_err());
    }
}
