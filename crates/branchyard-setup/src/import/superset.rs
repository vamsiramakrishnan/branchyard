//! `.superset/config.json`, written from Superset's public documentation
//! of project lifecycle scripts (docs.superset.sh/setup-teardown-scripts),
//! not from its source: Superset is under the Elastic License 2.0, and no
//! code, schema or text of it is copied here.
//!
//! As documented: `setup`, `teardown` and `run` each hold a list of
//! commands, run in order in the workspace; `cwd` runs them in another
//! directory of the worktree; a key with no commands falls back to the
//! script `.superset/<key>.sh`. Scripts see `SUPERSET_ROOT_PATH`,
//! `SUPERSET_WORKSPACE_NAME` and `SUPERSET_WORKSPACE_PATH`, renamed here to
//! Branchyard's variables. The per-user override under `~/.superset` and
//! `.superset/config.local.json` are personal and not committed, so they
//! are not imported.

use serde_json::Value;

use super::{in_directory, rename_variables, Imported, Reader};

pub const FILE: &str = ".superset/config.json";

const VARIABLES: &[(&str, &str)] = &[
    ("SUPERSET_ROOT_PATH", "BRANCHYARD_ROOT"),
    ("SUPERSET_WORKSPACE_PATH", "BRANCHYARD_WORKTREE"),
    ("SUPERSET_WORKSPACE_NAME", "BRANCHYARD_BRANCH"),
];

/// The commands of `key`: a list (a lone string is taken too), else its
/// fallback script when the repository has it.
fn commands(config: &Value, key: &str, read: Reader) -> Result<Vec<String>, String> {
    let listed = match config.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(one)) => Some(vec![one.clone()]),
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|i| i.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or(format!("{key} must be a list of commands"))?,
        ),
        Some(_) => return Err(format!("{key} must be a list of commands")),
    };
    let mut listed: Vec<String> = listed
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.trim().to_owned())
        .filter(|c| !c.is_empty())
        .collect();
    if listed.is_empty() && config.get(key).is_none() {
        let script = format!(".superset/{key}.sh");
        if read(&script).is_some() {
            listed.push(format!("sh {script}"));
        }
    }
    Ok(listed)
}

pub fn import(text: &str, read: Reader) -> Result<Imported, String> {
    let config: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if !config.is_object() {
        return Err("must be a JSON object".into());
    }
    let cwd = match config.get("cwd") {
        None | Some(Value::Null) => None,
        Some(Value::String(cwd)) => Some(cwd.as_str()),
        Some(_) => return Err("cwd must be a string".into()),
    };
    let translate = |list: Vec<String>| -> Vec<String> {
        list.into_iter()
            .map(|c| in_directory(cwd, rename_variables(&c, VARIABLES)))
            .collect()
    };
    let run = translate(commands(&config, "run", read)?);
    Ok(Imported {
        file: FILE,
        copy: Vec::new(),
        setup: translate(commands(&config, "setup", read)?),
        run: (!run.is_empty()).then(|| run.join(" && ")),
        teardown: translate(commands(&config, "teardown", read)?),
        notes: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superset_config_maps_to_a_workspace() {
        let none = |_: &str| None;
        let imported = import(
            r#"{"setup": ["bun install", "cp \"$SUPERSET_ROOT_PATH/.env\" .env"],
                "teardown": ["docker-compose down"], "run": ["bun dev"], "cwd": "apps/web"}"#,
            &none,
        )
        .unwrap();
        assert_eq!(
            imported.setup,
            [
                "cd apps/web && bun install",
                "cd apps/web && cp \"$BRANCHYARD_ROOT/.env\" .env"
            ]
        );
        assert_eq!(imported.teardown, ["cd apps/web && docker-compose down"]);
        assert_eq!(imported.run.as_deref(), Some("cd apps/web && bun dev"));
        // A key with no commands falls back to its script; an empty list
        // means none.
        let scripts = |path: &str| {
            (path == ".superset/setup.sh" || path == ".superset/run.sh").then(String::new)
        };
        let imported = import(r#"{"run": []}"#, &scripts).unwrap();
        assert_eq!(imported.setup, ["sh .superset/setup.sh"]);
        assert_eq!(imported.run, None);
        assert!(import(r#"{"setup": [1]}"#, &none).is_err());
        assert!(import(r#"{"setup": {"before": ["x"]}}"#, &none).is_err());
    }
}
