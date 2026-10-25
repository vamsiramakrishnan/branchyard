//! Which harnesses a machine has, and keeping them there: the harness
//! lifecycle (docs/harness-lifecycle.md).
//!
//! Detection is one POSIX `sh` script, generated from the harness catalog
//! (`catalog/harnesses.toml`) and run on the machine in question: here
//! with `/bin/sh -s`, over `ssh`, or through a recipe's transport. Nothing
//! needs installing there. For each harness it finds the executable (on
//! `PATH`, then in the usual install directories), runs its version
//! command with a timeout, runs its own login status command where
//! upstream records one, and notes which credential files and key
//! variables exist, by name. It never prints or reads a secret's value:
//! a variable is tested with `[ -n "${NAME:-}" ]` and a file with `[ -e ]`.
//!
//! A login state is `verified` only when the harness's own status command
//! said so; otherwise it is `likely` (a key variable or credential file is
//! present, or every known credential file is absent) or unknown. Three
//! harnesses need a look inside their configuration, still by name only
//! (the `probes` module): Antigravity reads a key only when its settings select the
//! Gemini provider, DeepSeek Harness takes its provider from
//! `cordis.patch.yml`, and Gemini CLI's headless runs need the workspace
//! trusted (`GEMINI_CLI_TRUST_WORKSPACE`).
//!
//! Installing runs the catalog's own install command for the harness,
//! pinned to the version its default profile was checked against when the
//! command is a package manager's, and only as [`InstallPolicy`] allows.
//! Every install, update and login is appended to a log
//! ([`HarnessLog`]). The router consults the inventory through
//! [`HarnessGate`].

use branchyard_support::best_effort;
use branchyard_support::time::now_ms;
use branchyard_support::LockExt as _;
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use branchyard_controls::catalog;
use branchyard_recipe::quote;
use serde::{Deserialize, Serialize};

/// Labels derived from an inventory: `harness:<id>` for each harness that
/// can run on the machine.
pub const LABEL_PREFIX: &str = "harness:";
/// How long a version or status command may run.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a cached inventory is used before it is detected again.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);
/// Where harness installers usually put executables, besides `PATH`.
/// `$HOME/` is expanded on the machine. `BRANCHYARD_HARNESS_DIRS` (a
/// colon-separated list) replaces it.
pub const KNOWN_DIRS: &[&str] = &[
    "$HOME/.local/bin",
    "$HOME/.npm-global/bin",
    "$HOME/.bun/bin",
    "$HOME/.claude/local",
    "$HOME/.cargo/bin",
    "/usr/local/bin",
    "/opt/homebrew/bin",
];
/// Programs install commands start with, looked for on the machine.
pub const TOOLS: &[&str] = &["npm", "bun", "pipx", "uv", "brew", "curl", "bash", "sh"];
/// The script's output format.
const FORMAT: &str = "by-inventory 1";
/// Lines of a command's output kept.
const OUTPUT_LINES: usize = 5;
/// Characters of each kept line.
const OUTPUT_WIDTH: usize = 200;

/// What one machine has.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    /// The machine's host name, as it reports it.
    pub host: String,
    /// `uname -s`: `Linux`, `Darwin`.
    pub os: String,
    /// When it was detected, in milliseconds since the Unix epoch.
    pub detected_at_ms: u64,
    /// The harness IDs looked for. One not listed in `harnesses` is not
    /// installed; one not looked for is unknown.
    pub checked: Vec<String>,
    /// The installed harnesses, in catalog order.
    pub harnesses: Vec<HarnessState>,
    /// Install programs found on the machine (`npm`, `curl`, ...), by name,
    /// with their paths.
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
}

/// One installed harness.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HarnessState {
    /// The catalog ID.
    pub id: String,
    /// The executable found.
    pub path: String,
    /// Found through `PATH`; otherwise in one of the install directories,
    /// where a harness started by name would not find it.
    pub on_path: bool,
    /// The version its version command printed, when one could be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Why there is no version: the command timed out, failed, or printed
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_note: Option<String>,
    pub login: Login,
    /// How much of its login's 5-hour and weekly windows is used, where
    /// `by usage` can tell (Claude Code and Codex on this machine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<Quota>,
}

/// Whether a harness is logged in, and how sure that is.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
    pub state: LoginState,
    pub evidence: Evidence,
    /// What decided it, for people: `codex login status: Logged in using
    /// ChatGPT`, `OPENAI_API_KEY is set`, `~/.codex/auth.json exists`.
    pub detail: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginState {
    LoggedIn,
    LoggedOut,
    #[default]
    Unknown,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// The harness's own status command said so.
    Verified,
    /// Inferred from credential files and key variables, by name.
    Likely,
    /// Nothing to go on.
    #[default]
    None,
}

/// A login's usage, as `by usage` reads it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_percent: Option<f64>,
    /// The harness said a limit was reached.
    pub limit_reached: bool,
    /// When the fuller window resets, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
}

impl fmt::Display for LoginState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LoginState::LoggedIn => "logged in",
            LoginState::LoggedOut => "logged out",
            LoginState::Unknown => "unknown",
        })
    }
}

impl fmt::Display for Evidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Evidence::Verified => "verified",
            Evidence::Likely => "likely",
            Evidence::None => "unknown",
        })
    }
}

impl Inventory {
    /// The installed harness with this catalog ID.
    pub fn get(&self, id: &str) -> Option<&HarnessState> {
        self.harnesses.iter().find(|h| h.id == id)
    }

    /// Whether `id` was looked for.
    pub fn checked(&self, id: &str) -> bool {
        self.checked.iter().any(|c| c == id)
    }

    /// Whether `id` can run on this machine: installed, on `PATH`, not
    /// logged out, and not at a usage limit. The error says why not, and
    /// what to do.
    pub fn ready(&self, id: &str) -> Result<&HarnessState, String> {
        let Some(state) = self.get(id) else {
            return Err(match self.checked(id) {
                true => format!(
                    "{id} is not installed on {} (by harnesses install {id})",
                    self.host
                ),
                false => format!("{id} was not looked for on {}", self.host),
            });
        };
        state.ready().map(|()| state)
    }

    /// `harness:<id>` for each harness that can run here, sorted.
    pub fn labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .harnesses
            .iter()
            .filter(|h| h.ready().is_ok())
            .map(|h| format!("{LABEL_PREFIX}{}", h.id))
            .collect();
        labels.sort();
        labels.dedup();
        labels
    }

    /// Replace or add `state`, or forget `id` when `state` is `None`.
    pub fn update(&mut self, id: &str, state: Option<HarnessState>) {
        if !self.checked(id) {
            self.checked.push(id.to_owned());
        }
        self.harnesses.retain(|h| h.id != id);
        if let Some(state) = state {
            self.harnesses.push(state);
            let order = |id: &str| catalog::harnesses().iter().position(|e| e.id == id);
            self.harnesses.sort_by_key(|h| order(&h.id));
        }
    }
}

impl HarnessState {
    /// Why it cannot run, if it cannot.
    pub fn ready(&self) -> Result<(), String> {
        let id = &self.id;
        if !self.on_path {
            return Err(format!(
                "{id} is at {} but not on PATH; add its directory to PATH",
                self.path
            ));
        }
        if self.login.state == LoginState::LoggedOut {
            return Err(format!(
                "{id} is {} logged out ({}); run by harnesses login {id}",
                match self.login.evidence {
                    Evidence::Verified => "verified",
                    _ => "likely",
                },
                self.login.detail
            ));
        }
        if self.quota.as_ref().is_some_and(|q| q.limit_reached) {
            return Err(format!(
                "{id}'s login has reached its usage limit (by usage)"
            ));
        }
        Ok(())
    }
}

// What is known about each harness's login, beyond the catalog.

/// How to tell whether one harness is logged in.
struct LoginCheck {
    harness: &'static str,
    /// Its own status command (the arguments after the executable), with
    /// the phrases that mean logged out and logged in, checked in that
    /// order, ignoring case.
    status: Option<StatusCommand>,
    /// Credential files, as `sh` words expanded on the machine, with how
    /// to show each.
    files: &'static [(&'static str, &'static str)],
    /// Key variables besides the catalog's `auth_env`.
    env: &'static [&'static str],
    /// On macOS the login is in the keychain, so no file proves nothing.
    keychain_on_macos: bool,
}

struct StatusCommand {
    args: &'static [&'static str],
    logged_out: &'static [&'static str],
    logged_in: &'static [&'static str],
}

