// Derived from generalaction/emdash
// packages/core/src/primitives/emdash-config/api/emdash-config.ts, at
// revision 873a3e2067f4abc136272ed2b61abea3a2c07bcf.
// Copyright 2026 General Action, Inc. Licensed under the Apache License,
// Version 2.0; the license is in vendor/emdash/LICENSE.md.
// Modified for Branchyard: the zod schema was translated to Rust over
// serde_json, keeping its shape (`preservePatterns` without `.emdash.json`
// itself, `shellSetup`, and `scripts.prepare`, `setup`, `run` and
// `teardown`) and its leniency (unknown keys ignored); a wrong type is an
// error here rather than a fall back to the defaults, and the result is
// mapped to `[workspace]` (prepare then setup as setup commands, emdash's
// script variables renamed to Branchyard's), which emdash does not do.

//! `.emdash.json`.

use serde_json::Value;

use super::{rename_variables, script_commands, Imported, Reader};

pub const FILE: &str = ".emdash.json";

/// emdash's script variables and Branchyard's equivalents.
const VARIABLES: &[(&str, &str)] = &[
    ("EMDASH_TASK_PATH", "BRANCHYARD_WORKTREE"),
    ("EMDASH_ROOT_PATH", "BRANCHYARD_ROOT"),
    ("EMDASH_PORT", "BRANCHYARD_PORT"),
];

/// emdash's parsed configuration: `parseEmdashConfig`'s result.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EmdashConfig {
    pub preserve_patterns: Option<Vec<String>>,
    pub shell_setup: Option<String>,
    pub prepare: Option<String>,
    pub setup: Option<String>,
    pub run: Option<String>,
    pub teardown: Option<String>,
}

fn optional_string(object: &Value, key: &str, at: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("{at}{key} must be a string")),
    }
}

/// `parseEmdashConfig`: the file's JSON, checked against emdash's schema.
pub fn parse(text: &str) -> Result<EmdashConfig, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if !value.is_object() {
        return Err("must be a JSON object".into());
    }
    let preserve_patterns = match value.get("preservePatterns") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => {
            let mut patterns = Vec::new();
            for item in items {
                let pattern = item
                    .as_str()
                    .ok_or("preservePatterns must be an array of strings")?;
                if pattern != FILE {
                    patterns.push(pattern.to_owned());
                }
            }
            Some(patterns)
        }
        Some(_) => return Err("preservePatterns must be an array of strings".into()),
    };
    let scripts = match value.get("scripts") {
        None | Some(Value::Null) => Value::Object(Default::default()),
        Some(scripts @ Value::Object(_)) => scripts.clone(),
        Some(_) => return Err("scripts must be an object".into()),
    };
    Ok(EmdashConfig {
        preserve_patterns,
        shell_setup: optional_string(&value, "shellSetup", "")?,
        prepare: optional_string(&scripts, "prepare", "scripts.")?,
        setup: optional_string(&scripts, "setup", "scripts.")?,
        run: optional_string(&scripts, "run", "scripts.")?,
        teardown: optional_string(&scripts, "teardown", "scripts.")?,
    })
}

pub fn import(text: &str, _: Reader) -> Result<Imported, String> {
    let config = parse(text)?;
    let commands = |script: &Option<String>| {
        script
            .as_deref()
            .map(script_commands)
            .unwrap_or_default()
            .into_iter()
            .map(|c| rename_variables(&c, VARIABLES))
            .collect::<Vec<_>>()
    };
    let mut setup = commands(&config.prepare);
    setup.extend(commands(&config.setup));
    let run = commands(&config.run);
    let mut notes = Vec::new();
    if config.shell_setup.is_some() {
        notes.push(
            "shellSetup is not imported: Branchyard has no per-shell setup for the harness yet"
                .to_owned(),
        );
    }
    Ok(Imported {
        file: FILE,
        copy: config.preserve_patterns.unwrap_or_default(),
        setup,
        run: (!run.is_empty()).then(|| run.join(" && ")),
        teardown: commands(&config.teardown),
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn emdash_config_maps_to_a_workspace() {
        let imported = import(
            r#"{
              "preservePatterns": [".env", ".env.local", ".emdash.json"],
              "shellSetup": "nvm use",
              "excludePatterns": ["retired"],
              "scripts": {
                "prepare": "corepack enable",
                "setup": "pnpm install\npnpm db:migrate",
                "run": "PORT=$EMDASH_PORT pnpm dev",
                "teardown": "docker compose -p \"$EMDASH_TASK_ID\" down"
              }
            }"#,
            &none,
        )
        .unwrap();
        assert_eq!(imported.copy, [".env", ".env.local"]);
        assert_eq!(
            imported.setup,
            ["corepack enable", "pnpm install", "pnpm db:migrate"]
        );
        assert_eq!(
            imported.run.as_deref(),
            Some("PORT=$BRANCHYARD_PORT pnpm dev")
        );
        // No Branchyard equivalent of a task ID: left as written.
        assert_eq!(
            imported.teardown,
            ["docker compose -p \"$EMDASH_TASK_ID\" down"]
        );
        assert_eq!(imported.notes.len(), 1);
        assert!(import("{}", &none).unwrap().is_empty());
        assert!(import(r#"{"scripts": {"setup": 1}}"#, &none)
            .unwrap_err()
            .contains("scripts.setup must be a string"));
        assert!(import("[1]", &none).is_err());
        assert!(import("{", &none).is_err());
    }

    /// The vendored source still has the shape this port reads.
    #[test]
    fn the_port_follows_the_vendored_schema() {
        let source = include_str!(
            "../../../../vendor/emdash/packages/core/src/primitives/emdash-config/api/emdash-config.ts"
        );
        for field in [
            "preservePatterns: preservePatternsSchema.optional()",
            "shellSetup: z.string().optional()",
            "prepare: z.string().optional()",
            "setup: z.string().optional()",
            "run: z.string().optional()",
            "teardown: z.string().optional()",
            "export const EMDASH_CONFIG_FILE = '.emdash.json'",
        ] {
            assert!(
                source.contains(field),
                "{field} is gone from emdash's schema"
            );
        }
    }
}
