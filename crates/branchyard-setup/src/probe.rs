//! What setup detects about the machine and the repository, through an
//! injected [`Probe`], so the engine itself does no I/O and tests use a
//! fake. A probe reports whether a variable is set, never its value; the
//! hash of an existing token file, never the token.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Variables whose presence setup reports, by name only: the credentials
/// harnesses read (`DEEPSEEK_API_KEY` for DeepSeek Harness, `GEMINI_API_KEY` for
/// Gemini CLI and Antigravity, `ZAI_API_KEY` for Oh My Pi's Z.ai provider),
/// and Branchyard's own.
pub const KNOWN_VARIABLES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "BRANCHYARD_REMOTE",
    "BRANCHYARD_TOKEN_FILE",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CODEX_API_KEY",
    "COPILOT_GITHUB_TOKEN",
    "DATABASE_URL",
    "DEEPSEEK_API_KEY",
    "GEMINI_API_KEY",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GOOGLE_API_KEY",
    "GOOGLE_CLOUD_PROJECT",
    "OPENAI_API_KEY",
    "ZAI_API_KEY",
];

/// Tools whose presence and version setup reports.
pub const KNOWN_TOOLS: &[&str] = &["docker", "git", "pg_isready", "psql", "python3"];

/// Files at a repository root that suggest a check command, in order.
pub const CHECK_MARKERS: &[(&str, &str)] = &[
    ("Cargo.toml", "cargo test"),
    ("package.json", "npm test"),
    ("pyproject.toml", "pytest"),
    ("go.mod", "go test ./..."),
    ("Makefile", "make test"),
];

/// Untracked files at a repository root that `[workspace] copy` suggests
/// carrying into each worktree, when present.
pub const ENV_FILES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.development",
    ".env.development.local",
    ".env.test",
    ".env.test.local",
];

/// Docker Compose files, in the order `docker compose` looks for them.
pub const COMPOSE_FILES: &[&str] = &[
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// The Compose project a branch's services run under: one per branch, so
/// two branches' stacks never collide (a project name takes no `.`).
const COMPOSE_PROJECT: &str = "\"by-$(printf %s \"$BRANCHYARD_BRANCH\" | tr . -)\"";

/// The files a prepared environment is keyed by when `inputs` names none
/// (`branchyard::environments::DEFAULT_INPUTS`), listed here so a
/// suggestion that names inputs keeps them.
pub const ENV_INPUTS: &[&str] = &[
    "package.json",
    "pnpm-lock.yaml",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "Cargo.lock",
    "rust-toolchain.toml",
    "pyproject.toml",
    "uv.lock",
    "poetry.lock",
    "Pipfile.lock",
    "requirements.txt",
    "go.mod",
    "go.sum",
    "Gemfile.lock",
    "composer.lock",
    "mix.lock",
    "flake.lock",
    ".tool-versions",
    ".nvmrc",
    ".node-version",
    ".python-version",
];

/// The `package.json` script that starts a development server: `dev`,
/// else `start`; `None` when it has neither, or the text is not JSON.
fn server_script(package_json: &str) -> Option<&'static str> {
    let package: serde_json::Value = serde_json::from_str(package_json).ok()?;
    let scripts = package.get("scripts")?.as_object()?;
    ["dev", "start"]
        .into_iter()
        .find(|name| scripts.get(*name).is_some_and(serde_json::Value::is_string))
}

