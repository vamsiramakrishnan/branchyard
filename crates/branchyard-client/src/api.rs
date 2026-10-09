//! Wire types of the server API, shared by the server and this client.
//!
//! Branches, statuses and recorded events use the `branchyard` SDK's own
//! serde forms, so a remote caller gets the same values a local one does.
//! Request types reject unknown fields: a misspelled option is an error,
//! not a silently ignored one.

use std::time::Duration;

use branchyard::{
    Activity, After, Binding, BranchInfo, Budget, Envelope, GraphEdit, HarnessInfo, Inspection,
    MapItem, MapReport, MapSummary, Merged, Policy, Provider, Provisioning, RecordedEvent, Seats,
    StallAction,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Limits for a branch; see [`branchyard::Budget`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Per call, like [`branchyard::Budget::max_duration`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_seconds: Option<f64>,
    /// Like [`branchyard::Budget::stall_after`], in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_after_seconds: Option<f64>,
    /// Like [`branchyard::Budget::stall_action`]; ignored without
    /// `stall_after_seconds`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_action: Option<StallAction>,
}

impl BudgetSpec {
    pub fn from_budget(budget: &Budget) -> Self {
        BudgetSpec {
            max_usd: budget.max_usd,
            max_turns: budget.max_turns,
            max_seconds: budget.max_duration.map(|d| d.as_secs_f64()),
            stall_after_seconds: budget.stall_after.map(|d| d.as_secs_f64()),
            stall_action: budget.stall_after.map(|_| budget.stall_action),
        }
    }

    /// The SDK budget, refusing values that are not positive and finite.
    pub fn to_budget(&self) -> Result<Budget, String> {
        let positive = |v: f64| v.is_finite() && v > 0.0;
        if let Some(usd) = self.max_usd.filter(|v| !positive(*v)) {
            return Err(format!("budget.max_usd must be positive, not {usd}"));
        }
        if self.max_turns == Some(0) {
            return Err("budget.max_turns must be positive".into());
        }
        let max_duration = match self.max_seconds {
            None => None,
            Some(s) if positive(s) => Some(
                Duration::try_from_secs_f64(s)
                    .map_err(|_| format!("budget.max_seconds {s} is too large"))?,
            ),
            Some(s) => return Err(format!("budget.max_seconds must be positive, not {s}")),
        };
        let stall_after = match self.stall_after_seconds {
            None => None,
            Some(s) if positive(s) => Some(
                Duration::try_from_secs_f64(s)
                    .map_err(|_| format!("budget.stall_after_seconds {s} is too large"))?,
            ),
            Some(s) => {
                return Err(format!(
                    "budget.stall_after_seconds must be positive, not {s}"
                ))
            }
        };
        Ok(Budget {
            max_usd: self.max_usd,
            max_turns: self.max_turns,
            max_duration,
            stall_after,
            stall_action: self.stall_action.unwrap_or_default(),
            // Not on the wire: a remote turn's hold has the default cap.
            hold_cap: None,
        })
    }
}

/// How requests no rule decides are answered. There is no remote `ask`:
/// a server has no terminal to ask on.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    Allow,
    #[default]
    Deny,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    /// Exact tool name, or a `*` suffix wildcard such as `mcp__*`.
    pub tool: String,
    pub allow: bool,
}

/// A permission policy: rules in order, then the mode. Defaults to deny.
/// With a preset, its rules follow these rules and its default replaces
/// `mode`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySpec {
    #[serde(default)]
    pub mode: PolicyMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RuleSpec>,
    /// A named preset (`read-only`, `edit-worktree`, `full`) standing for
    /// its explicit rules; see `docs/egress.md#permission-presets`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<branchyard::PolicyPreset>,
}

impl PolicySpec {
    pub fn allow_all() -> Self {
        PolicySpec {
            mode: PolicyMode::Allow,
            rules: Vec::new(),
            preset: None,
        }
    }

    /// Only the preset's rules and default.
    pub fn preset(preset: branchyard::PolicyPreset) -> Self {
        PolicySpec {
            preset: Some(preset),
            ..PolicySpec::default()
        }
    }