/// From emdash's agent plugins (`checkStatus`: Codex runs `codex login
/// status` and reads it with these patterns) and Scion's harness configs
/// (`auth.types.*.required_files` and `autodetect.env`). Claude Code's
/// emdash status check (`claude/auth.ts`) is not in the vendored sources,
/// so its login is read from files and variables only.
const LOGIN_CHECKS: &[LoginCheck] = &[
    LoginCheck {
        harness: "claude-code",
        status: None,
        files: &[(
            "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/.credentials.json",
            "~/.claude/.credentials.json",
        )],
        env: &["CLAUDE_CODE_OAUTH_TOKEN"],
        keychain_on_macos: true,
    },
    LoginCheck {
        harness: "codex",
        status: Some(StatusCommand {
            args: &["login", "status"],
            logged_out: &[
                "not authenticated",
                "not logged in",
                "not signed in",
                "login required",
            ],
            logged_in: &["authenticated", "logged in", "signed in"],
        }),
        files: &[(
            "${CODEX_HOME:-$HOME/.codex}/auth.json",
            "~/.codex/auth.json",
        )],
        env: &["CODEX_API_KEY"],
        keychain_on_macos: false,
    },
    LoginCheck {
        harness: "gemini-cli",
        status: None,
        files: &[(
            "$HOME/.gemini/oauth_creds.json",
            "~/.gemini/oauth_creds.json",
        )],
        env: &[],
        keychain_on_macos: false,
    },
    LoginCheck {
        harness: "opencode",
        status: None,
        files: &[(
            "$HOME/.local/share/opencode/auth.json",
            "~/.local/share/opencode/auth.json",
        )],
        env: &[],
        keychain_on_macos: false,
    },
    LoginCheck {
        harness: "antigravity",
        status: None,
        files: &[(
            "$HOME/.gemini/antigravity-cli/antigravity-oauth-token",
            "~/.gemini/antigravity-cli/antigravity-oauth-token",
        )],
        // GEMINI_API_KEY is the catalog's `auth_env`: agy reads a key from
        // it alone (GOOGLE_API_KEY has no effect), and only with
        // `modelProvider` set to `gemini` in its settings.
        env: &["AGY_TOKEN"],
        keychain_on_macos: false,
    },
    // DeepSeek Harness 0.2.0-rc.2 has no login command: DEEPSEEK_API_KEY
    // (the catalog's `auth_env`) feeds its shipped provider, and `probes`
    // reads the provider a `cordis.patch.yml` configures.
    LoginCheck {
        harness: "deepseek-harness",
        status: None,
        files: &[],
        env: &[],
        keychain_on_macos: false,
    },
    // Oh My Pi 18.10.3 reads provider keys from its environment (the
    // catalog's `auth_env`) and keeps `omp login` logins in a database,
    // which `probes` reads for provider names.
    LoginCheck {
        harness: "oh-my-pi",
        status: None,
        files: &[],
        env: &[],
        keychain_on_macos: false,
    },
    LoginCheck {
        harness: "github-copilot",
        status: None,
        files: &[("$HOME/.copilot/config.json", "~/.copilot/config.json")],
        env: &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"],
        keychain_on_macos: false,
    },
    LoginCheck {
        harness: "grok-build",
        status: None,
        files: &[("$HOME/.grok/auth.json", "~/.grok/auth.json")],
        env: &["XAI_API_KEY"],
        keychain_on_macos: false,
    },
    LoginCheck {
        harness: "hermes",
        status: None,
        files: &[],
        env: &["GOOGLE_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"],
        keychain_on_macos: false,
    },
    LoginCheck {
        harness: "muse-code",
        status: None,
        files: &[],
        env: &["META_API_KEY"],
        keychain_on_macos: false,
    },
];

/// Version arguments other than `--version`: emdash's `versionArgs`.
const VERSION_ARGS: &[(&str, &[&str])] = &[("jules", &["version"])];

fn login_check(id: &str) -> Option<&'static LoginCheck> {
    LOGIN_CHECKS.iter().find(|c| c.harness == id)
}

/// The `cordis.patch.yml` files DeepSeek Harness composes its providers
/// from, as `probes` reads them: under `$DSH_HOME` (default `~/.dsh`) and
/// under its ACP profile there, for people.
const DSH_CONFIGS: &[&str] = &[
    "~/.dsh/cordis.patch.yml",
    "~/.dsh/profiles/acp/cordis.patch.yml",
];

/// Where Oh My Pi keeps the logins `omp login` stores, as shown to people.
const OMP_LOGINS: &str = "~/.omp/agent/agent.db";

/// The Bun release Oh My Pi 18.10.3 needs (`engines.bun` of
/// `@oh-my-pi/pi-coding-agent`, and `MIN_BUN_VERSION` of its installer): an
/// older Bun fails to parse `omp` and its version command exits 1.
const OMP_MIN_BUN: &str = "1.3.14";

/// What only a look inside a configuration file or a flag variable can
/// tell, as `sh` run on the machine once the harness is found. Each prints
/// `fact ID NAME` (or an `env` line for a key variable that is set) and
/// never a value: the files are searched, not printed.
///
/// - Antigravity 1.3.3 reads `GEMINI_API_KEY` only when its `settings.json`
///   has `"modelProvider": "gemini"`: `model-provider-gemini`.
/// - Gemini CLI 0.63.0's headless `gemini -p` refuses an untrusted
///   directory unless `GEMINI_CLI_TRUST_WORKSPACE` is `true`:
///   `trust-workspace`.
/// - DeepSeek Harness 0.2.0-rc.2 composes its `llm-pi-ai` providers from
///   `cordis.patch.yml` under `$DSH_HOME` and the profile's: a `providers:`
///   key is `providers FILE`, and each `apiKeyEnv:` variable that is set is
///   an `env` line.
/// - Oh My Pi 18.10.3 is a Bun script: `runs-on-bun` when its executable's
///   first bytes name `bun`, and `bun VERSION` or `bun missing` for the Bun
///   on `PATH`. Its `omp login` logins are rows of `~/.omp/agent/agent.db`:
///   `login PROVIDER` for each provider with one, read with `sqlite3` or
///   `python3` (`login-store unread` when the file exists and neither
///   does); the provider's name is read, never the credential. A key in
///   `~/.omp/agent/.env` or `~/.omp/.env`, which omp loads, is `env-file
///   NAME FILE`, the name alone.
fn probes(id: &str) -> &'static str {
    match id {
        "antigravity" => {
            r##"  grep -Eq '"modelProvider"[[:space:]]*:[[:space:]]*"gemini"' "$HOME/.gemini/antigravity-cli/settings.json" 2>/dev/null && echo 'fact antigravity model-provider-gemini'
"##
        }
        "gemini-cli" => {
            r##"  [ "${GEMINI_CLI_TRUST_WORKSPACE:-}" = true ] && echo 'fact gemini-cli trust-workspace'
"##
        }
        "deepseek-harness" => {
            r##"  ph=${DSH_HOME:-$HOME/.dsh}
  for pf in cordis.patch.yml profiles/acp/cordis.patch.yml; do
    [ -f "$ph/$pf" ] || continue
    grep -Eq '^[[:space:]-]*providers:' "$ph/$pf" 2>/dev/null && echo "fact deepseek-harness providers $pf"
    for pv in $(sed -n 's/^[[:space:]-]*apiKeyEnv:[^A-Za-z_]*\([A-Za-z_][A-Za-z0-9_]*\).*$/\1/p' "$ph/$pf" 2>/dev/null); do
      eval "[ -n \"\${$pv:-}\" ]" && echo "env deepseek-harness $pv"
    done
  done
"##
        }
        "oh-my-pi" => {
            r##"  head -c 64 "$f" 2>/dev/null | tr -d '\0' | grep -q bun && echo 'fact oh-my-pi runs-on-bun'
  if b=$(command -v bun 2>/dev/null); then echo "fact oh-my-pi bun $("$b" --version 2>/dev/null | head -n1)"; else echo 'fact oh-my-pi bun missing'; fi
  db="$HOME/.omp/agent/agent.db"
  if [ -f "$db" ]; then
    q='select distinct provider from auth_credentials where disabled_cause is null'
    if command -v sqlite3 >/dev/null 2>&1; then
      for pv in $(sqlite3 -readonly "$db" "$q" 2>/dev/null); do echo "fact oh-my-pi login $pv"; done
    elif command -v python3 >/dev/null 2>&1; then
      for pv in $(python3 -I -c 'import sqlite3,sys
c=sqlite3.connect("file:"+sys.argv[1]+"?mode=ro",uri=True)
print("\n".join(r[0] for r in c.execute(sys.argv[2])))' "$db" "$q" 2>/dev/null); do echo "fact oh-my-pi login $pv"; done
    else
      echo 'fact oh-my-pi login-store unread'
    fi
  fi
  for ef in "$HOME/.omp/agent/.env" "$HOME/.omp/.env"; do
    [ -f "$ef" ] || continue
    for pv in $(sed -n 's/^[[:space:]]*\(export[[:space:]][[:space:]]*\)\{0,1\}\([A-Za-z_][A-Za-z0-9_]*_API_KEY\)[[:space:]]*=.*/\2/p' "$ef" 2>/dev/null); do echo "fact oh-my-pi env-file $pv ~/${ef#"$HOME"/}"; done
  done
"##
        }
        _ => "",
    }
}

/// Whether a Bun `version` (`1.3.11`) is older than [`OMP_MIN_BUN`].
fn bun_too_old(version: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parts(version) < parts(OMP_MIN_BUN)
}

fn version_args(id: &str) -> &'static [&'static str] {
    VERSION_ARGS
        .iter()
        .find(|(h, _)| *h == id)
        .map_or(&["--version"], |(_, a)| *a)
}

/// The key variables a harness reads instead of a login: the catalog's
/// `auth_env` and the ones above, once each.
pub fn key_variables(id: &str) -> Vec<String> {
    let mut vars: Vec<String> = catalog::harness(id)
        .map(|e| e.auth_env.clone())
        .unwrap_or_default();
    for var in login_check(id).map(|c| c.env).unwrap_or_default() {
        if !vars.iter().any(|v| v == var) {
            vars.push((*var).to_owned());
        }
    }
    vars
}

/// The status command `id`'s login is verified with, for people.
pub fn status_command(id: &str) -> Option<String> {
    let entry = catalog::harness(id)?;
    let check = login_check(id)?.status.as_ref()?;
    Some(format!(
        "{} {}",
        entry.binaries.first()?,
        check.args.join(" ")
    ))
}

// Detection.

/// How to detect.
#[derive(Clone, Debug)]
pub struct DetectOptions {
    /// How long each version or status command may run.
    pub timeout: Duration,
    /// Directories looked in after `PATH`; `$HOME/` is expanded on the
    /// machine.
    pub dirs: Vec<String>,
    /// Only these catalog IDs; every harness when `None`.
    pub only: Option<Vec<String>>,
}

impl Default for DetectOptions {
    fn default() -> DetectOptions {
        DetectOptions {
            timeout: DEFAULT_TIMEOUT,
            dirs: KNOWN_DIRS.iter().map(|d| (*d).to_owned()).collect(),
            only: None,
        }
    }
}

impl DetectOptions {
    /// The defaults, with `BRANCHYARD_HARNESS_DIRS` (colon-separated, may
    /// be empty) in place of [`KNOWN_DIRS`] and `BRANCHYARD_HARNESS_TIMEOUT`
    /// (seconds) in place of [`DEFAULT_TIMEOUT`] when set.
    pub fn from_env() -> DetectOptions {
        let mut options = DetectOptions::default();
        if let Some(seconds) = std::env::var("BRANCHYARD_HARNESS_TIMEOUT")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s > 0.0)
        {
            options.timeout = Duration::from_secs_f64(seconds);
        }
        if let Some(dirs) = std::env::var_os("BRANCHYARD_HARNESS_DIRS") {
            options.dirs = std::env::split_paths(&dirs)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.display().to_string())
                .collect();
        }
        options
    }

    fn ids(&self) -> Vec<&'static catalog::HarnessEntry> {
        catalog::harnesses()
            .iter()
            .filter(|e| !e.binaries.is_empty())
            .filter(|e| match &self.only {
                Some(only) => only.contains(&e.id),
                None => true,
            })
            .collect()
    }
}