/// What the files at a repository root suggest for `[workspace]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceFacts {
    /// The files that suggested it, such as `pnpm-lock.yaml`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub found: Vec<String>,
    /// Untracked-looking files to copy: those of [`ENV_FILES`] present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy: Vec<String>,
    /// Install commands, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup: Vec<String>,
    /// A development server on the branch's port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub teardown: Vec<String>,
    /// A shell snippet run before each turn and script, from emdash's
    /// `shellSetup`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_setup: Option<String>,
    /// Variables the scripts and the harness get, from a devcontainer's
    /// `containerEnv` and `remoteEnv`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Setup once per environment key (docs/environments.md), as a
    /// devcontainer is built once.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prepare: bool,
    /// Files that key the environment, besides the lockfiles present:
    /// the devcontainer file itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// What another tool's committed configuration (`.emdash.json`,
    /// `orca.yaml`, `.superset/config.json`, `.conductor/settings.toml`,
    /// `devcontainer.json`) could not carry over, and why; see
    /// [`crate::import`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl WorkspaceFacts {
    pub fn detect(probe: &dyn Probe) -> WorkspaceFacts {
        let mut facts = WorkspaceFacts::default();
        let mut found = |file: &str| {
            let here = probe.exists(file);
            if here {
                facts.found.push(file.to_owned());
            }
            here
        };
        // JavaScript: the lockfile names the package manager, and
        // package.json's scripts say whether there is a server to run.
        let node = [
            ("pnpm-lock.yaml", "pnpm install --frozen-lockfile", "pnpm"),
            ("yarn.lock", "yarn install --frozen-lockfile", "yarn"),
            ("bun.lock", "bun install --frozen-lockfile", "bun run"),
            ("bun.lockb", "bun install --frozen-lockfile", "bun run"),
            ("package-lock.json", "npm ci", "npm run"),
        ]
        .into_iter()
        .find(|(lock, _, _)| found(lock))
        .map(|(_, install, runner)| (install, runner));
        let mut setup = Vec::new();
        let mut run = None;
        if found("package.json") {
            let (install, runner) = node.unwrap_or(("npm install", "npm run"));
            setup.push(install.to_owned());
            // Only a script that exists: a run script for a missing one
            // fails on the first worktree, after it was trusted.
            run = probe
                .read("package.json")
                .and_then(|text| server_script(&text))
                .map(|script| format!("PORT=$BRANCHYARD_PORT {runner} {script}"));
        }
        if found("Cargo.toml") {
            setup.push("cargo fetch".to_owned());
        }
        if found("uv.lock") {
            setup.push("uv sync".to_owned());
        } else if found("poetry.lock") {
            setup.push("poetry install".to_owned());
        } else if found("pyproject.toml") {
            setup.push("python3 -m venv .venv && .venv/bin/pip install -e .".to_owned());
        }
        if found("go.mod") {
            setup.push("go mod download".to_owned());
        }
        let mut teardown = Vec::new();
        if COMPOSE_FILES.iter().any(|file| found(file)) {
            setup.push(format!("docker compose -p {COMPOSE_PROJECT} up -d"));
            teardown.push(format!("docker compose -p {COMPOSE_PROJECT} down"));
        }
        let copy = ENV_FILES
            .iter()
            .filter(|file| probe.exists(file))
            .map(|file| (*file).to_owned())
            .collect();
        facts.copy = copy;
        facts.setup = setup;
        facts.run = run;
        facts.teardown = teardown;
        facts.import(probe);
        facts
    }

    /// Prefer what another worktree tool's committed configuration says
    /// over what the lockfiles suggest: the first file (in
    /// [`crate::import::IMPORTERS`] order) that gives setup, a run script,
    /// teardown or variables decides it, every file's copy globs are
    /// added, and a file that keys a prepared environment turns `prepare`
    /// on with its inputs.
    fn import(&mut self, probe: &dyn Probe) {
        let (imported, notes) = crate::import::detect(probe);
        self.notes = notes;
        let mut setup = None;
        let mut run = None;
        let mut teardown = None;
        let mut env = None;
        for found in &imported {
            if found.is_empty() {
                continue;
            }
            self.found.push(found.file.to_owned());
            if env.is_none() && !found.env.is_empty() {
                env = Some(found.env.clone());
            }
            if found.prepare {
                self.prepare = true;
                for input in &found.inputs {
                    if !self.inputs.contains(input) {
                        self.inputs.push(input.clone());
                    }
                }
            }
            for glob in &found.copy {
                let valid = crate::config::check_copy_glob(glob).is_ok();
                if !valid {
                    self.notes.push(format!(
                        "{}: copy {glob:?} is not a [workspace] copy glob",
                        found.file
                    ));
                } else if !self.copy.contains(glob) {
                    self.copy.push(glob.clone());
                }
            }
            if setup.is_none() && !found.setup.is_empty() {
                setup = Some(found.setup.clone());
            }
            if run.is_none() {
                run = found.run.clone();
            }
            if teardown.is_none() && !found.teardown.is_empty() {
                teardown = Some(found.teardown.clone());
            }
            if self.shell_setup.is_none() {
                self.shell_setup = found.shell_setup.clone();
            }
        }
        if let Some(setup) = setup {
            self.setup = setup;
        }
        if let Some(run) = run {
            self.run = Some(run);
        }
        if let Some(teardown) = teardown {
            self.teardown = teardown;
        }
        if let Some(env) = env {
            self.env = env;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.copy.is_empty() && self.setup.is_empty() && self.run.is_none()
    }
}