    pub fn to_policy(&self) -> Policy {
        let preset = self.preset.map(|p| p.rules());
        let default_allow = match &preset {
            Some(rules) => rules.default_allow,
            None => self.mode == PolicyMode::Allow,
        };
        let base = match default_allow {
            true => Policy::allow_all(),
            false => Policy::deny_all(),
        };
        let policy = self
            .rules
            .iter()
            .fold(base, |policy, rule| match rule.allow {
                true => policy.allow(rule.tool.clone()),
                false => policy.deny(rule.tool.clone()),
            });
        let Some(preset) = preset else {
            return policy;
        };
        let policy = preset.deny.iter().fold(policy, |p, tool| p.deny(*tool));
        preset.allow.iter().fold(policy, |p, tool| p.allow(*tool))
    }
}

/// `POST /v1/repos/{repo}/tasks`. With `harnesses`, runs one branch per
/// harness like [`branchyard::TaskBuilder::run_on`]; otherwise one branch.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRequest {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harnesses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default)]
    pub budget: BudgetSpec,
    #[serde(default)]
    pub policy: PolicySpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Vec<String>>,
    #[serde(default)]
    pub isolated: bool,
    /// Executable replacing the profile's, on the server. Refused unless the
    /// server allows client commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Let the harness delegate within this envelope, like
    /// [`branchyard::TaskOptions::delegation`]. Refused unless the server
    /// allows delegation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Envelope>,
    /// Allow the harness's shell commands that run the server's `by` with a
    /// delegation subcommand, after the policy's own rules, like
    /// `by --allow-delegation`. Refused unless the server allows delegation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_delegation: bool,
    /// Run a profile whose driver cannot route tool approvals, like
    /// [`branchyard::TaskOptions::unapproved_tools`]. Refused unless the
    /// server allows unapproved tools.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unapproved_tools: bool,
    /// Where the harness runs, in [`branchyard::Provider`]'s serde form.
    /// Anything but `local` is refused unless the server allows that
    /// provider. Paths and `pass_env` names are the server's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    /// What to provision in the harness's home, like
    /// [`branchyard::TaskOptions::provision`]. Secrets are named only: the
    /// server's operator decides where each comes from, and one the server
    /// does not define is refused. MCP servers are commands the server
    /// runs, refused unless it allows client commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<Provisioning>,
    /// Make the branch a rig's root, like [`branchyard::TaskOptions::seats`]:
    /// the seats its harness may spawn by name, within `delegation`. Refused
    /// unless the server allows delegation. Each seat's provisioning is
    /// held to the same rules as `provision`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seats: Option<Seats>,
    /// Worker labels the operation needs: only a worker started with every
    /// one of them (`by worker --label gpu`) claims it. See
    /// `docs/server.md#worker-labels`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_labels: Vec<String>,
    /// The operation's priority, -10 to 10 (default 0): higher runs first,
    /// and the server caps it at the tenant's `max_priority`. See
    /// `docs/server.md#scheduling`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// Plan first, like `by run --plan`: the first turn runs read-only and
    /// the branch waits for `POST .../plan/approve`. See
    /// `docs/plans-and-goals.md`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub plan: bool,
    /// A goal a judge verifies, like `by run --goal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalRequest>,
    /// Tools the branch's harness is denied outright, like `by run --deny`
    /// and [`branchyard::TaskOptions::deny`]: stored with the branch, ahead
    /// of every policy its turns run under, and passed on to its children.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

/// A task's goal: its text, the follow-up turns it may get, and the judge
/// harness (one of the server's) that verifies it; without one, the
/// branch's check and a non-empty diff decide.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalRequest {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// `POST /v1/repos/{repo}/maps`: a wide map, run as one operation. See
/// `docs/map.md`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MapRequest {
    /// The map's name (default: a slug of the prompt). Running a map of
    /// this name again skips its items done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The items, each with its id, as `branchyard::parse_map_items` reads
    /// them from a file.
    pub items: Vec<MapItem>,
    /// The JSON Schema every answer must match (the subset in
    /// `docs/map.md#schemas`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduce: Option<String>,
    /// Remove an item's branches once its answer is recorded.
    #[serde(default, skip_serializing_if = "is_false")]
    pub remove_done: bool,
    /// Run the items that failed in an earlier run again.
    #[serde(default, skip_serializing_if = "is_false")]
    pub retry_failed: bool,
    /// Every branch's options, as a task's; `prompt` is the template, and
    /// `name`, `harnesses`, `seats`, `plan` and `goal` are refused.
    pub task: TaskRequest,
}

