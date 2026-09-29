//! What setup detects about the machine and the repository, through an
//! injected [`Probe`], so the engine itself does no I/O and tests use a
//! fake. A probe reports whether a variable is set, never its value; the
//! hash of an existing token file, never the token.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Variables whose presence setup reports, by name only: the credentials
/// harness provisioning reads, and Branchyard's own.
pub const KNOWN_VARIABLES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "BRANCHYARD_REMOTE",
    "BRANCHYARD_TOKEN_FILE",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CODEX_API_KEY",
    "COPILOT_GITHUB_TOKEN",
    "DATABASE_URL",
    "GEMINI_API_KEY",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GOOGLE_API_KEY",
    "GOOGLE_CLOUD_PROJECT",
    "OPENAI_API_KEY",
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
        // JavaScript: the lockfile names the package manager.
        let node = [
            (
                "pnpm-lock.yaml",
                "pnpm install --frozen-lockfile",
                "pnpm dev",
            ),
            ("yarn.lock", "yarn install --frozen-lockfile", "yarn dev"),
            ("bun.lock", "bun install --frozen-lockfile", "bun run dev"),
            ("bun.lockb", "bun install --frozen-lockfile", "bun run dev"),
            ("package-lock.json", "npm ci", "npm run dev"),
        ]
        .into_iter()
        .find(|(lock, _, _)| found(lock))
        .map(|(_, install, dev)| (install, dev));
        let mut setup = Vec::new();
        let mut run = None;
        if found("package.json") {
            let (install, dev) = node.unwrap_or(("npm install", "npm run dev"));
            setup.push(install.to_owned());
            run = Some(format!("PORT=$BRANCHYARD_PORT {dev}"));
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
        facts
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
    fn platform(&self) -> Platform;
    /// A file's text, relative to [`Probe::root`] unless absolute; `None`
    /// when missing. Only files setup may write are asked for.
    fn read(&self, path: &str) -> Option<String>;
    fn exists(&self, path: &str) -> bool;
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
    /// What the root's files suggest for `[workspace]`.
    #[serde(default, skip_serializing_if = "WorkspaceFacts::is_empty")]
    pub workspace: WorkspaceFacts,
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
            workspace: WorkspaceFacts::detect(probe),
            project_text: probe.read(crate::config::PROJECT_FILE),
            user_text: probe.read(&user_config_path),
            user_config_path,
        }
    }

    /// Installed harnesses, in the registry's order.
    pub fn installed(&self) -> impl Iterator<Item = &HarnessFact> {
        self.harnesses.iter().filter(|h| h.installed)
    }

    /// The harness to suggest: Claude Code or Codex when installed, else
    /// the first installed, else Claude Code.
    pub fn preferred_harness(&self) -> String {
        for preferred in ["claude-code", "codex"] {
            if self.installed().any(|h| h.harness == preferred) {
                return preferred.into();
            }
        }
        self.installed()
            .next()
            .map(|h| h.harness.clone())
            .unwrap_or_else(|| "claude-code".into())
    }

    pub fn repo_name(&self) -> String {
        let base = self
            .git
            .as_ref()
            .map(|g| g.root.as_str())
            .unwrap_or(self.root.as_str())
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
        let installed: Vec<String> = self
            .installed()
            .map(|h| match &h.version {
                Some(v) if !v.is_empty() => format!("{} {v}", h.harness),
                _ => h.harness.clone(),
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