/// The Claude Code plugin manifest in a Branchyard checkout.
pub const PLUGIN_MANIFEST: &str = "plugins/branchyard/.claude-plugin/plugin.json";

/// A harness profile's default, and whether it is installed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessFact {
    pub harness: String,
    pub profile: String,
    /// The executable was found on `PATH`.
    pub installed: bool,
    /// The first line of `--version`, when it answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Live qualification, such as "9/9 on 2.1.283".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualification: Option<String>,
    /// Whether it is logged in, as `by harnesses` tells it, when it is
    /// installed and the machine's inventory was read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<HarnessLogin>,
}

/// A harness's login as the inventory reads it (docs/harness-lifecycle.md):
/// from its own status command, or from key variables and credential
/// files by name. Never a credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessLogin {
    /// `logged_in`, `logged_out` or `unknown`.
    pub state: String,
    /// `verified` (its status command said so), `likely` (inferred), or
    /// `none`.
    pub evidence: String,
    /// What decided it: `codex login status: Logged in using ChatGPT`.
    pub detail: String,
}

impl HarnessLogin {
    /// Logged in, likely or verified: what a routing candidate needs.
    pub fn logged_in(&self) -> bool {
        self.state == "logged_in"
    }
}

/// A login a harness keeps in a file, found in the home directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LoginFile {
    /// The harness that keeps it.
    pub harness: String,
    /// The file, under `~/`: `~/.codex/auth.json`.
    pub file: String,
    /// The secret Branchyard provisions its content as into a private home
    /// (`CODEX_AUTH`); `None` when no provisioner takes this file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// A non-secret label read from it, as `key value`: Codex's
    /// `auth_mode chatgpt`, Antigravity's `modelProvider gemini`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Files a login lives in beyond the provisioners' `auth-file` methods,
/// and the label key read from each: `(harness, file under ~/, label)`.
/// Antigravity's are found and shown, not provisioned: it takes
/// `GEMINI_API_KEY`.
const OTHER_LOGIN_FILES: &[(&str, &str, Option<&str>)] = &[
    (
        "antigravity",
        ".gemini/antigravity-cli/antigravity-oauth-token",
        None,
    ),
    (
        "antigravity",
        ".gemini/antigravity-cli/settings.json",
        Some("modelProvider"),
    ),
];

/// The label key read from a provisioned login file.
fn login_label_key(harness: &str) -> Option<&'static str> {
    match harness {
        "codex" => Some("auth_mode"),
        _ => None,
    }
}

/// The logins kept in files under the home directory, provisioned ones
/// first (in the provisioners' order), each with its label.
pub fn login_files(probe: &dyn Probe) -> Vec<LoginFile> {
    let provisioned = branchyard_provision::file_logins().into_iter().map(|l| {
        (
            l.harness,
            l.path,
            Some(l.secret),
            login_label_key(l.harness),
        )
    });
    let others = OTHER_LOGIN_FILES
        .iter()
        .map(|(harness, path, label)| (*harness, *path, None, *label));
    provisioned
        .chain(others)
        .filter_map(|(harness, path, secret, label)| {
            let file = format!("~/{path}");
            probe.exists(&file).then(|| LoginFile {
                harness: harness.to_owned(),
                label: label.and_then(|key| {
                    probe
                        .json_label(&file, key)
                        .map(|value| format!("{key} {value}"))
                }),
                secret: secret.map(str::to_owned),
                file,
            })
        })
        .collect()
}