/// `POST /v1/repos/{repo}/maps/{name}/resume`: run a recorded map again
/// with the request that started it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MapResumeRequest {
    #[serde(default, skip_serializing_if = "is_false")]
    pub retry_failed: bool,
}

/// `GET /v1/repos/{repo}/maps`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MapList {
    pub maps: Vec<MapSummary>,
}

/// `GET /v1/repos/{repo}/task-records`: the repository's tasks and their
/// attempts (docs/task-repos.md). `GET .../task-records/{task}`
/// returns one [`branchyard::tasks::TaskView`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskList {
    pub tasks: Vec<branchyard::tasks::TaskView>,
}

/// `POST /v1/repos/{repo}/branches/{branch}/send`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendRequest {
    /// Empty with `retry`.
    #[serde(default)]
    pub prompt: String,
    /// Submit again the prompt of the branch's last turn that was cut off,
    /// like `by send --retry`, instead of `prompt`. Refused when none was.
    #[serde(default, skip_serializing_if = "is_false")]
    pub retry: bool,
    #[serde(default)]
    pub budget: BudgetSpec,
    #[serde(default)]
    pub policy: PolicySpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Let the harness delegate within this envelope, like
    /// [`branchyard::TaskOptions::delegation`]. Refused unless the server
    /// allows delegation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Envelope>,
    /// Allow the harness's shell commands that run the server's `by` with a
    /// delegation subcommand, after the policy's own rules, like
    /// `by --allow-delegation`. Refused unless the server allows delegation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_delegation: bool,
    /// Run a profile whose driver cannot route tool approvals, like
    /// [`branchyard::TaskOptions::unapproved_tools`]. Refused unless the
    /// server allows unapproved tools.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unapproved_tools: bool,
    /// What to provision in the harness's home, like
    /// [`branchyard::TaskOptions::provision`]. Secrets are named only: the
    /// server's operator decides where each comes from, and one the server
    /// does not define is refused. MCP servers are commands the server
    /// runs, refused unless it allows client commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<Provisioning>,
    /// Worker labels the operation needs: only a worker started with every
    /// one of them (`by worker --label gpu`) claims it. See
    /// `docs/server.md#worker-labels`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_labels: Vec<String>,
    /// The operation's priority, -10 to 10 (default 0): higher runs first,
    /// and the server caps it at the tenant's `max_priority`. See
    /// `docs/server.md#scheduling`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
}

/// `POST /v1/repos/{repo}/branches/{branch}/fork`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkRequest {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub fresh_session: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default)]
    pub budget: BudgetSpec,
    #[serde(default)]
    pub policy: PolicySpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Vec<String>>,
    #[serde(default)]
    pub isolated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Let the harness delegate within this envelope, like
    /// [`branchyard::TaskOptions::delegation`]. Refused unless the server
    /// allows delegation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Envelope>,
    /// Allow the harness's shell commands that run the server's `by` with a
    /// delegation subcommand, after the policy's own rules, like
    /// `by --allow-delegation`. Refused unless the server allows delegation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_delegation: bool,
    /// Run a profile whose driver cannot route tool approvals, like
    /// [`branchyard::TaskOptions::unapproved_tools`]. Refused unless the
    /// server allows unapproved tools.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unapproved_tools: bool,
    /// Without one, the fork keeps its parent's provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    /// What to provision in the harness's home, like
    /// [`branchyard::TaskOptions::provision`]. Secrets are named only: the
    /// server's operator decides where each comes from, and one the server
    /// does not define is refused. MCP servers are commands the server
    /// runs, refused unless it allows client commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<Provisioning>,
    /// Worker labels the operation needs: only a worker started with every
    /// one of them (`by worker --label gpu`) claims it. See
    /// `docs/server.md#worker-labels`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_labels: Vec<String>,
    /// The operation's priority, -10 to 10 (default 0): higher runs first,
    /// and the server caps it at the tenant's `max_priority`. See
    /// `docs/server.md#scheduling`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
}

