//! `.conductor/settings.toml`, written from Conductor's public
//! documentation of scripts (conductor.build/docs/reference/scripts);
//! Conductor is closed source and nothing of it is copied here.
//!
//! As documented, `[scripts]` has `setup`, `archive` (run before a
//! workspace is archived: Branchyard's teardown), `run_mode`, and `run`:
//! one command, or named `[scripts.run.NAME]` tables with `command`,
//! optional `args`, `options.cwd`, `default`, `icon`, `hide` and
//! `available_in`. Scripts see `CONDUCTOR_ROOT_PATH`,
//! `CONDUCTOR_WORKSPACE_PATH`, `CONDUCTOR_WORKSPACE_NAME` and
//! `CONDUCTOR_PORT`, renamed here to Branchyard's variables.

use super::{in_directory, quote_word, rename_variables, script_commands, Imported, Reader};

pub const FILE: &str = ".conductor/settings.toml";

const VARIABLES: &[(&str, &str)] = &[
    ("CONDUCTOR_ROOT_PATH", "BRANCHYARD_ROOT"),
    ("CONDUCTOR_WORKSPACE_PATH", "BRANCHYARD_WORKTREE"),
    ("CONDUCTOR_WORKSPACE_NAME", "BRANCHYARD_BRANCH"),
    ("CONDUCTOR_PORT", "BRANCHYARD_PORT"),
];

fn string(table: &toml::Table, key: &str, at: &str) -> Result<Option<String>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("{at}{key} must be a string")),
    }
}

pub fn import(text: &str, _: Reader) -> Result<Imported, String> {
    let settings: toml::Table = toml::from_str(text).map_err(|e| e.message().to_owned())?;
    let Some(scripts) = settings.get("scripts") else {
        return Ok(Imported {
            file: FILE,
            ..Imported::default()
        });
    };
    let scripts = scripts.as_table().ok_or("scripts must be a table")?;
    let commands = |script: Option<String>| -> Vec<String> {
        script
            .as_deref()
            .map(script_commands)
            .unwrap_or_default()
            .into_iter()
            .map(|c| rename_variables(&c, VARIABLES))
            .collect()
    };
    let mut notes = Vec::new();
    let run = match scripts.get("run") {
        None => None,
        Some(toml::Value::String(command)) => Some(command.clone()),
        Some(toml::Value::Table(named)) => {
            // The default script, else the first by name.
            let mut entries: Vec<(&String, &toml::Table)> = Vec::new();
            for (name, entry) in named {
                let entry = entry
                    .as_table()
                    .ok_or(format!("scripts.run.{name} must be a table"))?;
                entries.push((name, entry));
            }
            let chosen = entries
                .iter()
                .find(|(_, e)| e.get("default").and_then(toml::Value::as_bool) == Some(true))
                .or(entries.first())
                .copied();
            match chosen {
                None => None,
                Some((name, entry)) => {
                    let at = format!("scripts.run.{name}.");
                    let command =
                        string(entry, "command", &at)?.ok_or(format!("{at}command is required"))?;
                    let mut line = command;
                    for arg in entry
                        .get("args")
                        .and_then(toml::Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let arg = arg.as_str().ok_or(format!("{at}args must be strings"))?;
                        line.push(' ');
                        line.push_str(&quote_word(arg));
                    }
                    let cwd = entry
                        .get("options")
                        .and_then(toml::Value::as_table)
                        .map(|o| string(o, "cwd", &format!("{at}options.")))
                        .transpose()?
                        .flatten();
                    let others: Vec<&str> = entries
                        .iter()
                        .filter(|(n, _)| *n != name)
                        .map(|(n, _)| n.as_str())
                        .collect();
                    if !others.is_empty() {
                        notes.push(format!(
                            "run script {name} became the run script; not imported: {}",
                            others.join(", ")
                        ));
                    }
                    Some(in_directory(cwd.as_deref(), line))
                }
            }
        }
        Some(_) => return Err("scripts.run must be a command or a table of scripts".into()),
    };
    if scripts.get("run_mode").is_some() {
        notes.push("run_mode is not imported: by workspace run runs one script".to_owned());
    }
    Ok(Imported {
        file: FILE,
        copy: Vec::new(),
        setup: commands(string(scripts, "setup", "scripts.")?),
        run: run.map(|c| rename_variables(&c, VARIABLES)),
        teardown: commands(string(scripts, "archive", "scripts.")?),
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
    fn conductor_settings_map_to_a_workspace() {
        let one = import(
            "[scripts]\nsetup = \"pnpm install\"\nrun = \"pnpm dev --port $CONDUCTOR_PORT\"\n\
             archive = \"./script/archive.sh\"\nrun_mode = \"concurrent\"\n",
            &none,
        )
        .unwrap();
        assert_eq!(one.setup, ["pnpm install"]);
        assert_eq!(one.run.as_deref(), Some("pnpm dev --port $BRANCHYARD_PORT"));
        assert_eq!(one.teardown, ["./script/archive.sh"]);
        assert_eq!(one.notes.len(), 1);
        let named = import(
            "[scripts]\nsetup = \"cp $CONDUCTOR_ROOT_PATH/.env .env\"\n\
             [scripts.run.web]\ncommand = \"pnpm dev\"\nargs = [\"--port\", \"$CONDUCTOR_PORT\"]\n\
             options = { cwd = \"apps/web\" }\ndefault = true\navailable_in = [\"local\"]\n\
             [scripts.run.worker]\ncommand = \"pnpm worker\"\n",
            &none,
        )
        .unwrap();
        assert_eq!(named.setup, ["cp $BRANCHYARD_ROOT/.env .env"]);
        assert_eq!(
            named.run.as_deref(),
            Some("cd apps/web && pnpm dev --port '$BRANCHYARD_PORT'")
        );
        assert!(
            named.notes[0].ends_with("not imported: worker"),
            "{:?}",
            named.notes
        );
        assert!(import("[scripts]\nsetup = 1\n", &none).is_err());
        assert!(import("[scripts.run.web]\nargs = []\n", &none).is_err());
        assert!(import("not toml =", &none).is_err());
        assert!(import("[other]\n", &none).unwrap().is_empty());
    }
}