/// A directory as a shell word: `$HOME/x` keeps `$HOME` for the machine to
/// expand.
fn dir_word(dir: &str) -> String {
    match dir.strip_prefix("$HOME/") {
        Some(rest) => format!("\"$HOME\"/{}", quote(rest)),
        None => quote(dir),
    }
}

/// A variable name safe to put in a script.
fn variable(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// The detection script: run with `sh -s` or `sh -c`, it prints one
/// record per line (see [`parse`]).
pub fn script(options: &DetectOptions) -> String {
    let ticks = (options.timeout.as_millis() / 100).max(1);
    let mut s = String::new();
    s.push_str(&format!(
        "# Branchyard harness detection (docs/harness-lifecycle.md). Prints names,\n\
         # paths and versions; tests key variables and files for presence only.\n\
         T={ticks}\n\
         D=$(mktemp -d 2>/dev/null) || {{ D=/tmp/by-inventory.$$; mkdir -p \"$D\" || exit 1; }}\n\
         trap 'rm -rf \"$D\"' EXIT\n\
         echo '{FORMAT}'\n\
         echo \"host $(hostname 2>/dev/null || uname -n 2>/dev/null)\"\n\
         echo \"os $(uname -s 2>/dev/null)\"\n"
    ));
    // run OUT TAG ID CMD...: CMD with no input and its output in OUT, killed
    // after T tenths of a second; prints `TAG ID STATUS` (or `timeout`),
    // then the output's first lines as `TAG-out ID LINE`.
    s.push_str(
        "run() {\n\
         \x20 o=$1; t=$2; i=$3; shift 3\n\
         \x20 rm -f \"$o\" \"$o.timeout\"\n\
         \x20 \"$@\" </dev/null >\"$o\" 2>&1 &\n\
         \x20 p=$!\n\
         \x20 ( n=0; while [ $n -lt $T ]; do kill -0 $p 2>/dev/null || exit 0; sleep 0.1; n=$((n+1)); done; : >\"$o.timeout\"; kill -9 $p 2>/dev/null ) >/dev/null 2>&1 &\n\
         \x20 w=$!\n\
         \x20 wait $p 2>/dev/null; r=$?\n\
         \x20 kill $w 2>/dev/null; wait $w 2>/dev/null\n\
         \x20 if [ -e \"$o.timeout\" ]; then r=timeout; fi\n\
         \x20 echo \"$t $i $r\"\n",
    );
    s.push_str(&format!(
        "  tr -d '\\r' <\"$o\" | tr '\\t' ' ' | grep -v '^[[:space:]]*$' | head -n {OUTPUT_LINES} | cut -c1-{OUTPUT_WIDTH} | sed \"s/^/$t-out $i /\"\n}}\n"
    ));
    // look ID BIN...: the first executable named BIN on PATH, else in the
    // install directories; prints `found ID ON_PATH PATH`.
    let dirs: Vec<String> = options.dirs.iter().map(|d| dir_word(d)).collect();
    s.push_str(&format!(
        "look() {{\n\
         \x20 i=$1; shift; f=; on=1\n\
         \x20 for b in \"$@\"; do f=$(command -v \"$b\" 2>/dev/null); case $f in /*) break;; esac; f=; done\n\
         \x20 if [ -z \"$f\" ]; then on=0; for d in {}; do for b in \"$@\"; do if [ -f \"$d/$b\" ] && [ -x \"$d/$b\" ]; then f=$d/$b; break 2; fi; done; done; fi\n\
         \x20 [ -n \"$f\" ] && echo \"found $i $on $f\"\n\
         }}\n",
        match dirs.is_empty() {
            true => "/nonexistent".to_owned(),
            false => dirs.join(" "),
        }
    ));
    for tool in TOOLS {
        s.push_str(&format!(
            "f=$(command -v {tool} 2>/dev/null) && case $f in /*) echo \"tool {tool} $f\";; esac\n"
        ));
    }
    for entry in options.ids() {
        let id = &entry.id;
        let bins: Vec<String> = entry.binaries.iter().map(|b| quote(b)).collect();
        s.push_str(&format!("echo 'checked {id}'\n"));
        s.push_str(&format!("if look {id} {}; then\n", bins.join(" ")));
        let version: Vec<String> = version_args(id).iter().map(|a| quote(a)).collect();
        s.push_str(&format!(
            "  run \"$D/out\" version {id} \"$f\" {}\n",
            version.join(" ")
        ));
        let check = login_check(id);
        if let Some(status) = check.and_then(|c| c.status.as_ref()) {
            let args: Vec<String> = status.args.iter().map(|a| quote(a)).collect();
            s.push_str(&format!(
                "  run \"$D/out\" status {id} \"$f\" {}\n",
                args.join(" ")
            ));
        }
        for var in key_variables(id).iter().filter(|v| variable(v)) {
            s.push_str(&format!(
                "  [ -n \"${{{var}:-}}\" ] && echo 'env {id} {var}'\n"
            ));
        }
        for (expr, shown) in check.map(|c| c.files).unwrap_or_default() {
            s.push_str(&format!(
                "  [ -e \"{expr}\" ] && echo {}\n",
                quote(&format!("file {id} {shown}"))
            ));
        }
        s.push_str(probes(id));
        s.push_str("fi\n");
    }
    s.push_str("echo end\n");
    s
}

/// A command's result in the script's output.
#[derive(Default)]
struct Ran {
    status: Option<String>,
    lines: Vec<String>,
}

#[derive(Default)]
struct Found {
    path: String,
    on_path: bool,
    version: Ran,
    status: Option<Ran>,
    env: Vec<String>,
    files: Vec<String>,
    /// What [`probes`] found, as it printed it after `fact ID`.
    facts: Vec<String>,
}

/// Read the script's output into an inventory taken at `now_ms`.
pub fn parse(output: &str, now_ms: u64) -> Result<Inventory, String> {
    let mut lines = output.lines();
    if lines.next().map(str::trim) != Some(FORMAT) {
        let first = output.lines().next().unwrap_or("");
        return Err(format!(
            "the detection script did not run: it printed {first:?} instead of {FORMAT:?}"
        ));
    }
    let mut inventory = Inventory {
        detected_at_ms: now_ms,
        ..Inventory::default()
    };
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    let mut ended = false;
    for line in lines {
        let (tag, rest) = line.split_once(' ').unwrap_or((line, ""));
        let mut words = rest.splitn(2, ' ');
        let mut word = || words.next().unwrap_or("").to_owned();
        match tag {
            "host" => inventory.host = rest.trim().to_owned(),
            "os" => inventory.os = rest.trim().to_owned(),
            "tool" => {
                let name = word();
                inventory.tools.insert(name, word());
            }
            "checked" => inventory.checked.push(rest.trim().to_owned()),
            "found" => {
                let id = word();
                let rest = word();
                let (on_path, path) = rest.split_once(' ').unwrap_or(("1", &rest));
                found.insert(
                    id,
                    Found {
                        path: path.to_owned(),
                        on_path: on_path == "1",
                        ..Found::default()
                    },
                );
            }
            "version" | "status" | "version-out" | "status-out" | "env" | "file" | "fact" => {
                let id = word();
                let value = word();
                let Some(f) = found.get_mut(&id) else {
                    continue;
                };
                match tag {
                    "version" => f.version.status = Some(value),
                    "version-out" => f.version.lines.push(value),
                    "status" => {
                        f.status = Some(Ran {
                            status: Some(value),
                            lines: Vec::new(),
                        })
                    }
                    "status-out" => {
                        if let Some(s) = &mut f.status {
                            s.lines.push(value)
                        }
                    }
                    "env" => f.env.push(value),
                    "fact" => f.facts.push(value),
                    _ => f.files.push(value),
                }
            }
            "end" => ended = true,
            _ => {}
        }
    }
    if !ended {
        return Err("the detection script stopped before it finished".into());
    }
    let order = |id: &str| catalog::harnesses().iter().position(|e| e.id == id);
    let mut states: Vec<HarnessState> = found
        .into_iter()
        .map(|(id, f)| state(&id, f, &inventory.os))
        .collect();
    states.sort_by_key(|s| order(&s.id));
    inventory.harnesses = states;
    Ok(inventory)
}

fn state(id: &str, found: Found, os: &str) -> HarnessState {
    let (version, version_note) = match found.version.status.as_deref() {
        Some("timeout") => (None, Some("its version command timed out".to_owned())),
        Some(status) => match found.version.lines.iter().find_map(|l| parse_version(l)) {
            Some(v) => (Some(v), None),
            None if status == "0" => (None, Some("its version command printed no version".into())),
            None => (
                None,
                Some(format!("its version command exited with status {status}")),
            ),
        },
        None => (None, Some("its version command did not run".into())),
    };
    // omp is a Bun script: when it cannot even print its version and the
    // Bun on PATH is missing or too old, that is why.
    let version_note = match (id, &version) {
        ("oh-my-pi", None) if found.facts.iter().any(|f| f == "runs-on-bun") => {
            let bun = found.facts.iter().find_map(|f| f.strip_prefix("bun "));
            let found_bun = match bun {
                Some("missing") => Some("no bun is on PATH".to_owned()),
                Some(v) if bun_too_old(v) => Some(format!("the bun on PATH is {v}")),
                _ => None,
            };
            match found_bun {
                Some(found_bun) => Some(format!(
                    "{}; omp runs on Bun {OMP_MIN_BUN} or newer and {found_bun}: \
                     `curl -fsSL https://omp.sh/install | sh` installs or upgrades it",
                    version_note.unwrap_or_default()
                )),
                None => version_note,
            }
        }
        _ => version_note,
    };
    let login = login(id, &found, os);
    HarnessState {
        id: id.to_owned(),
        path: found.path,
        on_path: found.on_path,
        version,
        version_note,
        login,
        quota: None,
    }
}

fn login(id: &str, found: &Found, os: &str) -> Login {
    let mut login = login_from_evidence(id, found, os);
    // Gemini CLI's headless `gemini -p` refuses an untrusted directory
    // whatever the login; say so where the login is shown.
    if id == "gemini-cli" && login.state == LoginState::LoggedIn {
        login
            .detail
            .push_str(match found.facts.iter().any(|f| f == "trust-workspace") {
                true => {
                    "; GEMINI_CLI_TRUST_WORKSPACE=true is set, so headless runs trust the workspace"
                }
                false => {
                    "; headless runs (gemini -p) refuse an untrusted workspace: set \
                     GEMINI_CLI_TRUST_WORKSPACE=true or pass --skip-trust"
                }
            });
    }
    login
}

fn login_from_evidence(id: &str, found: &Found, os: &str) -> Login {
    let check = login_check(id);
    let program = catalog::harness(id)
        .and_then(|e| e.binaries.first().cloned())
        .unwrap_or_else(|| id.to_owned());
    let verified = |state, detail: String| Login {
        state,
        evidence: Evidence::Verified,
        detail,
    };
    let likely = |state, detail: String| Login {
        state,
        evidence: Evidence::Likely,
        detail,
    };
    // The harness's own answer, when it gave one.
    let mut said: Option<(LoginState, String)> = None;
    if let (Some(command), Some(ran)) = (check.and_then(|c| c.status.as_ref()), &found.status) {
        let text = ran.lines.join(" ");
        let lower = text.to_lowercase();
        // Warnings the command prints first (Codex names its home directory)
        // are not the answer; show its last line.
        let answer = ran.lines.last().map_or(text.trim(), |l| l.trim());
        let shown = format!("{program} {}: {answer}", command.args.join(" "));
        if ran.status.as_deref() != Some("timeout") {
            if command.logged_out.iter().any(|p| lower.contains(p)) {
                said = Some((LoginState::LoggedOut, shown));
            } else if command.logged_in.iter().any(|p| lower.contains(p)) {
                said = Some((LoginState::LoggedIn, shown));
            }
        }
    }
    if let Some((LoginState::LoggedIn, detail)) = &said {
        // Codex sends its requests with CODEX_API_KEY when the variable is
        // set, whatever `codex login status` says: that key's organization is
        // billed, and a key without quota answers "Quota exceeded" beside a
        // valid login. Say so, rather than report the login alone.
        let detail = match found.env.iter().find(|v| v.as_str() == "CODEX_API_KEY") {
            Some(var) if id == "codex" => format!(
                "{detail}; {var} is set, and Codex uses it instead of the login (unset it, or \
                 run isolated, to use the login)"
            ),
            _ => detail.clone(),
        };
        return verified(LoginState::LoggedIn, detail);
    }
    // Antigravity reads GEMINI_API_KEY only when its settings select the
    // gemini provider; without that it asks for a Google sign-in.
    let provider = found.facts.iter().any(|f| f == "model-provider-gemini");
    let key_is_used =
        |var: &&String| id != "antigravity" || var.as_str() != "GEMINI_API_KEY" || provider;
    // A key variable works whatever the login says.
    if let Some(var) = found.env.iter().find(key_is_used) {
        return likely(
            LoginState::LoggedIn,
            match (id, var.as_str()) {
                ("antigravity", "GEMINI_API_KEY") => "GEMINI_API_KEY is set and settings.json \
                    selects the gemini provider (an API key; not checked)"
                    .to_owned(),
                _ => format!("{var} is set (an API key; not checked)"),
            },
        );
    }
    if let Some((state, detail)) = said {
        return verified(state, detail);
    }
    if let Some(file) = found.files.first() {
        return likely(LoginState::LoggedIn, format!("{file} exists"));
    }
    if id == "antigravity" && found.env.iter().any(|v| v == "GEMINI_API_KEY") {
        // Still a key Branchyard can use: isolated runs set the provider.
        return likely(
            LoginState::LoggedIn,
            "GEMINI_API_KEY is set but ~/.gemini/antigravity-cli/settings.json does not set \
             modelProvider gemini: agy will ask for a Google sign-in; Branchyard sets it in \
             isolated runs"
                .to_owned(),
        );
    }
    if let Some(file) = found
        .facts
        .iter()
        .find_map(|f| f.strip_prefix("providers "))
        .filter(|_| id == "deepseek-harness")
    {
        return likely(
            LoginState::LoggedIn,
            format!("~/.dsh/{file} configures a provider (its key variable is not checked)"),
        );
    }
    if id == "deepseek-harness" {
        let vars = key_variables(id).join(" or ");
        return likely(
            LoginState::LoggedOut,
            format!(
                "no provider in {} and no {vars}; set {vars} (the shipped provider \
                 deepseek-official reads it) or configure a provider under the llm-pi-ai \
                 plugin in cordis.patch.yml (`dsh --profile acp --dump-config` shows the \
                 composed configuration)",
                DSH_CONFIGS.join(" or ")
            ),
        );
    }
    if id == "oh-my-pi" {
        let logins: Vec<&str> = found
            .facts
            .iter()
            .filter_map(|f| f.strip_prefix("login "))
            .collect();
        if !logins.is_empty() {
            return likely(
                LoginState::LoggedIn,
                format!(
                    "omp login stored for {} in {OMP_LOGINS} (not checked)",
                    logins.join(", ")
                ),
            );
        }
        if let Some(rest) = found.facts.iter().find_map(|f| f.strip_prefix("env-file ")) {
            let (var, file) = rest.split_once(' ').unwrap_or((rest, "~/.omp/agent/.env"));
            return likely(
                LoginState::LoggedIn,
                format!("{var} is set in {file}, which omp loads (an API key; not checked)"),
            );
        }
        let vars = key_variables(id).join(", ");
        if found.facts.iter().any(|f| f == "login-store unread") {
            return Login {
                state: LoginState::Unknown,
                evidence: Evidence::None,
                detail: format!(
                    "{OMP_LOGINS} may hold a login, which needs sqlite3 or python3 on the \
                     machine to read, and none of {vars} is set"
                ),
            };
        }
        return likely(
            LoginState::LoggedOut,
            format!(
                "no login stored in {OMP_LOGINS} and none of {vars} is set; set one, or run \
                 omp login"
            ),
        );
    }
    match check {
        Some(c) if !(c.files.is_empty() || c.keychain_on_macos && os == "Darwin") => {
            let files: Vec<&str> = c.files.iter().map(|(_, shown)| *shown).collect();
            let vars = key_variables(id);
            let mut detail = format!(
                "no {} and no {}",
                files.join(" or "),
                match vars.is_empty() {
                    true => "key variable".to_owned(),
                    false => vars.join(" or "),
                }
            );
            if id == "antigravity" {
                detail.push_str(
                    "; a GEMINI_API_KEY also needs modelProvider gemini in \
                     ~/.gemini/antigravity-cli/settings.json",
                );
            }
            likely(LoginState::LoggedOut, detail)
        }
        _ => Login {
            state: LoginState::Unknown,
            evidence: Evidence::None,
            detail: "Branchyard knows no way to tell for this harness".into(),
        },
    }
}

/// The first version-shaped word in `line`: digits and dots with at least
/// one dot, and any suffix of letters, digits, `.`, `-` and `+`.
pub fn parse_version(line: &str) -> Option<String> {
    for word in line.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | '/')) {
        let word = word.trim_start_matches(['v', 'V']);
        let digits = word
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(word.len());
        let core = word[..digits].trim_end_matches('.');
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() >= 2 && parts.iter().all(|p| !p.is_empty()) {
            let rest = &word[digits..];
            let suffix: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
                .collect();
            let suffix = match suffix.starts_with(['-', '+']) {
                true => suffix,
                false => String::new(),
            };
            return Some(format!("{core}{suffix}"));
        }
    }
    None
}