/// `POST /v1/repos/{repo}/branches/{branch}/reincarnate`: a new branch from
/// `branch`'s latest candidate, always with a fresh session and a generated
/// handoff brief; see [`branchyard::Branch::reincarnate`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReincarnateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default)]
    pub budget: BudgetSpec,
    #[serde(default)]
    pub policy: PolicySpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Vec<String>>,
    #[serde(default)]
    pub isolated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Let the harness delegate within this envelope, like
    /// [`branchyard::TaskOptions::delegation`]. Refused unless the server
    /// allows delegation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Envelope>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_delegation: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub unapproved_tools: bool,
    /// Without one, the new branch keeps the old one's provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    /// Like [`branchyard::TaskOptions::provision`]; a model change here is
    /// how a reincarnation changes model. Without one, the old branch's is
    /// kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<Provisioning>,
    /// Worker labels the operation needs: only a worker started with every
    /// one of them (`by worker --label gpu`) claims it. See
    /// `docs/server.md#worker-labels`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_labels: Vec<String>,
    /// The operation's priority, -10 to 10 (default 0): higher runs first,
    /// and the server caps it at the tenant's `max_priority`. See
    /// `docs/server.md#scheduling`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
}

/// `POST /v1/repos/{repo}/branches/{branch}/merge`. Without a target, the
/// branch checked out in the served repository.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// `POST /v1/repos/{repo}/branches/{parent}/spawn`: a child of `parent`,
/// created with the server's authority as a person, like
/// `by spawn --parent`. The parent's envelope bounds it exactly as it bounds
/// a local spawn. Needs a server that allows delegation.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnRequest {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The child's limits; they also bound the parent's side of the call,
    /// as with `by spawn --parent`.
    #[serde(default)]
    pub budget: BudgetSpec,
    /// Answers the child's tool requests, before the parent's denials.
    #[serde(default)]
    pub policy: PolicySpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub unapproved_tools: bool,
    /// The seat of the parent's rig the child fills, like
    /// [`branchyard::Spawn::seat`]. Without a name, the child is named
    /// `<parent>-<seat>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat: Option<String>,
    /// Siblings the child waits for, like [`branchyard::Spawn::depends_on`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "is_settled")]
    pub after: After,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Binding>,
    /// Worker labels the operation needs: only a worker started with every
    /// one of them (`by worker --label gpu`) claims it. See
    /// `docs/server.md#worker-labels`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_labels: Vec<String>,
    /// The operation's priority, -10 to 10 (default 0): higher runs first,
    /// and the server caps it at the tenant's `max_priority`. See
    /// `docs/server.md#scheduling`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// The child's connector grant, like [`branchyard::Spawn::connectors`]:
    /// narrowed to its parent's; unset is its seat's or its parent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connectors: Option<Vec<branchyard::connectors::GrantEntry>>,
    /// Children the child may have, like [`branchyard::Spawn::max_children`];
    /// at most its parent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_children: Option<u32>,
    /// Harnesses the child may delegate to, like
    /// [`branchyard::Spawn::harnesses`]; each must be allowed to its parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harnesses: Option<Vec<String>>,
    /// Plan first, like `by spawn --plan`: the child's first turn is
    /// read-only and its plan is escalated to its parent.
    #[serde(default, skip_serializing_if = "is_false")]
    pub plan: bool,
    /// The child's model, like [`branchyard::Spawn::model`]; unset is its
    /// seat's or its parent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

fn is_settled(after: &After) -> bool {
    *after == After::Settled
}

/// `POST /v1/repos/{repo}/branches/{branch}/graph`: a graph proposal for
/// the branch's children, applied with the server's authority as a person,
/// like `by graph apply --parent`. The answer is the
/// [`branchyard::GraphApplied`]; a proposal made against a revision that
/// moved on is `409 stale_revision` and changes nothing. Children run on
/// the server; follow them with `inspect`, `events` and `graph`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphRequest {
    pub expected_revision: u64,
    pub edits: Vec<GraphEdit>,
    /// Answers the children's tool requests, before their denials.
    #[serde(default)]
    pub policy: PolicySpec,
    #[serde(default, skip_serializing_if = "is_false")]
    pub unapproved_tools: bool,
}

/// `POST /v1/repos/{repo}/branches/{branch}/integrate`: merge a delegated
/// child into the parent that delegated it, like `by integrate`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrateRequest {
    /// Siblings of the path's branch, delegated by the same parent, to
    /// integrate together with it, in order after it: merged in one
    /// temporary worktree, checked once on the result, all or none, like
    /// `by integrate a b c`. The result is then `merged_all`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub with: Vec<String>,
}

/// `POST /v1/repos/{repo}/branches/{branch}/steer`: input for the branch's
/// running turn, like `by send --steer`. The answer is the
/// [`branchyard::Steer`] after waiting briefly for its delivery.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SteerRequest {
    pub text: String,
}

