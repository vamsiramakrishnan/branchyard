//! `by harnesses --all` and `by connectors catalog`: what Branchyard knows
//! about harness CLIs and connectors from the generated catalogs in
//! `branchyard_controls::catalog`, beside what it can drive.

use serde_json::{json, Value};

use branchyard_controls::catalog;

use crate::commands::{print, Env, Outcome, Target};
use crate::json;
use crate::render::{table, Cell, Column, Tone};

/// One catalog harness and what Branchyard does with it.
struct Row {
    entry: &'static catalog::HarnessEntry,
    /// Profiles that drive it, the default first.
    profiles: Vec<&'static str>,
    /// One of its executables is on this machine's `PATH`; `None` remotely,
    /// where the server's machine is the one that matters.
    installed: Option<bool>,
}

fn rows(target: &Target) -> Vec<Row> {
    catalog::harnesses()
        .iter()
        .map(|entry| {
            let profiles = branchyard_harness::profiles::PROFILES
                .iter()
                .filter(|p| p.harness == entry.id)
                .map(|p| p.id)
                .collect();
            let installed = match target {
                Target::Local => Some(
                    entry
                        .binaries
                        .iter()
                        .any(|b| crate::setup_io::which(b).is_some()),
                ),
                Target::Remote(_) => None,
            };
            Row {
                entry,
                profiles,
                installed,
            }
        })
        .collect()
}

/// `by harnesses --all`.
pub fn harnesses(env: &Env, target: &Target, as_json: bool) -> Outcome {
    let rows = rows(target);
    if as_json {
        let list: Vec<Value> = rows
            .iter()
            .map(|row| {
                let mut value = serde_json::to_value(row.entry).expect("a catalog entry");
                value["drivable"] = json!(!row.profiles.is_empty());
                value["profiles"] = json!(row.profiles);
                value["installed"] = json!(row.installed);
                value
            })
            .collect();
        return print(&json::text(&Value::Array(list)));
    }
    let columns = [
        Column {
            header: "HARNESS",
            max: 20,
            right: false,
        },
        Column {
            header: "BRANCHYARD",
            max: 24,
            right: false,
        },
        Column {
            header: "ON PATH",
            max: 7,
            right: false,
        },
        Column {
            header: "INSTALL",
            max: 56,
            right: false,
        },
        Column {
            header: "LOGIN",
            max: 28,
            right: false,
        },
        Column {
            header: "API KEY",
            max: 28,
            right: false,
        },
    ];
    let cells: Vec<Vec<Cell>> = rows
        .iter()
        .map(|row| {
            let e = row.entry;
            vec![
                Cell::plain(&e.id),
                match row.profiles.first() {
                    Some(profile) => Cell::toned(*profile, Tone::Green),
                    None => Cell::toned("knows about", Tone::Dim),
                },
                match row.installed {
                    Some(true) => Cell::toned("yes", Tone::Green),
                    Some(false) => Cell::toned("no", Tone::Dim),
                    None => Cell::toned("-", Tone::Dim),
                },
                Cell::plain(e.install.first().map(String::as_str).unwrap_or("-")),
                Cell::plain(e.login.as_deref().unwrap_or("-")),
                Cell::plain(match e.auth_env.is_empty() {
                    true => "-".to_owned(),
                    false => e.auth_env.join(", "),
                }),
            ]
        })
        .collect();
    let drivable = rows.iter().filter(|r| !r.profiles.is_empty()).count();
    let mut text = table(&columns, &cells, env.style());
    text.push_str(&format!(
        "\n{} harnesses: Branchyard drives {drivable} (a profile in the second column) and \
         knows the rest from emdash's and Orca's registries; `by harnesses --all --json` has \
         every install command, models and flags. See docs/compatibility.md.\n",
        rows.len()
    ));
    print(&text)
}

/// `by connectors catalog`.
pub fn connectors(env: &Env, as_json: bool) -> Outcome {
    let entries = catalog::connectors();
    if as_json {
        let value = serde_json::to_value(entries).expect("catalog entries");
        return print(&json::text(&value));
    }
    let columns = [
        Column {
            header: "CONNECTOR",
            max: 20,
            right: false,
        },
        Column {
            header: "KIND",
            max: 13,
            right: false,
        },
        Column {
            header: "AUTH",
            max: 6,
            right: false,
        },
        Column {
            header: "CREDENTIALS",
            max: 28,
            right: false,
        },
        Column {
            header: "DESCRIPTION",
            max: 60,
            right: false,
        },
    ];
    let cells: Vec<Vec<Cell>> = entries
        .iter()
        .map(|e| {
            let credentials: Vec<String> = e
                .credentials
                .iter()
                .map(|c| match c.required {
                    true => c.name.clone(),
                    false => format!("{} (optional)", c.name),
                })
                .collect();
            vec![
                Cell::plain(&e.id),
                Cell::plain(&e.kind),
                Cell::plain(&e.auth),
                Cell::plain(match credentials.is_empty() {
                    true => "-".to_owned(),
                    false => credentials.join(", "),
                }),
                Cell::plain(&e.description),
            ]
        })
        .collect();
    let mut text = table(&columns, &cells, env.style());
    text.push_str(&format!(
        "\n{} connectors, from emdash's MCP catalog (catalog/connectors.toml): the starting list \
         Anvil adopts connectors from. None is granted to a branch by being listed; see \
         docs/connectors.md.\n",
        entries.len()
    ));
    print(&text)
}