/// The top-level string `key` of a JSON document, only when it reads as a
/// label (letters, digits, `.`, `_`, `-`, at most 32 characters), so no
/// token can come back through it.
pub fn label_in(text: &str, key: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let label = value.get(key)?.as_str()?;
    let plain = !label.is_empty()
        && label.len() <= 32
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    plain.then(|| label.to_owned())
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitFact {
    /// The work tree's root.
    pub root: String,
    /// `origin`'s default branch, or the current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Platform {
    /// `linux`, `macos`, ...
    pub os: String,
    /// `/dev/kvm` is present and usable (Microsandbox needs it).
    pub kvm: bool,
}

/// The machine and repository, as far as setup is concerned.
pub trait Probe {
    /// The project root: the git work tree's, or the current directory.
    fn root(&self) -> String;
    fn home(&self) -> Option<String>;
    fn git(&self) -> Option<GitFact>;
    fn harnesses(&self) -> Vec<HarnessFact>;
    /// Whether `name` is set to something non-empty. Never its value.
    fn env_is_set(&self, name: &str) -> bool;
    /// `Some(version or "")` when `program` is on `PATH`.
    fn tool(&self, program: &str) -> Option<String>;
    /// Whether `program` is on `PATH`, without running it.
    fn on_path(&self, program: &str) -> bool {
        self.tool(program).is_some()
    }
    fn platform(&self) -> Platform;
    /// A file's text, relative to [`Probe::root`] unless absolute; `None`
    /// when missing. Only files setup may write are asked for.
    fn read(&self, path: &str) -> Option<String>;
    fn exists(&self, path: &str) -> bool;
    /// A non-secret label from a JSON credential or settings file (`~/`
    /// for the home directory): its top-level string `key`, through
    /// [`label_in`], such as Codex's `auth_mode`. Nothing else of the file
    /// leaves the probe.
    fn json_label(&self, path: &str, key: &str) -> Option<String> {
        let _ = (path, key);
        None
    }
    /// The SHA-256 of a token file's first line, lowercase hex: how setup
    /// keeps an existing token without reading it into a plan.
    fn token_sha256(&self, path: &str) -> Option<String>;
    /// `~/.config/branchyard/config.toml`, or the platform's equivalent.
    fn user_config_path(&self) -> String;
}

/// A random bearer token or webhook secret. Injected so tests are
/// deterministic.
pub trait Entropy {
    fn token(&mut self) -> String;
}

/// Everything a topic reads to choose its questions and defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Facts {
    pub root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitFact>,
    pub harnesses: Vec<HarnessFact>,
    /// Which of [`KNOWN_VARIABLES`] are set.
    pub variables: BTreeSet<String>,
    /// Which of [`KNOWN_TOOLS`] are installed, with their version.
    pub tools: BTreeMap<String, String>,
    pub platform: Platform,
    /// The check suggested by the files at the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_check: Option<String>,
    /// `branchyard.toml` exists at the root.
    pub project_config: bool,
    pub user_config_path: String,
    pub user_config: bool,
    /// The plugin's directory in this checkout, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_dir: Option<String>,
    /// Harnesses with an install manifest (`plugins/branchyard/install`)
    /// whose binaries are on `PATH`: what `by init plugin` offers first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub install_detected: Vec<String>,
    /// What the root's files suggest for `[workspace]`.
    #[serde(default, skip_serializing_if = "WorkspaceFacts::is_empty")]
    pub workspace: WorkspaceFacts,
    /// Logins harnesses keep in files under the home directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logins: Vec<LoginFile>,
    /// The existing project and user files' text, for defaults.
    #[serde(skip)]
    #[schemars(skip)]
    pub project_text: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub user_text: Option<String>,
}