/// Run the detection script on this machine with `/bin/sh -s`.
pub fn detect_local(options: &DetectOptions) -> Result<Inventory, String> {
    detect_with(options, true, |_| {
        let mut command = Command::new("/bin/sh");
        command.arg("-s");
        command
    })
}

/// Run the detection script with the command `make` returns for it: the
/// script goes on its stdin when `stdin` (`sh -s`, `ssh host sh -s`);
/// otherwise `make` puts it in the arguments (`sh -c SCRIPT`).
#[allow(clippy::expect_used)] // ratchet: branchyard
pub fn detect_with(
    options: &DetectOptions,
    stdin: bool,
    make: impl FnOnce(&str) -> Command,
) -> Result<Inventory, String> {
    let text = script(options);
    let mut command = make(&text);
    command
        .stdin(match stdin {
            true => Stdio::piped(),
            false => Stdio::null(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not run {program}: {e}"))?;
    if stdin {
        let mut input = child.stdin.take().expect("piped");
        let text = text.clone();
        std::thread::spawn(move || {
            best_effort(
                "send the detection script",
                input.write_all(text.as_bytes()),
            );
        });
    }
    let mut stderr = child.stderr.take().expect("piped");
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        best_effort(
            "read the detection errors",
            stderr.read_to_string(&mut text),
        );
        text
    });
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("piped")
        .read_to_string(&mut out)
        .map_err(|e| format!("reading {program}'s output: {e}"))?;
    let status = child
        .wait()
        .map_err(|e| format!("waiting for {program}: {e}"))?;
    let errors = errors.join().unwrap_or_default();
    parse(&out, now_ms()).map_err(|e| match errors.trim() {
        "" => format!("{e} ({program} exited with {status})"),
        why => format!("{e}: {}", tail(why, 400)),
    })
}

fn tail(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().rev().nth(max) {
        Some((at, _)) => format!("…{}", &text[at..]),
        None => text.to_owned(),
    }
}

// Caching.

/// An inventory kept in a file for a while, so commands that run often do
/// not run every version command each time. Keyed by what detection
/// depends on: `PATH`, `HOME` and the install directories.
#[derive(Clone, Debug)]
pub struct InventoryCache {
    pub path: PathBuf,
    pub ttl: Duration,
}

#[derive(Serialize, Deserialize)]
struct Cached {
    key: String,
    inventory: Inventory,
}

impl InventoryCache {
    pub fn new(path: impl Into<PathBuf>, ttl: Duration) -> InventoryCache {
        InventoryCache {
            path: path.into(),
            ttl,
        }
    }

    fn key(options: &DetectOptions) -> String {
        let mut hasher = blake3::Hasher::new();
        for part in [
            std::env::var("PATH").unwrap_or_default(),
            std::env::var("HOME").unwrap_or_default(),
            options.dirs.join(":"),
            options.timeout.as_millis().to_string(),
        ] {
            hasher.update(part.as_bytes());
            hasher.update(b"\0");
        }
        hasher.finalize().to_hex().to_string()
    }

    /// The cached inventory, if it is fresh and was taken under the same
    /// `PATH`, `HOME` and directories.
    pub fn get(&self, options: &DetectOptions) -> Option<Inventory> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let cached: Cached = serde_json::from_str(&text).ok()?;
        let age = now_ms().saturating_sub(cached.inventory.detected_at_ms);
        (cached.key == Self::key(options) && Duration::from_millis(age) < self.ttl)
            .then_some(cached.inventory)
    }

    /// Keep `inventory`.
    pub fn put(&self, options: &DetectOptions, inventory: &Inventory) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let cached = Cached {
            key: Self::key(options),
            inventory: inventory.clone(),
        };
        let tmp = self
            .path
            .with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(&cached)?)?;
        std::fs::rename(&tmp, &self.path)
    }

    /// Forget it.
    pub fn clear(&self) {
        branchyard_support::cleanup_file(&self.path);
    }

    /// The cached inventory when fresh, else a new detection here, kept.
    /// Only a detection of every harness is kept.
    pub fn local(&self, options: &DetectOptions, refresh: bool) -> Result<Inventory, String> {
        if !refresh && options.only.is_none() {
            if let Some(inventory) = self.get(options) {
                return Ok(inventory);
            }
        }
        let inventory = detect_local(options)?;
        if options.only.is_none() {
            best_effort("store the inventory", self.put(options, &inventory));
        }
        Ok(inventory)
    }
}

