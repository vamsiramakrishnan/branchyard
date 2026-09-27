//! `plugins/herdr/herdr-plugin.toml` against the rules Herdr's manifest
//! loader applies (herdr fff6c82, `src/app/api/plugins/manifest.rs`), and
//! against what this binary expects of it.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item};

fn plugin_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/herdr")
}

fn manifest() -> DocumentMut {
    std::fs::read_to_string(plugin_dir().join("herdr-plugin.toml"))
        .unwrap()
        .parse()
        .unwrap()
}

fn str_of<'a>(item: &'a Item, key: &str) -> &'a str {
    item.get(key)
        .and_then(Item::as_str)
        .unwrap_or_else(|| panic!("{key} is missing or not a string"))
}

fn command(item: &Item) -> Vec<String> {
    let command: Vec<String> = item["command"]
        .as_array()
        .expect("command is an argv array")
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert!(!command.is_empty() && command.iter().all(|a| !a.is_empty()));
    command
}

/// Herdr's local ids: ASCII letters, digits, colon, underscore, hyphen.
fn local_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'))
}

fn entries<'a>(doc: &'a DocumentMut, key: &str) -> Vec<&'a toml_edit::Table> {
    doc.get(key)
        .and_then(Item::as_array_of_tables)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

#[test]
fn the_manifest_is_one_herdr_loads() {
    let doc = manifest();
    let root = doc.as_item();
    let id = str_of(root, "id");
    assert!(id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '_' | '-')));
    assert_eq!(id, "branchyard", "the binary's default plugin id");
    for key in ["name", "version", "min_herdr_version"] {
        assert!(!str_of(root, key).trim().is_empty());
    }
    let platforms: Vec<&str> = root["platforms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(platforms
        .iter()
        .all(|p| ["linux", "macos", "windows"].contains(p)));

    let mut actions = Vec::new();
    for action in entries(&doc, "actions") {
        let action_id = action["id"].as_str().unwrap();
        assert!(local_id(action_id), "{action_id}");
        assert!(!action["title"].as_str().unwrap().is_empty());
        for context in action["contexts"].as_array().unwrap() {
            assert!(["global", "workspace", "tab", "pane", "selection"]
                .contains(&context.as_str().unwrap()));
        }
        actions.push((action_id.to_owned(), command(&Item::Table(action.clone()))));
    }
    let mut panes = Vec::new();
    for pane in entries(&doc, "panes") {
        let pane_id = pane["id"].as_str().unwrap();
        assert!(local_id(pane_id), "{pane_id}");
        let placement = pane
            .get("placement")
            .and_then(Item::as_str)
            .unwrap_or("overlay");
        assert!(["overlay", "popup", "split", "tab", "zoomed"].contains(&placement));
        if placement != "popup" {
            assert!(pane.get("width").is_none() && pane.get("height").is_none());
        }
        panes.push((pane_id.to_owned(), command(&Item::Table(pane.clone()))));
    }

    // What the binary opens and what users bind keys to.
    let ids = |list: &[(String, Vec<String>)]| -> Vec<String> {
        let mut ids: Vec<String> = list.iter().map(|(id, _)| id.clone()).collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(&panes), ["bridge", "log", "send"]);
    assert_eq!(ids(&actions), ["cancel", "merge", "send", "start"]);

    // Every command is the launcher, relative to the plugin root as Herdr
    // resolves it, with a subcommand this binary has.
    let launcher = plugin_dir().join("bin/branchyard-herdr");
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&launcher).unwrap().permissions(),
    );
    assert!(mode & 0o111 != 0, "the launcher is executable");
    for (_, argv) in actions.iter().chain(&panes) {
        assert_eq!(argv[0], "./bin/branchyard-herdr");
        let sub: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        assert!(
            matches!(
                sub[..],
                ["bridge"]
                    | ["log"]
                    | ["send"]
                    | ["start"]
                    | ["action", "merge" | "cancel" | "send"]
            ),
            "{sub:?}"
        );
    }
}

#[test]
fn the_launcher_runs_the_binary() {
    let output = std::process::Command::new(plugin_dir().join("bin/branchyard-herdr"))
        .arg("--help")
        .env(
            "BRANCHYARD_HERDR_BIN",
            env!("CARGO_BIN_EXE_branchyard-herdr"),
        )
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("Usage: branchyard-herdr"), "{help}");
    for command in ["bridge", "start", "log", "action", "send"] {
        assert!(
            help.contains(&format!("\n  {command} ")),
            "{command}: {help}"
        );
    }
}
