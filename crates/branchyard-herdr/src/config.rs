//! Where the plugin finds the Branchyard server, `by` and Herdr.
//!
//! Each setting comes from a command-line flag, else the environment
//! (the same `BRANCHYARD_*` variables `by --remote` reads), else
//! `config.env` in the plugin's Herdr config directory: `KEY=VALUE` lines,
//! `#` comments. Herdr starts plugin commands with its own server's
//! environment, so the file is usually where they are set.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The plugin id in `herdr-plugin.toml`, when Herdr does not say.
pub const DEFAULT_PLUGIN_ID: &str = "branchyard";

/// The variables `by --remote` reads, passed on to every `by` the plugin
/// starts.
pub const REMOTE_VARS: [&str; 4] = [
    "BRANCHYARD_REMOTE",
    "BRANCHYARD_TOKEN_FILE",
    "BRANCHYARD_REPO",
    "BRANCHYARD_CA_FILE",
];

#[derive(Clone, Debug)]
pub struct Config {
    values: BTreeMap<String, String>,
}

impl Config {
    /// Settings from `flags` (variable name to value), the process
    /// environment, and the config file, in that order.
    pub fn load(flags: &[(String, String)]) -> Config {
        Config::from_sources(flags, |name| std::env::var(name).ok())
    }

