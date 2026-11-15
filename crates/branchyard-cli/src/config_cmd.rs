//! `by config show|path|validate|schema`: the configuration `by` reads,
//! where it is, whether it loads, and its JSON Schema.

use std::path::Path;

use serde_json::{json, Value};

use crate::args::ConfigAction;
use crate::commands::{print, Failure, Outcome};
use crate::setup_io::{self, locate};

/// `by config ACTION`; the files it reads are in `by config --help`.
pub fn main(action: &ConfigAction, json: bool) -> Outcome {
    let cwd = std::env::current_dir()?;
    match action {
        ConfigAction::Show => show(&cwd, json),
        ConfigAction::Path => path(&cwd, json),
        ConfigAction::Validate { file } => validate(&cwd, file.as_deref(), json),
        // The schema is JSON either way.
        ConfigAction::Schema => print(&branchyard_setup::schema::config_json()),
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
    let effective =
        setup_io::load(cwd, Some(&env)).map_err(|e| crate::errors::config(&e.to_string()))?;
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
    // What the configuration says that is not wrong: the route notes of the
    // one file asked about, else of the merge, which is what `by` reads.
    let mut notes = Vec::new();
    for path in &files {
        match setup_io::read_layer(path) {
            Ok(layer) => {
                if file.is_some() {
                    notes = layer.models.route_notes();
                }
                results.push((path.clone(), None));
            }
            Err(e) => {
                ok = false;
                results.push((path.clone(), Some(e.to_string())));
            }
        }
    }
    // The layers must also merge.
    if ok && file.is_none() {
        match setup_io::load(cwd, None) {
            Ok(effective) => notes = effective.config.models.route_notes(),
            Err(e) => {
                ok = false;
                results.push((cwd.to_path_buf(), Some(e.to_string())));
            }
        }
    }
    if !ok {
        notes.clear();
    }
    if json {
        let entries: Vec<Value> = results
            .iter()
            .map(|(path, error)| json!({ "path": path, "ok": error.is_none(), "error": error }))
            .collect();
        print(&to_json(
            &json!({ "valid": ok, "files": entries, "notes": notes }),
        ))?;
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
        // Notes, not refusals: the exit status is the files'.
        for note in &notes {
            text.push_str(&format!("note     {note}\n"));
        }
        print(&text)?;
    }
    match ok {
        true => Ok(()),
        false => Err(Failure::Reported),
    }
}