/// `POST /v1/repos/{repo}/branches/{branch}/cancel`: no fields yet.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {}

/// The branches a cancel asked to stop: the branch and its delegated
/// descendants that were running a turn.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelResult {
    pub cancelled: Vec<String>,
}

/// `POST /v1/repos/{repo}/branches/{branch}/discard`: set a settled
/// branch aside, like `by discard`. The answer is the branch's
/// [`branchyard::Inspection`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscardRequest {
    /// Why, recorded in the branch's status; the server names the caller
    /// when it is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `POST /v1/repos/{repo}/wait`: block until branches settle, like
/// `by wait`. The answer is a [`branchyard::Waited`].
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitRequest {
    /// The branches to wait for; at least one.
    pub branches: Vec<String>,
    /// Return once any one of them has settled, not all.
    #[serde(default, skip_serializing_if = "is_false")]
    pub any: bool,
    /// Return after this many seconds even if the wait is not satisfied,
    /// with `timed_out` set. The server caps it, at 30 seconds, so a
    /// longer wait asks again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<f64>,
}

/// `POST /v1/repos/{repo}/branches/{branch}/ask`: a question to the
/// branch's parent, like `by ask`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskRequest {
    pub text: String,
    /// Block up to this many seconds for an answer; `None` returns once
    /// the question is sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_seconds: Option<f64>,
}

/// `POST /v1/repos/{repo}/branches/{branch}/report` or `/escalate`: a
/// message with no answer expected, like `by report` and `by escalate`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextRequest {
    pub text: String,
}

/// `POST /v1/repos/{repo}/branches/{branch}/answer`: an answer to one of
/// the branch's own descendants' messages, like `by answer`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerRequest {
    pub message_id: u64,
    pub text: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Task,
    Send,
    Fork,
    Merge,
    /// A child created with `POST .../spawn`.
    Spawn,
    /// A child merged into its parent with `POST .../integrate`.
    Integrate,
    /// A branch started with `POST .../reincarnate`.
    Reincarnate,
    /// A plan approved with `POST .../plan/approve`, run as a turn.
    ApprovePlan,
    /// A plan rejected with `POST .../plan/reject`.
    RejectPlan,
    /// A wide map started with `POST .../maps` or `.../maps/{name}/resume`.
    Map,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    /// Accepted and recorded; waiting for a slot.
    Queued,
    Running,
    Succeeded,
    Failed,
    /// The server stopped before the operation finished. Its branches may
    /// still say `running`; see `docs/server.md`.
    Interrupted,
}

impl OperationState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OperationState::Succeeded | OperationState::Failed | OperationState::Interrupted
        )
    }
}

/// What a finished operation produced.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OperationResult {
    /// Branches a task, send or fork ran, in request order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<BranchInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged: Option<Merged>,
    /// Several children integrated together (`IntegrateRequest::with`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_all: Option<branchyard::MergedAll>,
    /// Every branch the operation's branches delegated to, directly or
    /// below, once they finished: the operation waits for them, as
    /// `by run` does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub descendants: Vec<BranchInfo>,
    /// A spawned child, inspected once its turn ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspection: Option<Inspection>,
    /// A map's rows and progress once it ended; its `branches` above are
    /// the branches that answered or were tried last, where they remain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map: Option<MapReport>,
}

/// A long operation, run in the background. Durable on the server from
/// the moment it is returned.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub repo: String,
    pub kind: OperationKind,
    pub state: OperationState,
    /// Branches the operation works on, known when it was accepted. For a
    /// task they are the planned names, which agree with the result unless
    /// another caller created a branch in between.
    pub branches: Vec<String>,
    /// Feed position when the operation was accepted: its activity has a
    /// greater sequence number.
    pub cursor: u64,
    /// Feed position once the operation finished and its activity was
    /// ingested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_cursor: Option<u64>,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<OperationResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
    /// Worker labels the operation needs; only a worker carrying all of
    /// them claims it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Why a queued operation has not been claimed, once it has waited
    /// longer than the server's `unclaimable_after`: no live worker serving
    /// its repository carries the labels it requires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<String>,
    /// Its priority as admitted (after the tenant's cap; a spawn's
    /// inherited from its parent when its request named none). Claims take
    /// higher priorities first; see `docs/server.md#scheduling`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub priority: i32,
}

