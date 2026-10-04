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
//! An entry is knowledge, not support: whether Branchyard can drive a
//! harness is its profile's business (`branchyard_harness::profiles`), and a
//! connector is used only once Anvil compiles it (`docs/connectors.md`).

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

/// Every connector in `catalog/connectors.toml`, by ID.
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
        HarnessEntry {
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
        }
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
             # Lovecast Inc., MIT License (vendor/orca/LICENSE).\n\
             # Modified for Branchyard: the TypeScript plugin definitions were reduced to\n\
             # data (Linux install commands with npm and Homebrew helpers expanded, CLI\n\
             # login, API-key variables, model names, model and resume flags) and joined\n\
             # with Orca's executables under Branchyard's harness IDs.\n"
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
        assert_eq!(claude.auth_env, ["ANTHROPIC_API_KEY"]);
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
        // Known about, from Orca alone.
        let trae = harness("trae").unwrap();
        assert_eq!(trae.binaries, ["traecli"]);
        assert!(trae.install.is_empty());
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
}