impl Facts {
    pub fn gather(probe: &dyn Probe) -> Facts {
        let user_config_path = probe.user_config_path();
        Facts {
            root: probe.root(),
            home: probe.home(),
            git: probe.git(),
            harnesses: probe.harnesses(),
            variables: KNOWN_VARIABLES
                .iter()
                .filter(|name| probe.env_is_set(name))
                .map(|name| (*name).to_owned())
                .collect(),
            tools: KNOWN_TOOLS
                .iter()
                .filter_map(|tool| {
                    probe
                        .tool(tool)
                        .map(|version| ((*tool).to_owned(), version))
                })
                .collect(),
            platform: probe.platform(),
            suggested_check: CHECK_MARKERS
                .iter()
                .find(|(file, _)| probe.exists(file))
                .map(|(_, check)| (*check).to_owned()),
            project_config: probe.exists(crate::config::PROJECT_FILE),
            user_config: probe.exists(&user_config_path),
            plugin_dir: probe
                .exists(PLUGIN_MANIFEST)
                .then(|| format!("{}/plugins/branchyard", probe.root())),
            install_detected: crate::install::detect(probe),
            workspace: WorkspaceFacts::detect(probe),
            logins: login_files(probe),
            project_text: probe.read(crate::config::PROJECT_FILE),
            user_text: probe.read(&user_config_path),
            user_config_path,
        }
    }

    /// Installed harnesses, in the registry's order.
    pub fn installed(&self) -> impl Iterator<Item = &HarnessFact> {
        self.harnesses.iter().filter(|h| h.installed)
    }

    /// The harness to suggest: Claude Code or Codex when installed and not
    /// known to be logged out, else the first installed one logged in,
    /// else the first installed, else Claude Code.
    pub fn preferred_harness(&self) -> String {
        let usable = |h: &&HarnessFact| h.login.as_ref().is_none_or(|l| l.state != "logged_out");
        for preferred in ["claude-code", "codex"] {
            if self
                .installed()
                .filter(usable)
                .any(|h| h.harness == preferred)
            {
                return preferred.into();
            }
        }
        self.installed()
            .find(|h| h.login.as_ref().is_some_and(HarnessLogin::logged_in))
            .or_else(|| self.installed().next())
            .map_or_else(|| "claude-code".into(), |h| h.harness.clone())
    }

    /// Installed harnesses logged in, likely or verified.
    pub fn logged_in(&self) -> impl Iterator<Item = &HarnessFact> {
        self.installed()
            .filter(|h| h.login.as_ref().is_some_and(HarnessLogin::logged_in))
    }

    pub fn repo_name(&self) -> String {
        let base = self
            .git
            .as_ref()
            .map_or(self.root.as_str(), |g| g.root.as_str())
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        let mut name = String::new();
        for c in base.chars() {
            if c.is_ascii_alphanumeric() {
                name.push(c.to_ascii_lowercase());
            } else if !name.is_empty() && !name.ends_with('-') {
                name.push('-');
            }
        }
        let name = name.trim_end_matches('-').to_owned();
        match name.is_empty() {
            true => "repo".into(),
            false => name,
        }
    }

