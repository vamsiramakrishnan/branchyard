//! Wire types of the server API, shared by the server and this client.
//!
//! Branches, statuses and recorded events use the `branchyard` SDK's own
//! serde forms, so a remote caller gets the same values a local one does.
//! Request types reject unknown fields: a misspelled option is an error,
//! not a silently ignored one.

use std::time::Duration;

use branchyard::{
    Activity, BranchInfo, Budget, Envelope, HarnessInfo, Inspection, Merged, Policy, Provider,
    Provisioning, RecordedEvent, Seats,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Limits for a branch; see [`branchyard::Budget`].
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
}

impl BudgetSpec {
    pub fn from_budget(budget: &Budget) -> Self {
        BudgetSpec {
            max_usd: budget.max_usd,
            max_turns: budget.max_turns,
            max_seconds: budget.max_duration.map(|d| d.as_secs_f64()),
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
        Ok(Budget {
            max_usd: self.max_usd,
            max_turns: self.max_turns,
            max_duration,
        })
    }
}

/// How requests no rule decides are answered. There is no remote `ask`:
/// a server has no terminal to ask on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    Allow,
    #[default]
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    /// Exact tool name, or a `*` suffix wildcard such as `mcp__*`.
    pub tool: String,
    pub allow: bool,
}

/// A permission policy: rules in order, then the mode. Defaults to deny.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySpec {
    #[serde(default)]
    pub mode: PolicyMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RuleSpec>,
}

impl PolicySpec {
    pub fn allow_all() -> Self {
        PolicySpec {
            mode: PolicyMode::Allow,
            rules: Vec::new(),
        }
    }

    pub fn to_policy(&self) -> Policy {
        let base = match self.mode {
            PolicyMode::Allow => Policy::allow_all(),
            PolicyMode::Deny => Policy::deny_all(),
        };
        self.rules
            .iter()
            .fold(base, |policy, rule| match rule.allow {
                true => policy.allow(rule.tool.clone()),
                false => policy.deny(rule.tool.clone()),
            })
    }
}

/// `POST /v1/repos/{repo}/tasks`. With `harnesses`, runs one branch per
/// harness like [`branchyard::TaskBuilder::run_on`]; otherwise one branch.
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
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// `POST /v1/repos/{repo}/branches/{branch}/send`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendRequest {
    pub prompt: String,
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
}

/// `POST /v1/repos/{repo}/branches/{branch}/fork`.
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
}

/// `POST /v1/repos/{repo}/branches/{branch}/merge`. Without a target, the
/// branch checked out in the served repository.
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
}

/// `POST /v1/repos/{repo}/branches/{branch}/integrate`: merge a delegated
/// child into the parent that delegated it, like `by integrate`. No fields
/// yet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrateRequest {}

/// `POST /v1/repos/{repo}/branches/{branch}/cancel`: no fields yet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {}

/// The branches a cancel asked to stop: the branch and its delegated
/// descendants that were running a turn.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelResult {
    pub cancelled: Vec<String>,
}

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
}

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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OperationResult {
    /// Branches a task, send or fork ran, in request order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<BranchInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged: Option<Merged>,
    /// Every branch the operation's branches delegated to, directly or
    /// below, once they finished: the operation waits for them, as
    /// `by run` does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub descendants: Vec<BranchInfo>,
    /// A spawned child, inspected once its turn ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspection: Option<Inspection>,
}

/// A long operation, run in the background. Durable on the server from
/// the moment it is returned.
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
}

/// A structured error. `code` is stable; `message` is for people.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

/// Every error response is `{"error": {...}}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

/// One entry of a repository's activity feed, as streamed.
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchEvents {
    pub events: Vec<RecordedEvent>,
    pub cursor: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchList {
    pub branches: Vec<BranchInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub diff: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    pub removed: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    pub name: String,
    /// The repository root on the server.
    pub root: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoList {
    pub repos: Vec<RepoEntry>,
}

/// A harness profile on the wire: [`branchyard::HarnessInfo`]'s own serde
/// form. `available` is about the server's `PATH`.
pub type HarnessEntry = HarnessInfo;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessList {
    pub harnesses: Vec<HarnessInfo>,
}

/// A merge on the wire: [`branchyard::Merged`]'s own serde form.
pub type MergedInfo = Merged;

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::{BranchStatus, PermissionKey, PermissionRequest};
    use serde_json::json;

    #[test]
    fn budgets_refuse_nonpositive_values() {
        let ok = BudgetSpec {
            max_usd: Some(2.0),
            max_turns: Some(3),
            max_seconds: Some(1.5),
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
