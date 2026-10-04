//! The plugin's short-lived commands: Herdr actions, the branch pane's
//! command and the send popup. Each runs `by` with the configured
//! `--remote` settings in its environment, so its output is `by`'s own.

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::bridge::PaneMap;
use crate::config::Config;
use crate::herdr::Herdr;

/// `by <args>` with the remote settings in its environment.
fn by(config: &Config, args: &[&str]) -> Command {
    let mut command = Command::new(config.by());
    command.args(args).envs(config.remote_env());
    command
}

/// The branch an action is for: `BRANCHYARD_HERDR_BRANCH` when set, else the one
/// the bridge shows in the focused pane (from Herdr's invocation context,
/// else `HERDR_PANE_ID`).
pub fn target_branch(config: &Config) -> Result<String, String> {
    if let Some(branch) = config.get("BRANCHYARD_HERDR_BRANCH") {
        return Ok(branch.to_owned());
    }
    let focused = config
        .get("HERDR_PLUGIN_CONTEXT_JSON")
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|context| context["focused_pane_id"].as_str().map(str::to_owned))
        .or_else(|| config.get("HERDR_PANE_ID").map(str::to_owned))
        .ok_or("no focused pane: run this from a Branchyard branch pane")?;
    PaneMap::load(config)
        .branch_of(&focused)
        .map(str::to_owned)
        .ok_or_else(|| format!("pane {focused} does not show a Branchyard branch"))
}

pub fn action(config: &Config, name: &str) -> Result<(), String> {
    let herdr = Herdr::new(config.herdr());
    let branch = match target_branch(config) {
        Ok(branch) => branch,
        Err(message) => {
            herdr.notify("Branchyard", &message);
            return Err(message);
        }
    };
    match name {
        "merge" | "cancel" => {
            let output = by(config, &[name, &branch])
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("cannot run {}: {e}", config.by().display()))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            print!("{stdout}");
            eprint!("{stderr}");
            let body = crate::model::one_line(
                match output.status.success() {
                    true => &stdout,
                    false => &stderr,
                },
                200,
            );
            let title = match (name, output.status.success()) {
                ("merge", true) => format!("Merged {branch}"),
                ("cancel", true) => format!("Cancelled {branch}"),
                _ => format!("{name} {branch} failed"),
            };
            herdr.notify(&title, &body);
            match output.status.success() {
                true => Ok(()),
                false => Err(format!("by {name} {branch} failed")),
            }
        }
        "send" => {
            let mut args: Vec<String> = [
                "plugin",
                "pane",
                "open",
                "--plugin",
                config.plugin_id(),
                "--entrypoint",
                "send",
                "--placement",
                "popup",
                "--env",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
            args.push(format!("BRANCHYARD_HERDR_BRANCH={branch}"));
            for (key, value) in config.remote_env() {
                args.push("--env".into());
                args.push(format!("{key}={value}"));
            }
            herdr.call(&args).map(|_| ()).map_err(|e| e.to_string())
        }
        other => Err(format!(
            "unknown action {other}; expected merge, cancel or send"
        )),
    }
}

/// The send popup: read one prompt line, run `by send`, and wait for Enter
/// so the result can be read. Ctrl-C stops watching; the turn keeps
/// running on the server and its branch pane shows it.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-herdr
pub fn send_popup(config: &Config) -> Result<(), String> {
    let branch = config
        .get("BRANCHYARD_HERDR_BRANCH")
        .ok_or("BRANCHYARD_HERDR_BRANCH is not set")?
        .to_owned();
    print!("Send to {branch} (empty to cancel): ");
    let _ = std::io::stdout().flush();
    let mut prompt = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut prompt)
        .map_err(|e| e.to_string())?;
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Ok(());
    }
    let extra: Vec<&str> = config
        .get("BRANCHYARD_HERDR_SEND_ARGS")
        .map(|args| args.split_whitespace().collect())
        .unwrap_or_default();
    let mut args = vec!["send", &branch, prompt];
    args.extend(extra);
    let status = by(config, &args)
        .status()
        .map_err(|e| format!("cannot run {}: {e}", config.by().display()))?;
    print!("\nby send exited with {status}; press Enter to close ");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().lock().read_line(&mut String::new());
    match status.success() {
        true => Ok(()),
        false => Err(format!("by send {branch} failed")),
    }
}

/// A branch pane: `by log --follow` for `BRANCHYARD_HERDR_BRANCH`. When it ends
/// (the branch was removed, say), the pane waits for Enter so the reason
/// stays visible.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-herdr
pub fn log(config: &Config) -> Result<(), String> {
    let branch = config
        .get("BRANCHYARD_HERDR_BRANCH")
        .ok_or("BRANCHYARD_HERDR_BRANCH is not set")?
        .to_owned();
    let status = by(config, &["log", "--follow", &branch])
        .status()
        .map_err(|e| format!("cannot run {}: {e}", config.by().display()))?;
    print!("\nby log exited with {status}; press Enter to close ");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().lock().read_line(&mut String::new());
    Ok(())
}

/// Open the bridge in a new, focused Herdr tab.
pub fn start(config: &Config) -> Result<(), String> {
    let herdr = Herdr::new(config.herdr());
    let mut args: Vec<String> = [
        "plugin",
        "pane",
        "open",
        "--plugin",
        config.plugin_id(),
        "--entrypoint",
        "bridge",
        "--placement",
        "tab",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    for (key, value) in config.remote_env() {
        args.push("--env".into());
        args.push(format!("{key}={value}"));
    }
    herdr.call(&args).map(|_| ()).map_err(|e| e.to_string())
}