fn is_zero(value: &i32) -> bool {
    *value == 0
}

/// A structured error. `code` is stable; `message` is for people.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

/// Every error response is `{"error": {...}}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

/// One entry of a repository's activity feed, as streamed.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeedEntry {
    /// Position in the feed, from 1. Resume after it with `cursor=<seq>`.
    pub seq: u64,
    pub branch: String,
    pub at_ms: u64,
    pub activity: Activity,
}

/// `GET .../branches/{branch}/events?cursor=N`: the events after the first
/// `N`, and the cursor to pass next.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchEvents {
    pub events: Vec<RecordedEvent>,
    pub cursor: u64,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchList {
    pub branches: Vec<BranchInfo>,
}

/// `GET /v1/repos/{repo}/operations[?branch=NAME]`: the caller's tenant's
/// queued and running operations of the repository, oldest first, each
/// saying why it waits when no live worker can claim it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperationList {
    pub operations: Vec<Operation>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub diff: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    pub removed: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    pub name: String,
    /// The repository root on the server.
    pub root: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoList {
    pub repos: Vec<RepoEntry>,
}

/// A harness profile on the wire: [`branchyard::HarnessInfo`]'s own serde
/// form. `available` is about the server's `PATH`.
pub type HarnessEntry = HarnessInfo;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessList {
    pub harnesses: Vec<HarnessInfo>,
}

/// One worker's machine and the harnesses it has
/// (docs/harness-lifecycle.md).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerInventory {
    /// The worker's ID, as its claims record it.
    pub id: String,
    pub host: String,
    /// The labels it claims with, `harness:<id>` ones included.
    pub labels: Vec<String>,
    /// The repositories it serves (only those the caller can see).
    pub repos: Vec<String>,
    /// Milliseconds since its last beat.
    pub seen_ms_ago: u64,
    /// The server that answered is this worker.
    #[serde(default)]
    pub this: bool,
    /// What it advertised; `None` when it advertises nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory: Option<branchyard::inventory::Inventory>,
}

/// `GET /v1/inventory`: the live workers serving the caller's repositories,
/// the answering server's first, and the harnesses each one's machine has.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InventoryReport {
    pub workers: Vec<WorkerInventory>,
}

/// A merge on the wire: [`branchyard::Merged`]'s own serde form.
pub type MergedInfo = Merged;

/// `POST /v1/services`: register a service in the server's fleet
/// registry, or renew one this caller registered (the same `id`). Needs
/// the `admin` scope. What is registered this way never carries anything
/// for the server to reclaim. See `docs/registry.md`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RegisterServiceRequest {
    /// The record's ID; a fresh one when absent. Registering the same ID
    /// again renews it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Such as `model_gateway` or `connector_gateway`.
    pub kind: String,
    #[serde(default)]
    pub capabilities: std::collections::BTreeMap<String, branchyard::services::Capability>,
    #[serde(default)]
    pub endpoints: Vec<branchyard::services::Endpoint>,
    #[serde(default)]
    pub health: branchyard::services::Health,
    /// Among equally healthy records, heavier ones are resolved first
    /// (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    /// The lease, in seconds (default 30, at least 1, at most a day).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

/// `GET /v1/services`: the fleet's services (workers among them), and
/// `POST /v1/services/gc`: those it expired and reclaimed.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServiceList {
    pub services: Vec<branchyard::services::Service>,
}

/// One service as `GET /.well-known/branchyard` describes it, to anyone:
/// what it is and can do, not where it is or who runs it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServiceSummary {
    pub id: String,
    pub kind: String,
    pub capabilities: std::collections::BTreeMap<String, branchyard::services::Capability>,
    pub health: branchyard::services::Health,
}