// Installing.

/// `[harnesses] install`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallMode {
    /// Never install or update.
    Never,
    /// Install when a person says yes on a terminal (or passes `--yes`).
    Ask,
    /// Install without asking, including on demand when the router needs
    /// a harness.
    Auto,
}

/// Whether and what may be installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallPolicy {
    pub mode: InstallMode,
    /// When not empty, only these harness IDs.
    pub allow: Vec<String>,
}

/// What the policy says about one install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Permission {
    Run,
    Ask,
    Refused(String),
}

impl InstallPolicy {
    /// The policy for `mode`: unset is `ask` on a terminal and `never`
    /// elsewhere (a server, a worker, a script).
    pub fn new(mode: Option<InstallMode>, allow: Vec<String>, interactive: bool) -> InstallPolicy {
        InstallPolicy {
            mode: mode.unwrap_or(match interactive {
                true => InstallMode::Ask,
                false => InstallMode::Never,
            }),
            allow,
        }
    }

    /// May `harness` be installed? `yes` answers an ask in advance;
    /// `interactive` says whether one can be asked.
    pub fn permits(&self, harness: &str, yes: bool, interactive: bool) -> Permission {
        if !self.allow.is_empty() && !self.allow.iter().any(|a| a == harness) {
            return Permission::Refused(format!(
                "{harness} is not in [harnesses] allow ({})",
                self.allow.join(", ")
            ));
        }
        match self.mode {
            InstallMode::Never => Permission::Refused(
                "[harnesses] install is \"never\" here (set install = \"ask\" or \"auto\" in your \
                 user configuration to allow it)"
                    .into(),
            ),
            InstallMode::Auto => Permission::Run,
            InstallMode::Ask if yes => Permission::Run,
            InstallMode::Ask if interactive => Permission::Ask,
            InstallMode::Ask => Permission::Refused(
                "[harnesses] install is \"ask\" and there is no terminal to ask on; pass --yes"
                    .into(),
            ),
        }
    }
}

/// Install or update.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallAction {
    Install,
    Update,
    Login,
}

impl fmt::Display for InstallAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            InstallAction::Install => "install",
            InstallAction::Update => "update",
            InstallAction::Login => "login",
        })
    }
}

/// A shell command's exit status (`None` when killed) and output, or why
/// it could not run.
pub type RunResult = Result<(Option<i32>, String), String>;

/// The command an install would run.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallPlan {
    pub harness: String,
    pub action: InstallAction,
    /// A shell command, from the catalog.
    pub command: String,
    /// The program it starts with: `npm`, `curl`, `brew`, ...
    pub method: String,
    /// The version it installs, when pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<String>,
    /// Why it is not pinned, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpinned: Option<String>,
}

/// The version `harness`'s default profile was checked against, if it
/// names one.
pub fn checked_version(harness: &str) -> Option<String> {
    branchyard_harness::profiles::PROFILES
        .iter()
        .find(|p| p.harness == harness)
        .and_then(|p| p.checked_against)
        .and_then(parse_version)
}

