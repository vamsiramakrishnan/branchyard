//! The vendored emdash and Orca sources, read as data for tests: the
//! registry's mapping tests and the catalogs' generation.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::tsdata;

pub fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub const EMDASH_AGENTS: &str = "vendor/emdash/packages/plugins/src/agents/impl";
pub const EMDASH_MCP: &str =
    "vendor/emdash/apps/emdash-desktop/src/core/primitives/mcp/api/catalog.ts";
pub const ORCA_AGENTS: &str = "vendor/orca/src/shared/tui-agent-config.ts";
pub const ORCA_AGENT_NAMES: &str = "vendor/orca/src/shared/tui-agent.ts";
pub const ORCA_RESUME: &str = "vendor/orca/src/shared/agent-resume-launch-command.ts";

pub fn read(relative: &str) -> String {
    fs::read_to_string(repository().join(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"))
}

/// One emdash agent plugin: `definePlugin`'s two arguments, the options
/// given to `buildStandardCommand` (or null), and the file's string
/// constants, by directory.
pub struct EmdashAgent {
    pub meta: Value,
    pub capabilities: Value,
    pub command: Value,
    pub constants: BTreeMap<String, String>,
}

/// `const NAME = 'text'` declarations, for identifiers a literal names.
fn constants(source: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in source.lines() {
        let line = line.trim_start();
        let Some(rest) = line
            .strip_prefix("const ")
            .or_else(|| line.strip_prefix("export const "))
        else {
            continue;
        };
        let Some((name, value)) = rest.split_once('=') else {
            continue;
        };
        let name = name.split(':').next().unwrap_or_default().trim();
        let value = value.trim_start();
        if !value.starts_with(['\'', '"', '`']) {
            continue;
        }
        if let Ok((Value::String(text), _)) = tsdata::value_at(value, 0) {
            out.insert(name.to_owned(), text);
        }
    }
    out
}

pub fn emdash_agents() -> BTreeMap<String, EmdashAgent> {
    let mut agents = BTreeMap::new();
    for entry in fs::read_dir(repository().join(EMDASH_AGENTS)).unwrap() {
        let dir = entry.unwrap().path();
        let file = dir.join("index.ts");
        if !file.is_file() {
            continue;
        }
        let source = fs::read_to_string(&file).unwrap();
        let mut args = tsdata::call_args(&source, "definePlugin")
            .unwrap_or_else(|e| panic!("{}: {e}", file.display()))
            .into_iter();
        let command = match source.contains("buildStandardCommand(ctx,") {
            true => tsdata::call_args(&source, "buildStandardCommand")
                .unwrap_or_else(|e| panic!("{}: {e}", file.display()))
                .remove(1),
            false => Value::Null,
        };
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        agents.insert(
            name,
            EmdashAgent {
                meta: args.next().unwrap(),
                capabilities: args.next().unwrap(),
                command,
                constants: constants(&source),
            },
        );
    }
    agents
}

/// Orca's `TUI_AGENT_CONFIG_SOURCE`, by agent key.
pub fn orca_agents() -> BTreeMap<String, Value> {
    let source = read(ORCA_AGENTS);
    let marker = "const TUI_AGENT_CONFIG_SOURCE";
    let start = source.find(marker).expect("TUI_AGENT_CONFIG_SOURCE");
    let open = start + source[start..].find("= {").unwrap() + 2;
    let (value, _) = tsdata::value_at(&source, open).unwrap();
    match value {
        Value::Object(map) => map.into_iter().collect(),
        other => panic!("TUI_AGENT_CONFIG_SOURCE is {other}"),
    }
}

/// Orca's `TuiAgent` union, with the display name each member's comment
/// gives, when it has one.
pub fn orca_agent_names() -> BTreeMap<String, Option<String>> {
    let source = read(ORCA_AGENT_NAMES);
    let start = source.find("export type TuiAgent =").expect("TuiAgent");
    let mut names = BTreeMap::new();
    for line in source[start..].lines().skip(1) {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("| '") else {
            break;
        };
        let (key, rest) = rest.split_once('\'').unwrap();
        let name = rest
            .split_once("//")
            .map(|(_, comment)| comment.trim().to_owned())
            .filter(|c| !c.is_empty());
        names.insert(key.to_owned(), name);
    }
    names
}

/// emdash's MCP `catalogData`, in source order.
pub fn emdash_mcp() -> Vec<(String, Value)> {
    let source = read(EMDASH_MCP);
    let start = source.find("export const catalogData").unwrap();
    let open = start + source[start..].find("= {").unwrap() + 2;
    let (value, _) = tsdata::value_at(&source, open).unwrap();
    match value {
        Value::Object(map) => map.into_iter().collect(),
        other => panic!("catalogData is {other}"),
    }
}
