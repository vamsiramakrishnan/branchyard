//! Calls to the Herdr CLI, the only plugin API Herdr has (`plugins.mdx`:
//! "The entire Herdr CLI is the plugin API"). Each call runs
//! `$HERDR_BIN_PATH <args>`; Herdr prints the JSON response on stdout, or
//! an error as JSON on stderr with a non-zero exit.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

/// The `--source` of every report; `custom:` marks a third-party
/// integration (`integrations.mdx`, "Integrate your own agent").
pub const SOURCE: &str = "custom:branchyard";
/// The `--agent` label shown for a branch pane.
pub const AGENT: &str = "branchyard";

#[derive(Debug)]
pub struct HerdrError {
    /// Herdr's error code, such as `pane_not_found`, when it gave one.
    pub code: Option<String>,
    pub message: String,
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub struct Herdr {
    bin: PathBuf,
}

/// A pane Herdr opened, and the tab it is the root of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub pane_id: String,
    pub tab_id: Option<String>,
}

impl Herdr {
    pub fn new(bin: PathBuf) -> Herdr {
        Herdr { bin }
    }

    /// Run `herdr <args>` and return its JSON response.
    #[allow(clippy::map_unwrap_or)] // ratchet: branchyard-herdr
    pub fn call(&self, args: &[String]) -> Result<Value, HerdrError> {
        let output = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| HerdrError {
                code: None,
                message: format!("cannot run {}: {e}", self.bin.display()),
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let parsed: Option<Value> = serde_json::from_str(stdout.trim()).ok();
        let error = parsed
            .as_ref()
            .and_then(|v| v.get("error").cloned())
            .or_else(|| {
                serde_json::from_str::<Value>(stderr.trim())
                    .ok()
                    .and_then(|v| v.get("error").cloned())
            });
        if output.status.success() && error.is_none() {
            return Ok(parsed.unwrap_or(Value::Null));
        }
        let code = error
            .as_ref()
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let message = error
            .as_ref()
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let text = format!("{} {}", stdout.trim(), stderr.trim());
                format!("herdr {} failed: {}", args.join(" "), text.trim())
            });
        Err(HerdrError { code, message })
    }

    /// `herdr plugin pane open` for one of the plugin's `[[panes]]`, in a
    /// new tab, without taking focus.
    pub fn open_tab(
        &self,
        plugin: &str,
        entrypoint: &str,
        workspace: Option<&str>,
        env: &[(String, String)],
    ) -> Result<Opened, HerdrError> {
        let mut args = strings(&[
            "plugin",
            "pane",
            "open",
            "--plugin",
            plugin,
            "--entrypoint",
            entrypoint,
            "--placement",
            "tab",
            "--no-focus",
        ]);
        if let Some(workspace) = workspace {
            args.extend(strings(&["--workspace", workspace]));
        }
        for (key, value) in env {
            args.push("--env".into());
            args.push(format!("{key}={value}"));
        }
        let response = self.call(&args)?;
        let pane = &response["result"]["plugin_pane"]["pane"];
        let pane_id = pane["pane_id"].as_str().ok_or_else(|| HerdrError {
            code: None,
            message: format!("herdr plugin pane open answered without a pane: {response}"),
        })?;
        Ok(Opened {
            pane_id: pane_id.to_owned(),
            tab_id: pane["tab_id"].as_str().map(str::to_owned),
        })
    }

    /// Whether Herdr still has the pane.
    pub fn pane_exists(&self, pane_id: &str) -> Result<bool, HerdrError> {
        match self.call(&strings(&["pane", "get", pane_id])) {
            Ok(_) => Ok(true),
            Err(e) if e.code.as_deref() == Some("pane_not_found") => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// `herdr pane report-agent` with this plugin's source and label.
    pub fn report(
        &self,
        pane_id: &str,
        state: &str,
        message: Option<&str>,
        seq: u64,
    ) -> Result<(), HerdrError> {
        let mut args = strings(&[
            "pane",
            "report-agent",
            pane_id,
            "--source",
            SOURCE,
            "--agent",
            AGENT,
            "--state",
            state,
        ]);
        if let Some(message) = message {
            args.extend(strings(&["--message", message]));
        }
        args.extend(strings(&["--seq", &seq.to_string()]));
        self.call(&args).map(|_| ())
    }

    /// `herdr notification show`, best effort.
    pub fn notify(&self, title: &str, body: &str) {
        let mut args = strings(&["notification", "show", title]);
        if !body.is_empty() {
            args.extend(strings(&["--body", body]));
        }
        if let Err(e) = self.call(&args) {
            eprintln!("branchyard-herdr: {e}");
        }
    }
}

pub fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}
