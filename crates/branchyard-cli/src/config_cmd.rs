//! `by config show|path|validate|schema`: the configuration `by` reads,
//! where it is, whether it loads, and its JSON Schema.

use std::path::Path;

use serde_json::{json, Value};

use crate::commands::{print, Failure, Outcome};
use crate::setup_io::{self, locate};

pub const USAGE: &str = "\
Show, locate or validate Branchyard's configuration.

Usage: by config show [--json]              Every effective value and where it came from
       by config path [--json]              The user and project files, and whether they exist
       by config validate [FILE] [--json]   Load FILE, or every file by reads, strictly
       by config schema                     The JSON Schema of the file (schema/branchyard.config.json)

Files: ~/.config/branchyard/config.toml (BRANCHYARD_USER_CONFIG overrides the
path), then branchyard.toml at or above the current directory, up to the
repository root. The project file overrides the user file key by key;
BRANCHYARD_REMOTE, BRANCHYARD_TOKEN_FILE, BRANCHYARD_REPO and
BRANCHYARD_CA_FILE override both; flags override everything. Neither file is
read inside a harness running on a branch (BRANCHYARD_BRANCH set).
Write one with `by init project`.
";

fn usage(message: &str) -> Outcome {
    eprintln!("by: {message}\nTry 'by config --help'.");
    std::process::exit(2);
}

pub fn main(args: &[String]) -> Outcome {
    let mut json = false;
    let mut positionals = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return print(USAGE),
            "--json" => json = true,
            other if other.starts_with('-') => return usage(&format!("unknown option {other}")),
            other => positionals.push(other.to_owned()),
        }
    }
    let cwd = std::env::current_dir()?;
    match positionals.first().map(String::as_str) {
        None => print(USAGE),
        Some("show") if positionals.len() == 1 => show(&cwd, json),
        Some("path") if positionals.len() == 1 => path(&cwd, json),
        Some("validate") if positionals.len() <= 2 => {
            validate(&cwd, positionals.get(1).map(String::as_str), json)
        }
        Some("schema") if positionals.len() == 1 => print(&branchyard_setup::schema::config_json()),
        Some(action @ ("show" | "path" | "validate" | "schema")) => {
            usage(&format!("too many arguments to config {action}"))
        }
        Some(other) => usage(&format!(
            "unknown config action '{other}'; use show, path, validate or schema"
        )),
    }
}

fn to_json(value: &Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(value).unwrap_or_default()
    )
}

fn show(cwd: &Path, json: bool) -> Outcome {
    let env = |name: &str| std::env::var(name).ok();
    let effective = setup_io::load(cwd, Some(&env)).map_err(|e| Failure::Message(e.to_string()))?;
    let flat = effective.config.flatten();
    if json {
        let values: serde_json::Map<String, Value> = flat
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    json!({ "value": value, "source": effective.sources.get(key) }),
                )
            })
            .collect();
        return print(&to_json(&json!({ "values": values })));
    }
    if flat.is_empty() {
        return print(
            "No configuration: every default is by's own. Write one with `by init project`.\n",
        );
    }
    let width = flat.keys().map(|k| k.len()).max().unwrap_or(0);
    let mut text = String::new();
    for (key, value) in &flat {
        let shown = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => match n.as_f64() {
                Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
                _ => n.to_string(),
            },
            other => other.to_string(),
        };
        let source = effective
            .sources
            .get(key)
            .map(|s| s.to_string())
            .unwrap_or_default();
        text.push_str(&format!("{key:<width$}  {shown}  ({source})\n"));
    }
    print(&text)
}

fn path(cwd: &Path, json: bool) -> Outcome {
    let located = locate(cwd);
    let user_exists = located.user.is_file();
    if json {
        return print(&to_json(&json!({
            "user": { "path": located.user, "exists": user_exists },
            "project": { "path": located.project, "exists": located.project_exists },
        })));
    }
    let mark = |exists: bool| match exists {
        true => "",
        false => " (not found)",
    };
    print(&format!(
        "user     {}{}\nproject  {}{}\n",
        located.user.display(),
        mark(user_exists),
        located.project.display(),
        mark(located.project_exists)
    ))
}

fn validate(cwd: &Path, file: Option<&str>, json: bool) -> Outcome {
    let files: Vec<std::path::PathBuf> = match file {
        Some(file) => vec![file.into()],
        None => {
            let located = locate(cwd);
            let mut files = Vec::new();
            if located.user.is_file() {
                files.push(located.user);
            }
            if located.project_exists {
                files.push(located.project);
            }
            files
        }
    };
    let mut results = Vec::new();
    let mut ok = true;
    for path in &files {
        let result = setup_io::read_layer(path);
        ok &= result.is_ok();
        results.push((path.clone(), result.err().map(|e| e.to_string())));
    }
    // The layers must also merge.
    if ok && file.is_none() {
        if let Err(e) = setup_io::load(cwd, None) {
            ok = false;
            results.push((cwd.to_path_buf(), Some(e.to_string())));
        }
    }
    if json {
        let entries: Vec<Value> = results
            .iter()
            .map(|(path, error)| json!({ "path": path, "ok": error.is_none(), "error": error }))
            .collect();
        print(&to_json(&json!({ "valid": ok, "files": entries })))?;
    } else if files.is_empty() {
        print("No configuration files to validate. Write one with `by init project`.\n")?;
    } else {
        let mut text = String::new();
        for (path, error) in &results {
            match error {
                None => text.push_str(&format!("ok       {}\n", path.display())),
                Some(error) => text.push_str(&format!("invalid  {error}\n")),
            }
        }
        print(&text)?;
    }
    match ok {
        true => Ok(()),
        false => Err(Failure::Reported),
    }
}