    /// The facts as lines to show before the questions.
    pub fn lines(&self) -> Vec<Fact> {
        let mut lines = Vec::new();
        let mut push = |id: &str, label: &str, value: String| {
            lines.push(Fact {
                id: id.into(),
                label: label.into(),
                value,
            })
        };
        push(
            "repository",
            "Repository",
            match &self.git {
                Some(git) => match &git.default_branch {
                    Some(branch) => format!("{} (default branch {branch})", git.root),
                    None => git.root.clone(),
                },
                None => format!("{} (not a git repository)", self.root),
            },
        );
        // A configuration file `by` itself refuses is said first: the
        // interview cannot start from its values.
        for (file, text) in [
            (crate::config::PROJECT_FILE, &self.project_text),
            (self.user_config_path.as_str(), &self.user_text),
        ] {
            if let Some(problem) = text.as_deref().and_then(crate::hint::unreadable) {
                push(
                    "config_problem",
                    file,
                    format!(
                        "does not load: {}; its values are not used here",
                        problem.problem()
                    ),
                );
            }
        }
        let installed: Vec<String> = self
            .installed()
            .map(|h| {
                let named = match &h.version {
                    Some(v) if !v.is_empty() => format!("{} {v}", h.harness),
                    _ => h.harness.clone(),
                };
                match &h.login {
                    Some(login) => format!(
                        "{named} ({}{})",
                        login.state.replace('_', " "),
                        match login.evidence.as_str() {
                            "none" | "" => String::new(),
                            how => format!(", {how}"),
                        }
                    ),
                    None => named,
                }
            })
            .collect();
        push(
            "harnesses",
            "Harnesses",
            match installed.is_empty() {
                true => "none installed on PATH".into(),
                false => installed.join(", "),
            },
        );
        if !self.logins.is_empty() {
            let logins: Vec<String> = self
                .logins
                .iter()
                .map(|l| {
                    let label = l
                        .label
                        .as_ref()
                        .map_or(String::new(), |l| format!(" ({l})"));
                    match &l.secret {
                        Some(secret) => format!("{} {}{label}, as {secret}", l.harness, l.file),
                        None => format!("{} {}{label}, not provisioned", l.harness, l.file),
                    }
                })
                .collect();
            push("logins", "Harness logins in files", logins.join("; "));
        }
        push(
            "credentials",
            "Credential variables set",
            match self.variables.is_empty() {
                true => "none".into(),
                false => self
                    .variables
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            },
        );
        let tools: Vec<String> = self
            .tools
            .iter()
            .map(|(tool, version)| match version.is_empty() {
                true => tool.clone(),
                false => format!("{tool} {version}"),
            })
            .collect();
        push(
            "tools",
            "Tools",
            match tools.is_empty() {
                true => "none".into(),
                false => tools.join(", "),
            },
        );
        push(
            "platform",
            "Platform",
            format!(
                "{}{}",
                self.platform.os,
                match self.platform.kvm {
                    true => ", KVM available",
                    false => ", no KVM",
                }
            ),
        );
        if let Some(check) = &self.suggested_check {
            push("check", "Suggested check", check.clone());
        }
        let w = &self.workspace;
        if !w.is_empty() {
            let mut parts = Vec::new();
            if !w.copy.is_empty() {
                parts.push(format!("copy {}", w.copy.join(", ")));
            }
            if !w.setup.is_empty() {
                parts.push(format!("setup {}", w.setup.join(" && ")));
            }
            if let Some(run) = &w.run {
                parts.push(format!("run {run}"));
            }
            push(
                "workspace",
                "Suggested workspace",
                format!("{} (from {})", parts.join("; "), w.found.join(", ")),
            );
        }
        if !w.notes.is_empty() {
            push("workspace_notes", "Not imported", w.notes.join("; "));
        }
        push(
            "config",
            "Configuration",
            format!(
                "project {}; user {} {}",
                match self.project_config {
                    true => "branchyard.toml exists",
                    false => "branchyard.toml not found",
                },
                self.user_config_path,
                match self.user_config {
                    true => "exists",
                    false => "not found",
                }
            ),
        );
        lines
    }
}

/// One detected fact, for display.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Fact {
    pub id: String,
    pub label: String,
    pub value: String,
}

/// A harness's display name.
pub fn harness_label(harness: &str) -> String {
    match harness {
        "claude-code" => "Claude Code".into(),
        "codex" => "Codex".into(),
        "antigravity" => "Antigravity".into(),
        "oh-my-pi" => "Oh My Pi".into(),
        "deepseek-harness" => "DeepSeek Harness".into(),
        "gemini-cli" => "Gemini CLI".into(),
        "opencode" => "OpenCode".into(),
        "pi" => "Pi".into(),
        "goose" => "Goose".into(),
        "cursor" => "Cursor".into(),
        "github-copilot" => "GitHub Copilot".into(),
        "amp" => "Amp".into(),
        "qwen-code" => "Qwen Code".into(),
        "kimi-cli" => "Kimi CLI".into(),
        "hermes" => "Hermes".into(),
        "droid" => "Droid".into(),
        "kiro" => "Kiro".into(),
        other => other.into(),
    }
}