    pub fn from_sources(
        flags: &[(String, String)],
        env: impl Fn(&str) -> Option<String>,
    ) -> Config {
        let mut values = BTreeMap::new();
        if let Some(dir) = env("HERDR_PLUGIN_CONFIG_DIR").filter(|d| !d.is_empty()) {
            if let Ok(text) = std::fs::read_to_string(Path::new(&dir).join("config.env")) {
                values.extend(parse_env_file(&text));
            }
        }
        for name in REMOTE_VARS.iter().chain(&[
            "BRANCHYARD_BY",
            "BRANCHYARD_HERDR_DEBOUNCE_MS",
            "BRANCHYARD_HERDR_WORKSPACE",
            "BRANCHYARD_HERDR_SEND_ARGS",
            "HERDR_BIN_PATH",
            "HERDR_PLUGIN_ID",
            "HERDR_PLUGIN_STATE_DIR",
            "HERDR_PLUGIN_CONTEXT_JSON",
            "HERDR_WORKSPACE_ID",
            "HERDR_PANE_ID",
            "BRANCHYARD_HERDR_BRANCH",
        ]) {
            if let Some(value) = env(name).filter(|v| !v.is_empty()) {
                values.insert((*name).to_owned(), value);
            }
        }
        for (name, value) in flags {
            values.insert(name.clone(), value.clone());
        }
        Config { values }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn remote(&self) -> Result<&str, String> {
        self.get("BRANCHYARD_REMOTE").ok_or_else(|| {
            "no Branchyard server: pass --remote URL, set BRANCHYARD_REMOTE, or add it to \
             config.env in the directory `herdr plugin config-dir branchyard` prints"
                .to_owned()
        })
    }

    pub fn token_file(&self) -> Result<&str, String> {
        self.get("BRANCHYARD_TOKEN_FILE").ok_or_else(|| {
            "no token: pass --token-file FILE, set BRANCHYARD_TOKEN_FILE, or add it to config.env"
                .to_owned()
        })
    }

    /// The Herdr executable: the running Herdr's, else `herdr` on `PATH`.
    pub fn herdr(&self) -> PathBuf {
        PathBuf::from(self.get("HERDR_BIN_PATH").unwrap_or("herdr"))
    }

    /// The `by` executable: `BRANCHYARD_BY`, else a `by` beside this
    /// program (a cargo build puts them together), else `by` on `PATH`.
    pub fn by(&self) -> PathBuf {
        if let Some(by) = self.get("BRANCHYARD_BY") {
            return PathBuf::from(by);
        }
        std::env::current_exe()
            .ok()
            .and_then(|exe| Some(exe.parent()?.join("by")))
            .filter(|by| by.is_file())
            .unwrap_or_else(|| PathBuf::from("by"))
    }

    pub fn plugin_id(&self) -> &str {
        self.get("HERDR_PLUGIN_ID").unwrap_or(DEFAULT_PLUGIN_ID)
    }

    /// Where the pane map is kept: Herdr's state directory for the plugin,
    /// else `~/.local/state/branchyard-herdr` outside Herdr.
    pub fn state_dir(&self) -> PathBuf {
        if let Some(dir) = self.get("HERDR_PLUGIN_STATE_DIR") {
            return PathBuf::from(dir);
        }
        let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
        Path::new(&home).join(".local/state/branchyard-herdr")
    }

    pub fn debounce(&self) -> Duration {
        let ms = self
            .get("BRANCHYARD_HERDR_DEBOUNCE_MS")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(250);
        Duration::from_millis(ms.min(60_000))
    }

    /// The workspace branch tabs open in: configured, else the one the
    /// bridge's own pane is in, else Herdr's active workspace.
    pub fn workspace(&self) -> Option<&str> {
        self.get("BRANCHYARD_HERDR_WORKSPACE")
            .or_else(|| self.get("HERDR_WORKSPACE_ID"))
    }

    /// `KEY=VALUE` for each `by --remote` setting that is known, for the
    /// processes the plugin starts in Herdr panes.
    pub fn remote_env(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = REMOTE_VARS
            .iter()
            .filter_map(|name| Some(((*name).to_owned(), self.get(name)?.to_owned())))
            .collect();
        env.push(("BRANCHYARD_BY".into(), self.by().display().to_string()));
        env
    }
}

/// `KEY=VALUE` lines; blank lines and `#` comments are skipped, and a
/// value may be wrapped in one pair of single or double quotes.
pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            let value = ['"', '\'']
                .iter()
                .find_map(|q| value.strip_prefix(*q)?.strip_suffix(*q))
                .unwrap_or(value);
            Some((key.trim().to_owned(), value.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_files_are_key_value_lines() {
        let parsed = parse_env_file(
            "# comment\n\nBRANCHYARD_REMOTE=http://h:1\nexport BRANCHYARD_REPO = 'app'\nX=\"a b\"\nnonsense\n",
        );
        assert_eq!(
            parsed,
            vec![
                ("BRANCHYARD_REMOTE".into(), "http://h:1".into()),
                ("BRANCHYARD_REPO".into(), "app".into()),
                ("X".into(), "a b".into()),
            ]
        );
    }

    #[test]
    fn flags_beat_the_environment_which_beats_the_file() {
        let temp = tempfile::Builder::new()
            .prefix("by-herdr-config-")
            .tempdir()
            .unwrap();
        let dir = temp.path();
        std::fs::write(
            dir.join("config.env"),
            "BRANCHYARD_REMOTE=http://file\nBRANCHYARD_REPO=file\nBRANCHYARD_TOKEN_FILE=/file\n",
        )
        .unwrap();
        let dir_text = dir.display().to_string();
        let env = |name: &str| match name {
            "HERDR_PLUGIN_CONFIG_DIR" => Some(dir_text.clone()),
            "BRANCHYARD_REPO" => Some("env".to_owned()),
            "BRANCHYARD_REMOTE" => Some("http://env".to_owned()),
            _ => None,
        };
        let config =
            Config::from_sources(&[("BRANCHYARD_REMOTE".into(), "http://flag".into())], env);
        assert_eq!(config.remote().unwrap(), "http://flag");
        assert_eq!(config.get("BRANCHYARD_REPO"), Some("env"));
        assert_eq!(config.token_file().unwrap(), "/file");
        assert_eq!(config.plugin_id(), "branchyard");
        assert_eq!(config.debounce(), Duration::from_millis(250));
    }
}
