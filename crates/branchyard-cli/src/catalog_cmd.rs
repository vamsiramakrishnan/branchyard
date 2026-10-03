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
    // What `by catalog refresh` cached, verified; a cache that does not
    // verify is refused, and the pinned baseline stands alone.
    let live = match crate::live_catalog::load(&crate::live_catalog::dir()) {
        Ok(loaded) => loaded.and_then(|l| l.connectors).unwrap_or_default(),
        Err(why) => {
            eprintln!("by: the live connector catalog was refused: {why}");
            Vec::new()
        }
    };
    if as_json {
        let mut value = serde_json::to_value(entries).expect("catalog entries");
        if let Value::Array(list) = &mut value {
            for entry in &live {
                let mut item = serde_json::to_value(entry).expect("a live entry");
                item["live"] = json!(true);
                list.push(item);
            }
        }
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
    let mut cells = cells;
    cells.extend(live.iter().map(|e| {
        vec![
            Cell::toned(&e.id, Tone::Cyan),
            Cell::plain(&e.kind),
            Cell::plain(&e.auth),
            Cell::plain(match e.credentials.is_empty() {
                true => "-".to_owned(),
                false => e.credentials.join(", "),
            }),
            Cell::plain(match &e.same_as {
                Some(same) => format!("(= {same}) {}", e.description),
                None => e.description.clone(),
            }),
        ]
    }));
    let mut text = table(&columns, &cells, env.style());
    if !live.is_empty() {
        text.push_str(&format!(
            "\n{} more from the MCP registry, pinned at the version `by catalog refresh` read.",
            live.len()
        ));
    }
    text.push_str(&format!(
        "\n{} connectors, from emdash's MCP catalog (catalog/connectors.toml): the starting list \
         Anvil adopts connectors from. None is granted to a branch by being listed; see \
         docs/connectors.md.\n",
        entries.len()
    ));
    print(&text)
}

/// `by catalog refresh|status`.
pub fn live(action: &crate::args::CatalogAction, as_json: bool) -> Outcome {
    use crate::args::CatalogAction;
    use crate::commands::Failure;
    use crate::live_catalog::{self, Sources};
    let dir = live_catalog::dir();
    match action {
        CatalogAction::Refresh {
            mcp_registry,
            npm_registry,
            max_pages,
            only,
        } => {
            let done = live_catalog::refresh(
                &dir,
                &Sources {
                    mcp_registry,
                    npm_registry,
                    max_pages: *max_pages,
                    connectors: only.as_deref() != Some("harnesses"),
                    harnesses: only.as_deref() != Some("connectors"),
                },
            )
            .map_err(Failure::Message)?;
            if as_json {
                return print(&json::text(
                    &serde_json::to_value(&done).expect("a summary"),
                ));
            }
            let mut out = String::new();
            if let Some(n) = done.connectors {
                out.push_str(&format!("connectors  {n} from {mcp_registry}\n"));
            }
            if let Some(n) = done.harnesses {
                out.push_str(&format!(
                    "harnesses   {n} latest releases from {npm_registry}\n"
                ));
            }
            out.push_str(&format!(
                "requests    {} ({} not modified)\ncached in   {}\n",
                done.requests,
                done.not_modified,
                done.dir.display()
            ));
            print(&out)
        }
        CatalogAction::Status => {
            let loaded = live_catalog::load(&dir).map_err(Failure::Message)?;
            let Some(loaded) = loaded else {
                return match as_json {
                    true => print(&json::text(&json!({"dir": dir, "cached": false}))),
                    false => print(&format!(
                        "nothing cached in {}; the pinned catalogs stand alone (by catalog \
                         refresh)\n",
                        dir.display()
                    )),
                };
            };
            if as_json {
                return print(&json::text(&json!({
                    "dir": dir,
                    "cached": true,
                    "verified": true,
                    "manifest": loaded.manifest,
                    "connectors": loaded.connectors,
                    "harnesses": loaded.harnesses,
                })));
            }
            let mut out = format!("cache       {} (checksums verified)\n", dir.display());
            for (what, part) in [
                ("connectors", &loaded.manifest.connectors),
                ("harnesses", &loaded.manifest.harnesses),
            ] {
                if let Some(part) = part {
                    out.push_str(&format!(
                        "{what:<11} {} from {} (sha256 {})\n",
                        part.count,
                        part.registry,
                        &part.sha256[..16]
                    ));
                }
            }
            for h in loaded.harnesses.iter().flatten() {
                out.push_str(&format!("  {} {}@{}\n", h.id, h.package, h.latest));
            }
            print(&out)
        }
    }
}