/// The programs a shell command needs: the first word of each pipeline
/// stage.
fn programs(command: &str) -> Vec<String> {
    command
        .split('|')
        .filter_map(|stage| stage.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

/// `command` with its package pinned to `version`, for package managers
/// whose `install -g PACKAGE` takes `PACKAGE@VERSION`; `None` for others.
fn pin(command: &str, version: &str) -> Option<String> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let manager = *words.first()?;
    if !matches!(manager, "npm" | "bun" | "pnpm") || command.contains('|') {
        return None;
    }
    // The first word after `install`/`i`/`add` and its flags is the package.
    let start = words
        .iter()
        .position(|w| matches!(*w, "install" | "i" | "add"))?
        + 1;
    let at = (start..words.len()).find(|&i| !words[i].starts_with('-'))?;
    let package = words[at];
    // `@scope/name@1.2` or `name@1.2`: replace a version already there.
    let bare = match package.rfind('@') {
        Some(0) | None => package,
        Some(i) => &package[..i],
    };
    let mut out: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
    out[at] = format!("{bare}@{version}");
    Some(out.join(" "))
}

/// What installing (or updating) `harness` would run on a machine with
/// `tools`: the first catalog command whose programs are all there,
/// pinned to `version` or, unpinned, to its default profile's checked
/// version where the command can take one.
pub fn plan(
    harness: &str,
    action: InstallAction,
    version: Option<&str>,
    tools: &BTreeMap<String, String>,
) -> Result<InstallPlan, String> {
    let entry = catalog::harness(harness).ok_or_else(|| {
        format!("{harness} is not in the harness catalog (by harnesses --all lists it)")
    })?;
    if entry.install.is_empty() {
        return Err(format!(
            "the catalog records no install command for {harness}{}",
            entry
                .homepage
                .as_deref()
                .map(|h| format!("; see {h}"))
                .unwrap_or_default()
        ));
    }
    let usable = entry.install.iter().find(|command| {
        programs(command)
            .iter()
            .all(|p| tools.contains_key(p) || p.contains('/'))
    });
    let Some(command) = usable else {
        let needs: Vec<String> = entry
            .install
            .iter()
            .map(|c| programs(c).join(" and "))
            .collect();
        return Err(format!(
            "no install command for {harness} can run there: they need {}",
            needs.join(", or ")
        ));
    };
    let method = programs(command).first().cloned().unwrap_or_default();
    let target = version
        .map(str::to_owned)
        .or_else(|| checked_version(harness));
    let (command, pinned, unpinned) = match &target {
        Some(v) => match pin(command, v) {
            Some(pinned) => (pinned, Some(v.clone()), None),
            None if version.is_some() => {
                return Err(format!(
                    "{harness}'s install command ({command}) cannot be pinned to a version"
                ))
            }
            None => (
                command.clone(),
                None,
                Some(format!("{method}'s installer takes no version")),
            ),
        },
        None => (
            command.clone(),
            None,
            Some("no version is recorded for it".into()),
        ),
    };
    Ok(InstallPlan {
        harness: harness.to_owned(),
        action,
        command,
        method,
        pinned,
        unpinned,
    })
}

/// How an install went.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Installed {
    pub plan: InstallPlan,
    /// The version before, when it was installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// What detection found afterwards; `None` when it is still missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<HarnessState>,
    /// The command's exit status.
    pub status: Option<i32>,
    /// The end of its output.
    pub output_tail: String,
    /// Installed, found afterwards, and at the pinned version if pinned.
    pub verified: bool,
    /// Why it is not verified, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// Run `plan` with `run` (a shell command, its output and exit status
/// returned), then detect the harness again with `detect` and say whether
/// it is there at the pinned version.
pub fn install(
    plan: &InstallPlan,
    before: Option<&HarnessState>,
    run: &mut dyn FnMut(&str) -> RunResult,
    detect: &mut dyn FnMut(&str) -> Result<Inventory, String>,
) -> Installed {
    let (status, output) = match run(&plan.command) {
        Ok(ran) => ran,
        Err(e) => (None, e),
    };
    let after = detect(&plan.harness)
        .ok()
        .and_then(|i| i.get(&plan.harness).cloned());
    let problem = if status != Some(0) {
        Some(match status {
            Some(code) => format!("{} exited with status {code}", plan.method),
            None => format!("{} did not run to the end", plan.method),
        })
    } else {
        match (&after, &plan.pinned) {
            (None, _) => Some(format!(
                "{} is still not found after the install (is the install directory on PATH?)",
                plan.harness
            )),
            (Some(after), Some(pin)) if after.version.as_deref() != Some(pin.as_str()) => {
                Some(format!(
                    "{} reports version {} after the install, not the pinned {pin}",
                    plan.harness,
                    after.version.as_deref().unwrap_or("unknown")
                ))
            }
            _ => None,
        }
    };
    Installed {
        plan: plan.clone(),
        before: before.and_then(|b| b.version.clone()),
        after,
        status,
        output_tail: tail(&output, 2000),
        verified: problem.is_none(),
        problem,
    }
}

/// Run a shell command on this machine, its output (stdout and stderr
/// together) captured.
pub fn run_local(command: &str) -> RunResult {
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("exec 2>&1; {command}"))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run /bin/sh: {e}"))?;
    Ok((
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

// The log.

/// One install, update or login, as the log records it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessEvent {
    pub at_ms: u64,
    /// The machine: `local`, `ssh://host`, ...
    pub on: String,
    pub harness: String,
    pub action: InstallAction,
    /// Who asked: `by harnesses`, `router`, ...
    pub by: String,
    /// The command run, when one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// `verified`, `failed`, `refused`, `ran`, `stored_key`, `reported`.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// An append-only JSON-lines file of [`HarnessEvent`]s.
#[derive(Clone, Debug)]
pub struct HarnessLog {
    pub path: PathBuf,
}

impl HarnessLog {
    pub fn new(path: impl Into<PathBuf>) -> HarnessLog {
        HarnessLog { path: path.into() }
    }

    /// Append `event`, creating the file (0600) and its directory.
    pub fn append(&self, event: &HarnessEvent) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        file.write_all(&line)
    }

    /// Every event, oldest first; lines that do not parse are skipped.
    pub fn read(&self) -> io::Result<Vec<HarnessEvent>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(text
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}

// The router's view.

/// Whether a harness can run on the machine a task will run on, asked by
/// the router (`RouteOptions::harnesses`) for each candidate it would
/// otherwise run there by name. It may install the harness first, when
/// policy allows.
pub trait HarnessGate: Send + Sync + fmt::Debug {
    /// `Ok` when `harness` (a catalog ID) can run; otherwise why not.
    fn check(&self, harness: &str) -> Result<(), String>;
}

/// [`HarnessGate`] over this machine's inventory, installing a missing
/// harness on demand when the policy is `auto` and allows it.
pub struct LocalGate {
    inventory: Mutex<Inventory>,
    policy: InstallPolicy,
    options: DetectOptions,
    log: Option<HarnessLog>,
    cache: Option<InventoryCache>,
    /// Say what would be installed instead of installing it.
    preview: bool,
    /// Told what is being installed, for people.
    say: Box<dyn Fn(&str) + Send + Sync>,
    /// Runs an install command; [`run_local`] unless replaced.
    runner: Box<dyn Fn(&str) -> RunResult + Send + Sync>,
}

impl fmt::Debug for LocalGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalGate")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl LocalGate {
    pub fn new(inventory: Inventory, policy: InstallPolicy, options: DetectOptions) -> LocalGate {
        LocalGate {
            inventory: Mutex::new(inventory),
            policy,
            options,
            log: None,
            cache: None,
            preview: false,
            say: Box::new(|_| {}),
            runner: Box::new(run_local),
        }
    }

    /// Record installs in `log`.
    pub fn with_log(mut self, log: HarnessLog) -> LocalGate {
        self.log = Some(log);
        self
    }

    /// Drop `cache` after an install, so the next command detects again.
    pub fn with_cache(mut self, cache: InventoryCache) -> LocalGate {
        self.cache = Some(cache);
        self
    }

    /// Install nothing: a harness that would be installed on demand is
    /// excluded, saying so, as `by fleet route` shows a route.
    pub fn preview(mut self) -> LocalGate {
        self.preview = true;
        self
    }

    /// Tell people what is installed on demand through `say`.
    pub fn with_messages(mut self, say: impl Fn(&str) + Send + Sync + 'static) -> LocalGate {
        self.say = Box::new(say);
        self
    }

    /// Run install commands with `runner` instead of `/bin/sh -c`.
    pub fn with_runner(
        mut self,
        runner: impl Fn(&str) -> RunResult + Send + Sync + 'static,
    ) -> LocalGate {
        self.runner = Box::new(runner);
        self
    }

    /// The inventory as it stands, after any installs.
    pub fn inventory(&self) -> Inventory {
        self.inventory.lock_recovering("inventory").clone()
    }

    pub fn into_arc(self) -> Arc<dyn HarnessGate> {
        Arc::new(self)
    }
}

impl HarnessGate for LocalGate {
    fn check(&self, harness: &str) -> Result<(), String> {
        let mut inventory = self.inventory.lock_recovering("inventory");
        if catalog::harness(harness).is_none() || !inventory.checked(harness) {
            // Not something detection knows: the PATH check decides.
            return Ok(());
        }
        if inventory.get(harness).is_some() {
            return inventory.ready(harness).map(|_| ());
        }
        let missing = format!("{harness} is not installed on this machine");
        if self.policy.mode != InstallMode::Auto {
            return Err(format!(
                "{missing}; install it with `by harnesses install {harness}` (the router \
                 installs on demand only with [harnesses] install = \"auto\")"
            ));
        }
        if let Permission::Refused(why) = self.policy.permits(harness, false, false) {
            return Err(format!("{missing}, and {why}"));
        }
        let plan = plan(harness, InstallAction::Install, None, &inventory.tools)
            .map_err(|why| format!("{missing}, and it cannot be installed: {why}"))?;
        if self.preview {
            return Err(format!(
                "{missing}; a routed run would install it first ([harnesses] install = \"auto\"): \
                 {}",
                plan.command
            ));
        }
        (self.say)(&format!(
            "installing {harness} on demand ([harnesses] install = \"auto\"): {}",
            plan.command
        ));
        let mut options = self.options.clone();
        options.only = Some(vec![harness.to_owned()]);
        let result = install(
            &plan,
            None,
            &mut |command| (self.runner)(command),
            &mut |_| detect_local(&options),
        );
        if let Some(log) = &self.log {
            best_effort(
                "log the harness install",
                log.append(&HarnessEvent {
                    at_ms: now_ms(),
                    on: "local".into(),
                    harness: harness.to_owned(),
                    action: InstallAction::Install,
                    by: "router".into(),
                    command: Some(plan.command.clone()),
                    outcome: match result.verified {
                        true => "verified".into(),
                        false => "failed".into(),
                    },
                    version_before: None,
                    version_after: result.after.as_ref().and_then(|a| a.version.clone()),
                    detail: result.problem.clone(),
                }),
            );
        }
        if let Some(cache) = &self.cache {
            cache.clear();
        }
        inventory.update(harness, result.after.clone());
        match (result.verified, &result.after) {
            (true, Some(after)) => {
                (self.say)(&format!(
                    "installed {harness} {}",
                    after.version.as_deref().unwrap_or("(version unknown)")
                ));
                inventory.ready(harness).map(|_| ())
            }
            _ => Err(format!(
                "{missing}, and installing it on demand failed: {}",
                result.problem.unwrap_or_default()
            )),
        }
    }
}

/// The catalog harness a harness or profile ID names.
pub fn harness_of(id: &str) -> Option<&'static str> {
    if let Some(entry) = catalog::harness(id) {
        return Some(entry.id.as_str());
    }
    branchyard_harness::profiles::PROFILES
        .iter()
        .find(|p| p.id == id)
        .map(|p| p.harness)
}

/// Where Branchyard keeps per-user harness state beside `config_dir`: the
/// inventory cache and the event log.
pub fn user_paths(config_dir: &Path) -> (PathBuf, PathBuf) {
    (
        config_dir.join("harness-inventory.json"),
        config_dir.join("harness-events.jsonl"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_from_what_harnesses_print() {
        for (line, want) in [
            ("2.1.283 (Claude Code)", Some("2.1.283")),
            ("codex-cli 0.157.1", Some("0.157.1")),
            ("v1.2.11", Some("1.2.11")),
            ("pi version 0.87.1-beta.2 (linux)", Some("0.87.1-beta.2")),
            ("tool 3.0", Some("3.0")),
            ("no version here", None),
            ("build 7", None),
        ] {
            assert_eq!(parse_version(line).as_deref(), want, "{line}");
        }
    }

    #[test]
    fn package_manager_commands_are_pinned_and_others_are_not() {
        assert_eq!(
            pin("npm install -g @openai/codex", "0.157.1").as_deref(),
            Some("npm install -g @openai/codex@0.157.1")
        );
        assert_eq!(
            pin(
                "npm install -g @earendil-works/pi-coding-agent --ignore-scripts",
                "0.87.1"
            )
            .as_deref(),
            Some("npm install -g @earendil-works/pi-coding-agent@0.87.1 --ignore-scripts")
        );
        assert_eq!(
            pin("npm install -g @xai-official/grok@latest", "1.0").as_deref(),
            Some("npm install -g @xai-official/grok@1.0")
        );
        assert_eq!(pin("curl -fsSL https://x/install.sh | bash", "1.0"), None);
        assert_eq!(pin("brew install --cask codex", "1.0"), None);
    }

    #[test]
    fn plans_pick_a_command_the_machine_can_run() {
        let tools = |names: &[&str]| -> BTreeMap<String, String> {
            names
                .iter()
                .map(|n| ((*n).to_owned(), format!("/bin/{n}")))
                .collect()
        };
        let npm = plan("codex", InstallAction::Install, None, &tools(&["npm"])).unwrap();
        assert_eq!(npm.command, "npm install -g @openai/codex@0.157.1");
        assert_eq!(npm.pinned.as_deref(), Some("0.157.1"));
        let curl = plan(
            "codex",
            InstallAction::Install,
            None,
            &tools(&["curl", "sh"]),
        )
        .unwrap();
        assert_eq!(curl.method, "curl");
        assert_eq!(curl.pinned, None);
        assert!(curl.unpinned.unwrap().contains("takes no version"));
        let none = plan("codex", InstallAction::Install, None, &tools(&[])).unwrap_err();
        assert!(none.contains("npm, or curl and sh, or brew"), "{none}");
        assert!(plan("trae", InstallAction::Install, None, &tools(&["npm"]))
            .unwrap_err()
            .contains("no install command"));
        let explicit = plan(
            "codex",
            InstallAction::Update,
            Some("0.200.0"),
            &tools(&["npm"]),
        );
        assert_eq!(
            explicit.unwrap().command,
            "npm install -g @openai/codex@0.200.0"
        );
        assert!(plan(
            "codex",
            InstallAction::Install,
            Some("1.0"),
            &tools(&["curl", "sh"])
        )
        .unwrap_err()
        .contains("cannot be pinned"));
    }

    #[test]
    fn policy_never_ask_auto_and_the_allowlist() {
        let never = InstallPolicy::new(None, vec![], false);
        assert_eq!(never.mode, InstallMode::Never);
        assert!(matches!(
            never.permits("codex", true, false),
            Permission::Refused(_)
        ));
        let ask = InstallPolicy::new(None, vec![], true);
        assert_eq!(ask.mode, InstallMode::Ask);
        assert_eq!(ask.permits("codex", false, true), Permission::Ask);
        assert_eq!(ask.permits("codex", true, false), Permission::Run);
        assert!(matches!(
            ask.permits("codex", false, false),
            Permission::Refused(_)
        ));
        let auto = InstallPolicy::new(Some(InstallMode::Auto), vec!["codex".into()], false);
        assert_eq!(auto.permits("codex", false, false), Permission::Run);
        let Permission::Refused(why) = auto.permits("goose", false, false) else {
            panic!("goose is not allowed");
        };
        assert!(why.contains("not in [harnesses] allow (codex)"), "{why}");
    }

    #[test]
    fn the_script_tests_variables_without_reading_them() {
        let text = script(&DetectOptions {
            only: Some(vec!["codex".into()]),
            ..DetectOptions::default()
        });
        assert!(text.contains("[ -n \"${OPENAI_API_KEY:-}\" ] && echo 'env codex OPENAI_API_KEY'"));
        assert!(text.contains(" login status"));
        assert!(!text.contains("echo \"$OPENAI_API_KEY"));
        assert!(!text.contains("checked claude-code"));
        assert!(text.contains("\"$HOME\"/.local/bin"));
    }

    /// `codex login status` says logged in and CODEX_API_KEY is set: the
    /// login is verified, and the detail says the key is what Codex uses.
    #[test]
    fn a_key_variable_beside_a_verified_codex_login_is_named_as_what_codex_uses() {
        let output = "by-inventory 1\nhost box\nos Linux\n\
            checked codex\n\
            found codex 1 /bin/codex\nversion codex 0\nversion-out codex codex-cli 0.157.1\n\
            status codex 0\nstatus-out codex Logged in using ChatGPT\n\
            file codex ~/.codex/auth.json\nenv codex CODEX_API_KEY\nend\n";
        let inventory = parse(output, 7).unwrap();
        let codex = inventory.get("codex").unwrap();
        assert_eq!(codex.login.state, LoginState::LoggedIn);
        assert_eq!(codex.login.evidence, Evidence::Verified);
        assert_eq!(
            codex.login.detail,
            "codex login status: Logged in using ChatGPT; CODEX_API_KEY is set, and Codex uses \
             it instead of the login (unset it, or run isolated, to use the login)"
        );
        // Without the variable the detail is the status alone.
        let alone = output.replace("env codex CODEX_API_KEY\n", "");
        let codex = parse(&alone, 7).unwrap();
        assert_eq!(
            codex.get("codex").unwrap().login.detail,
            "codex login status: Logged in using ChatGPT"
        );
    }

    /// Codex prints a warning naming its home before the answer; the login
    /// detail is the answer, not the warning cut mid-path.
    #[test]
    fn a_status_command_detail_is_its_last_line() {
        let output = "by-inventory 1\nhost box\nos Linux\nchecked codex\n\
            found codex 1 /bin/codex\nversion codex 0\nversion-out codex codex-cli 0.157.1\n\
            status codex 1\n\
            status-out codex WARNING: proceeding, even though we could not update PATH: Refusing to create helper binaries under temporary dir \"/tmp/x\" (codex_home: AbsolutePathBuf(\"/tmp/claude-0/-home-user-branchyard/f2a73\n\
            status-out codex Not logged in\nend\n";
        let codex = parse(output, 7).unwrap();
        let login = &codex.get("codex").unwrap().login;
        assert_eq!(login.state, LoginState::LoggedOut);
        assert_eq!(login.detail, "codex login status: Not logged in");
    }

    #[test]
    fn output_is_parsed_into_states() {
        let output = "by-inventory 1\nhost box\nos Linux\ntool npm /usr/bin/npm\n\
            checked claude-code\nchecked codex\nchecked goose\n\
            found codex 1 /bin/codex\nversion codex 0\nversion-out codex codex-cli 0.157.1\n\
            status codex 1\nstatus-out codex Not logged in\n\
            found claude-code 0 /home/me/.local/bin/claude\nversion claude-code timeout\n\
            file claude-code ~/.claude/.credentials.json\nend\n";
        let inventory = parse(output, 7).unwrap();
        assert_eq!(inventory.host, "box");
        assert_eq!(inventory.tools["npm"], "/usr/bin/npm");
        let ids: Vec<&str> = inventory.harnesses.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["claude-code", "codex"]);
        let codex = inventory.get("codex").unwrap();
        assert_eq!(codex.version.as_deref(), Some("0.157.1"));
        assert_eq!(codex.login.state, LoginState::LoggedOut);
        assert_eq!(codex.login.evidence, Evidence::Verified);
        let claude = inventory.get("claude-code").unwrap();
        assert!(!claude.on_path);
        assert_eq!(
            claude.version_note.as_deref(),
            Some("its version command timed out")
        );
        assert_eq!(claude.login.state, LoginState::LoggedIn);
        assert_eq!(claude.login.evidence, Evidence::Likely);
        assert!(inventory
            .ready("goose")
            .unwrap_err()
            .contains("not installed on box"));
        assert!(inventory
            .ready("codex")
            .unwrap_err()
            .contains("verified logged out"));
        assert!(inventory
            .ready("claude-code")
            .unwrap_err()
            .contains("not on PATH"));
        assert!(inventory
            .ready("amp")
            .unwrap_err()
            .contains("not looked for"));
        assert!(inventory.labels().is_empty());
        assert!(parse("hello\n", 0).is_err());
        assert!(parse("by-inventory 1\nhost x\n", 0)
            .unwrap_err()
            .contains("stopped before"));
    }

    /// What detection makes of `id` found on a Linux machine, given the
    /// `env`, `file` and `fact` lines the script printed for it.
    fn found(id: &str, lines: &[&str]) -> HarnessState {
        let mut output = format!(
            "by-inventory 1\nhost box\nos Linux\nchecked {id}\nfound {id} 1 /bin/{id}\n\
             version {id} 0\nversion-out {id} 1.0.0\n"
        );
        for line in lines {
            output.push_str(&format!("{line}\n"));
        }
        output.push_str("end\n");
        parse(&output, 0).unwrap().get(id).unwrap().clone()
    }

    const AGY_SETTINGS: &str = "~/.gemini/antigravity-cli/settings.json";

    #[test]
    fn antigravity_uses_a_gemini_key_only_with_the_gemini_provider() {
        // agy 1.3.3 reads GEMINI_API_KEY alone, and only with modelProvider
        // gemini in its settings.
        let with = found(
            "antigravity",
            &[
                "env antigravity GEMINI_API_KEY",
                "fact antigravity model-provider-gemini",
            ],
        );
        assert_eq!(with.login.state, LoginState::LoggedIn);
        assert_eq!(with.login.evidence, Evidence::Likely);
        assert_eq!(
            with.login.detail,
            "GEMINI_API_KEY is set and settings.json selects the gemini provider \
             (an API key; not checked)"
        );
        // The key without the setting: agy would ask for a Google sign-in,
        // but isolated runs set the provider, so it can still run.
        let without = found("antigravity", &["env antigravity GEMINI_API_KEY"]);
        assert_eq!(without.login.state, LoginState::LoggedIn);
        assert_eq!(without.login.evidence, Evidence::Likely);
        assert_eq!(
            without.login.detail,
            format!(
                "GEMINI_API_KEY is set but {AGY_SETTINGS} does not set modelProvider gemini: \
                 agy will ask for a Google sign-in; Branchyard sets it in isolated runs"
            )
        );
        assert!(without.ready().is_ok());
        // A Google sign-in works whatever the key and the setting say.
        let token = found(
            "antigravity",
            &[
                "env antigravity GEMINI_API_KEY",
                "file antigravity ~/.gemini/antigravity-cli/antigravity-oauth-token",
            ],
        );
        assert_eq!(
            token.login.detail,
            "~/.gemini/antigravity-cli/antigravity-oauth-token exists"
        );
        let agy_token = found("antigravity", &["env antigravity AGY_TOKEN"]);
        assert_eq!(
            agy_token.login.detail,
            "AGY_TOKEN is set (an API key; not checked)"
        );
        let nothing = found("antigravity", &[]);
        assert_eq!(nothing.login.state, LoginState::LoggedOut);
        assert_eq!(nothing.login.evidence, Evidence::Likely);
        assert!(
            nothing
                .login
                .detail
                .starts_with("no ~/.gemini/antigravity-cli/antigravity-oauth-token and no GEMINI_API_KEY or AGY_TOKEN;"),
            "{}",
            nothing.login.detail
        );
        assert!(!nothing.login.detail.contains("GOOGLE_API_KEY"));
        assert!(nothing
            .login
            .detail
            .ends_with("a GEMINI_API_KEY also needs modelProvider gemini in ~/.gemini/antigravity-cli/settings.json"));
        // GOOGLE_API_KEY has no effect on agy, so the script does not test it.
        let text = script(&DetectOptions {
            only: Some(vec!["antigravity".into()]),
            ..DetectOptions::default()
        });
        assert!(text.contains("echo 'env antigravity GEMINI_API_KEY'"));
        assert!(text.contains("echo 'env antigravity AGY_TOKEN'"));
        assert!(!text.contains("GOOGLE_API_KEY"));
        assert!(text.contains(
            "grep -Eq '\"modelProvider\"[[:space:]]*:[[:space:]]*\"gemini\"' \
             \"$HOME/.gemini/antigravity-cli/settings.json\""
        ));
    }

    #[test]
    fn oh_my_pi_is_logged_in_by_a_key_a_stored_login_or_an_env_file() {
        // The catalog's key variables, through the generic path.
        let keyed = found("oh-my-pi", &["env oh-my-pi GEMINI_API_KEY"]);
        assert_eq!(keyed.login.state, LoginState::LoggedIn);
        assert_eq!(keyed.login.evidence, Evidence::Likely);
        assert_eq!(
            keyed.login.detail,
            "GEMINI_API_KEY is set (an API key; not checked)"
        );
        // Logins `omp login` stored, by provider name.
        let stored = found(
            "oh-my-pi",
            &[
                "fact oh-my-pi bun 1.4.3",
                "fact oh-my-pi login anthropic",
                "fact oh-my-pi login openai-codex",
            ],
        );
        assert_eq!(stored.login.state, LoginState::LoggedIn);
        assert_eq!(stored.login.evidence, Evidence::Likely);
        assert_eq!(
            stored.login.detail,
            "omp login stored for anthropic, openai-codex in ~/.omp/agent/agent.db (not checked)"
        );
        // A key in a .env file omp loads: the name, never the line.
        let env_file = found(
            "oh-my-pi",
            &["fact oh-my-pi env-file OPENAI_API_KEY ~/.omp/agent/.env"],
        );
        assert_eq!(env_file.login.state, LoginState::LoggedIn);
        assert_eq!(
            env_file.login.detail,
            "OPENAI_API_KEY is set in ~/.omp/agent/.env, which omp loads (an API key; not \
             checked)"
        );
        // Nothing: logged out, with what to do.
        let nothing = found("oh-my-pi", &["fact oh-my-pi bun 1.4.3"]);
        assert_eq!(nothing.login.state, LoginState::LoggedOut);
        assert_eq!(nothing.login.evidence, Evidence::Likely);
        assert_eq!(
            nothing.login.detail,
            "no login stored in ~/.omp/agent/agent.db and none of GEMINI_API_KEY, \
             OPENAI_API_KEY, ANTHROPIC_API_KEY, OPENROUTER_API_KEY, ZAI_API_KEY is set; set one, \
             or run omp login"
        );
        // The database is there but nothing on the machine can read it.
        let unread = found("oh-my-pi", &["fact oh-my-pi login-store unread"]);
        assert_eq!(unread.login.state, LoginState::Unknown);
        assert_eq!(unread.login.evidence, Evidence::None);
        assert!(
            unread.login.detail.starts_with(
                "~/.omp/agent/agent.db may hold a login, which needs sqlite3 or python3"
            ),
            "{}",
            unread.login.detail
        );
        // The script reads provider names from the database and variable
        // names from the .env files, and tests the key variables.
        let text = script(&DetectOptions {
            only: Some(vec!["oh-my-pi".into()]),
            ..DetectOptions::default()
        });
        assert!(text.contains("echo 'env oh-my-pi GEMINI_API_KEY'"));
        assert!(text.contains("echo 'env oh-my-pi OPENROUTER_API_KEY'"));
        assert!(text.contains("select distinct provider from auth_credentials"));
        assert!(text.contains("echo 'fact oh-my-pi login-store unread'"));
        assert!(text.contains("echo 'fact oh-my-pi runs-on-bun'"));
        assert!(!text.contains("select data") && !text.contains("select *"));
    }

    #[test]
    fn oh_my_pi_that_cannot_start_names_the_bun_it_needs() {
        let state = |facts: &[&str]| {
            let mut output = "by-inventory 1\nhost box\nos Linux\nchecked oh-my-pi\n\
                found oh-my-pi 1 /usr/local/bin/omp\nversion oh-my-pi 1\n\
                version-out oh-my-pi SyntaxError: Unexpected identifier 'u'\n"
                .to_owned();
            for fact in facts {
                output.push_str(&format!("fact oh-my-pi {fact}\n"));
            }
            output.push_str("end\n");
            parse(&output, 0).unwrap().get("oh-my-pi").unwrap().clone()
        };
        let old = state(&["runs-on-bun", "bun 1.3.11"]);
        assert_eq!(old.version, None);
        assert_eq!(
            old.version_note.as_deref(),
            Some(
                "its version command exited with status 1; omp runs on Bun 1.3.14 or newer and \
                 the bun on PATH is 1.3.11: `curl -fsSL https://omp.sh/install | sh` installs \
                 or upgrades it"
            )
        );
        let missing = state(&["runs-on-bun", "bun missing"]);
        assert!(missing
            .version_note
            .as_deref()
            .unwrap()
            .contains("Bun 1.3.14 or newer and no bun is on PATH"));
        // A new enough Bun, or a prebuilt binary: the failure is something
        // else, and the note says only what happened.
        for facts in [&["runs-on-bun", "bun 1.4.3"][..], &["bun 1.3.11"][..]] {
            assert_eq!(
                state(facts).version_note.as_deref(),
                Some("its version command exited with status 1"),
                "{facts:?}"
            );
        }
        assert!(bun_too_old("1.3.11") && bun_too_old("1.2") && bun_too_old("0.9.9"));
        assert!(!bun_too_old("1.3.14") && !bun_too_old("1.4.3") && !bun_too_old("2.0.0"));
        // A working omp has a version and no note.
        let fine = found(
            "oh-my-pi",
            &["fact oh-my-pi runs-on-bun", "fact oh-my-pi bun 1.4.3"],
        );
        assert_eq!(fine.version.as_deref(), Some("1.0.0"));
        assert_eq!(fine.version_note, None);
    }

    #[test]
    fn deepseek_harness_is_logged_in_by_a_key_or_a_configured_provider() {
        let version = found("deepseek-harness", &[]);
        assert_eq!(version.version.as_deref(), Some("1.0.0"));
        assert_eq!(parse_version("0.2.0-rc.2").as_deref(), Some("0.2.0-rc.2"));
        for (line, shown) in [
            (
                "fact deepseek-harness providers cordis.patch.yml",
                "~/.dsh/cordis.patch.yml",
            ),
            (
                "fact deepseek-harness providers profiles/acp/cordis.patch.yml",
                "~/.dsh/profiles/acp/cordis.patch.yml",
            ),
        ] {
            let configured = found("deepseek-harness", &[line]);
            assert_eq!(configured.login.state, LoginState::LoggedIn, "{line}");
            assert_eq!(configured.login.evidence, Evidence::Likely);
            assert_eq!(
                configured.login.detail,
                format!("{shown} configures a provider (its key variable is not checked)")
            );
        }
        // The shipped provider's key, and a key variable a config names.
        for var in ["DEEPSEEK_API_KEY", "MY_PROXY_KEY"] {
            let keyed = found(
                "deepseek-harness",
                &[&format!("env deepseek-harness {var}")],
            );
            assert_eq!(keyed.login.state, LoginState::LoggedIn, "{var}");
            assert_eq!(keyed.login.evidence, Evidence::Likely);
            assert_eq!(
                keyed.login.detail,
                format!("{var} is set (an API key; not checked)")
            );
        }
        // Neither: logged out, and the detail says how to configure it.
        let none = found("deepseek-harness", &[]);
        assert_eq!(none.login.state, LoginState::LoggedOut);
        assert_eq!(none.login.evidence, Evidence::Likely);
        for want in [
            "~/.dsh/cordis.patch.yml",
            "~/.dsh/profiles/acp/cordis.patch.yml",
            "DEEPSEEK_API_KEY",
            "deepseek-official",
            "llm-pi-ai",
            "dsh --profile acp --dump-config",
        ] {
            assert!(
                none.login.detail.contains(want),
                "{want}: {}",
                none.login.detail
            );
        }
        let why = none.ready().unwrap_err();
        assert!(why.contains("likely logged out"), "{why}");
        assert!(why.contains("by harnesses login deepseek-harness"), "{why}");
        // DEEPSEEK_API_KEY is the catalog's key variable, found by name.
        assert_eq!(key_variables("deepseek-harness"), ["DEEPSEEK_API_KEY"]);
        let text = script(&DetectOptions {
            only: Some(vec!["deepseek-harness".into()]),
            ..DetectOptions::default()
        });
        assert!(text.contains("echo 'env deepseek-harness DEEPSEEK_API_KEY'"));
        assert!(text.contains("${DSH_HOME:-$HOME/.dsh}"));
        for file in DSH_CONFIGS {
            let relative = file.trim_start_matches("~/.dsh/");
            assert!(text.contains(relative), "{relative}");
        }
        assert!(!text.contains("cat "), "a config file must not be printed");
    }

    #[test]
    fn gemini_cli_notes_what_headless_runs_need() {
        let keyed = found("gemini-cli", &["env gemini-cli GEMINI_API_KEY"]);
        assert_eq!(keyed.login.state, LoginState::LoggedIn);
        assert_eq!(keyed.login.evidence, Evidence::Likely);
        assert_eq!(
            keyed.login.detail,
            "GEMINI_API_KEY is set (an API key; not checked); headless runs (gemini -p) \
             refuse an untrusted workspace: set GEMINI_CLI_TRUST_WORKSPACE=true or pass \
             --skip-trust"
        );
        let trusted = found(
            "gemini-cli",
            &[
                "file gemini-cli ~/.gemini/oauth_creds.json",
                "fact gemini-cli trust-workspace",
            ],
        );
        assert_eq!(
            trusted.login.detail,
            "~/.gemini/oauth_creds.json exists; GEMINI_CLI_TRUST_WORKSPACE=true is set, so \
             headless runs trust the workspace"
        );
        // The note is for a harness that can run; a logged-out one says what
        // it is missing.
        let out = found("gemini-cli", &[]);
        assert_eq!(out.login.state, LoginState::LoggedOut);
        assert_eq!(
            out.login.detail,
            "no ~/.gemini/oauth_creds.json and no GEMINI_API_KEY or GOOGLE_API_KEY"
        );
        let text = script(&DetectOptions {
            only: Some(vec!["gemini-cli".into()]),
            ..DetectOptions::default()
        });
        assert!(text.contains("echo 'env gemini-cli GEMINI_API_KEY'"));
        assert!(text.contains("echo 'env gemini-cli GOOGLE_API_KEY'"));
        assert!(text.contains("[ \"${GEMINI_CLI_TRUST_WORKSPACE:-}\" = true ]"));
    }

    #[test]
    fn profile_ids_name_their_harness() {
        assert_eq!(harness_of("codex"), Some("codex"));
        assert_eq!(harness_of("claude-code-acp"), Some("claude-code"));
        assert_eq!(harness_of("nope"), None);
        assert_eq!(checked_version("codex").as_deref(), Some("0.157.1"));
        assert_eq!(checked_version("amp"), None);
    }
}