/// `GET /.well-known/branchyard`: what this server is, where its API and
/// keys are, and which services its fleet has live. Public.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WellKnown {
    /// `branchyard`.
    pub service: String,
    pub version: String,
    /// The API's base path, `/v1`.
    pub api: String,
    /// The connector gateway's verification keys, when the server has
    /// connectors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    /// The full list, with endpoints and owners, for a caller with the
    /// `read` scope.
    pub services_uri: String,
    /// The served repositories' names.
    pub repos: Vec<String>,
    /// Scopes a token may carry.
    pub scopes: Vec<String>,
    pub services: Vec<ServiceSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::{BranchStatus, PermissionKey, PermissionRequest};
    use serde_json::json;

    #[test]
    fn a_preset_follows_the_request_rules_and_sets_the_default() {
        let spec: PolicySpec = serde_json::from_value(json!({
            "preset": "edit-worktree",
            "rules": [{"tool": "Bash", "allow": true}, {"tool": "Write", "allow": false}],
        }))
        .unwrap();
        let policy = spec.to_policy();
        let decide = |tool: &str| {
            let request = PermissionRequest {
                key: PermissionKey("1".into()),
                tool: tool.into(),
                input: Value::Null,
            };
            matches!(
                policy.decide("b", &request),
                branchyard::PermissionDecision::Allow
            )
        };
        // The request's own rules come first.
        assert!(decide("Bash"));
        assert!(!decide("Write"));
        // Then the preset's.
        assert!(decide("Edit") && decide("Read"));
        assert!(!decide("WebFetch"));
        // Its default: deny.
        assert!(!decide("mcp__other"));
        let full = PolicySpec::preset(branchyard::PolicyPreset::Full).to_policy();
        let request = PermissionRequest {
            key: PermissionKey("1".into()),
            tool: "anything".into(),
            input: Value::Null,
        };
        assert_eq!(
            full.decide("b", &request),
            branchyard::PermissionDecision::Allow
        );
        let wire = serde_json::to_value(PolicySpec::preset(branchyard::PolicyPreset::ReadOnly));
        assert_eq!(
            wire.unwrap(),
            json!({"mode": "deny", "preset": "read-only"})
        );
        let unknown = serde_json::from_value::<PolicySpec>(json!({"preset": "yolo"}));
        assert!(unknown.unwrap_err().to_string().contains("unknown variant"));
    }

    #[test]
    fn budgets_refuse_nonpositive_values() {
        let ok = BudgetSpec {
            max_usd: Some(2.0),
            max_turns: Some(3),
            max_seconds: Some(1.5),
            ..BudgetSpec::default()
        };
        let budget = ok.to_budget().unwrap();
        assert_eq!(budget.max_duration, Some(Duration::from_millis(1500)));
        assert_eq!(BudgetSpec::from_budget(&budget), ok);
        for bad in [
            json!({"max_usd": 0}),
            json!({"max_usd": -1}),
            json!({"max_turns": 0}),
            json!({"max_seconds": 0}),
            json!({"max_seconds": 1e300}),
        ] {
            let spec: BudgetSpec = serde_json::from_value(bad.clone()).unwrap();
            assert!(spec.to_budget().is_err(), "{bad}");
        }
    }

    #[test]
    fn policies_apply_rules_before_the_mode() {
        let request = |tool: &str| PermissionRequest {
            key: PermissionKey("k".into()),
            tool: tool.into(),
            input: Value::Null,
        };
        let spec: PolicySpec = serde_json::from_value(json!({
            "mode": "deny",
            "rules": [{"tool": "Read", "allow": true}, {"tool": "mcp__*", "allow": false}]
        }))
        .unwrap();
        let policy = spec.to_policy();
        assert_eq!(
            policy.decide("b", &request("Read")),
            branchyard::PermissionDecision::Allow
        );
        assert!(matches!(
            policy.decide("b", &request("Bash")),
            branchyard::PermissionDecision::Deny { .. }
        ));
        assert_eq!(PolicySpec::default().mode, PolicyMode::Deny);
    }

    #[test]
    fn requests_reject_unknown_fields() {
        let error = serde_json::from_value::<TaskRequest>(json!({"prompt": "x", "harnes": "y"}))
            .unwrap_err();
        assert!(error.to_string().contains("harnes"), "{error}");
        let task: TaskRequest = serde_json::from_value(json!({"prompt": "x"})).unwrap();
        assert_eq!(
            task,
            TaskRequest {
                prompt: "x".into(),
                ..TaskRequest::default()
            }
        );
    }

    #[test]
    fn feed_entries_round_trip_sdk_activity() {
        let entry = FeedEntry {
            seq: 3,
            branch: "b".into(),
            at_ms: 9,
            activity: Activity::Status(BranchStatus::Failed { reason: "r".into() }),
        };
        let text = serde_json::to_string(&entry).unwrap();
        assert_eq!(serde_json::from_str::<FeedEntry>(&text).unwrap(), entry);
        assert!(
            text.contains(r#""status":{"state":"failed","reason":"r"}"#),
            "{text}"
        );
    }
}