/// Profiles that cannot route tool permission requests to Branchyard.
pub fn unapproved_tools(harness: &str) -> bool {
    matches!(harness, "antigravity" | "pi" | "amp")
}

/// A probe with fixed answers, for tests and examples.
#[derive(Clone, Debug, Default)]
pub struct FakeProbe {
    pub root: String,
    pub home: Option<String>,
    pub git: Option<GitFact>,
    pub harnesses: Vec<HarnessFact>,
    pub variables: BTreeSet<String>,
    pub tools: BTreeMap<String, String>,
    pub platform: Platform,
    /// Files by path as [`Probe::read`] is asked for them.
    pub files: BTreeMap<String, String>,
}

impl FakeProbe {
    /// A repository at `/src/app` with Claude Code and Codex installed,
    /// `ANTHROPIC_API_KEY` set, git and docker, on Linux without KVM.
    pub fn typical() -> FakeProbe {
        let harness =
            |harness: &str, profile: &str, installed: bool, version: Option<&str>| HarnessFact {
                harness: harness.into(),
                profile: profile.into(),
                installed,
                version: version.map(str::to_owned),
                qualification: None,
                login: None,
            };
        FakeProbe {
            root: "/src/app".into(),
            home: Some("/home/dev".into()),
            git: Some(GitFact {
                root: "/src/app".into(),
                default_branch: Some("main".into()),
            }),
            harnesses: vec![
                harness(
                    "claude-code",
                    "claude-code-stream-json",
                    true,
                    Some("2.1.283 (Claude Code)"),
                ),
                harness("codex", "codex-app-server", true, Some("codex-cli 0.157.1")),
                harness("antigravity", "antigravity-stream-json", false, None),
                harness("gemini-cli", "gemini-cli-acp", false, None),
                harness("opencode", "opencode-acp", false, None),
            ],
            variables: ["ANTHROPIC_API_KEY".to_owned()].into(),
            tools: [
                ("docker".to_owned(), "27.1.1".to_owned()),
                ("git".to_owned(), "2.45.2".to_owned()),
            ]
            .into(),
            platform: Platform {
                os: "linux".into(),
                kvm: false,
            },
            files: [("Cargo.toml".to_owned(), "[package]\n".to_owned())].into(),
        }
    }
}

impl Probe for FakeProbe {
    fn root(&self) -> String {
        self.root.clone()
    }
    fn home(&self) -> Option<String> {
        self.home.clone()
    }
    fn git(&self) -> Option<GitFact> {
        self.git.clone()
    }
    fn harnesses(&self) -> Vec<HarnessFact> {
        self.harnesses.clone()
    }
    fn env_is_set(&self, name: &str) -> bool {
        self.variables.contains(name)
    }
    fn tool(&self, program: &str) -> Option<String> {
        self.tools.get(program).cloned()
    }
    fn platform(&self) -> Platform {
        self.platform.clone()
    }
    fn read(&self, path: &str) -> Option<String> {
        self.files.get(path).cloned()
    }
    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }
    fn json_label(&self, path: &str, key: &str) -> Option<String> {
        label_in(self.files.get(path)?, key)
    }
    fn token_sha256(&self, path: &str) -> Option<String> {
        self.files
            .get(path)
            .map(|text| crate::sha256_hex(text.lines().next().unwrap_or("").trim().as_bytes()))
    }
    fn user_config_path(&self) -> String {
        format!(
            "{}/.config/branchyard/config.toml",
            self.home.as_deref().unwrap_or("/home/dev")
        )
    }
}

/// Counting tokens: `token-0001...`, for deterministic tests.
#[derive(Clone, Debug, Default)]
pub struct CountingEntropy(pub u32);

impl Entropy for CountingEntropy {
    fn token(&mut self) -> String {
        self.0 += 1;
        format!("test-token-{:04}-{}", self.0, "0".repeat(48))
    }
}
