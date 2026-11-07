//! Durable console projects, task-backed workspaces and collaboration wire types.

use branchyard::ResultContract;
use serde::{Deserialize, Serialize};

/// A registered repository or operator-managed knowledge source.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProjectSource {
    /// Knowledge work with no browser-supplied filesystem path.
    Knowledge {},
    /// A repository already granted by the operator.
    Repository {
        /// Existing registered repository name.
        repo: String,
    },
}

/// Independent result acceptance choices.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultMode {
    /// Code integration.
    Code,
    /// Immutable deliverables, without requiring a Git diff.
    Artifacts,
    /// Code integration and artifact acceptance independently.
    Mixed,
}

/// An attempt has isolated writing or constrained review access.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAccess {
    /// An isolated writer attempt.
    Writer,
    /// A reviewer consuming immutable inputs.
    Reviewer,
}

/// Membership within existing tenant and repository policy.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Manage membership and edit the workspace.
    Owner,
    /// Start work and edit collaboration records.
    Editor,
    /// Review outputs and add comments.
    Reviewer,
    /// Read permitted workspace records.
    Viewer,
}

/// A stable project source and result policy.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    /// Stable project identifier.
    pub id: String,
    /// Human title.
    pub name: String,
    /// Source grant.
    pub source: ProjectSource,
    /// Acceptance policy.
    pub result_mode: ResultMode,
    /// Creation time in epoch milliseconds.
    pub created_at_ms: u64,
}

/// Create a project without accepting host paths.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateProjectRequest {
    /// Human title.
    pub name: String,
    /// Source choice.
    pub source: ProjectSource,
    /// Acceptance policy.
    pub result_mode: ResultMode,
}

/// Reserve a durable task-backed workspace.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkspaceRequest {
    /// Owning project.
    pub project_id: String,
    /// Human title.
    pub title: String,
}

/// Stable workspace metadata and execution references.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    /// Stable workspace identifier.
    pub id: String,
    /// Owning project.
    pub project_id: String,
    /// Human title.
    pub title: String,
    /// Checked collaboration revision.
    pub revision: u64,
    /// Derived execution or explicit lifecycle intent.
    pub state: String,
    /// Opaque repository resolver key.
    pub repo: String,
    /// Native task identifier.
    pub task_id: String,
    /// Independent code and deliverable acceptance policy.
    pub result_mode: ResultMode,
    /// Creation time in epoch milliseconds.
    pub created_at_ms: u64,
    /// Named native attempts.
    #[serde(default)]
    pub sessions: Vec<Session>,
    /// Workspace membership.
    #[serde(default)]
    pub members: Vec<Member>,
    /// Immutable original sources.
    #[serde(default)]
    pub sources: Vec<Source>,
    /// Accepted immutable artifact provenance records.
    #[serde(default)]
    pub accepted_deliverables: Vec<serde_json::Value>,
    /// Workspace result contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_contract: Option<ResultContract>,
    /// Verified subject observing this response, independent of device name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_subject: Option<String>,
    /// The observer's current workspace membership, intersected with server policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_role: Option<Role>,
    /// Server readiness, upload limits and the observer's permitted actions.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub capabilities: std::collections::BTreeMap<String, serde_json::Value>,
}

/// A named native attempt, without a transcript copy.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Stable session identifier.
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Human session title.
    pub name: String,
    /// Native attempt branch.
    pub branch: String,
    /// Selected harness.
    pub harness: String,
    /// Selected model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Derived execution status.
    pub status: String,
    /// Attempt access contract.
    pub access: SessionAccess,
    /// Current immutable candidate commit, derived from the native attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    /// Immutable candidate selected as this reviewer's input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_candidate: Option<String>,
    /// Native harness conversation identity, when the harness has recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session: Option<String>,
    /// Durable operation that admitted this session attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Current native result contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_contract: Option<ResultContract>,
    /// Schema-valid current-turn result; a present JSON null remains a result.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "branchyard::contract::present"
    )]
    pub result: Option<serde_json::Value>,
    /// Native event sequence proving the current result's provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_event: Option<u64>,
    /// Why a required current-turn result is missing or invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_error: Option<String>,
    /// Stable verified subject that admitted the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Execution preset the session started with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// What it was started with beyond its harness: `connectors` (each
    /// `connector`, `mode`, `account`), `skills` (unset: every built-in
    /// skill) and `helpers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<serde_json::Value>,
    /// The tenant's tool-policy version when the session was admitted, or
    /// last checked at a claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<u64>,
    /// The digest of the turn manifest the session runs with (what it was
    /// admitted with, resolved once; `docs/connectors.md#the-turn-manifest`),
    /// once its first turn has started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
}

/// Admit an isolated native session through the durable queue.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartSessionRequest {
    /// Explicitly acknowledge a harness that cannot enforce tool permissions.
    /// False by default, and refused unless the operator also permits it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unapproved_tools: bool,
    /// Existing typed execution backend; still subject to operator provider policy.
    /// Refused beside `preset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<branchyard::Provider>,
    /// Operator execution preset ID from `GET /v1/execution-presets`. The
    /// server supplies its provider, secrets and worker labels; the request
    /// may then not carry `provider` or `provision`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Human title.
    pub name: String,
    /// Initial instruction. With `issue`, instructions added to the issue's
    /// text, and may be empty (`""`).
    pub prompt: String,
    /// Start from this issue (a GitHub URL, `#N`, `linear:KEY`, a Linear,
    /// Jira or GitLab URL, ...): the server fetches it with its own `gh`
    /// login and tracker credentials and the prompt becomes the issue under
    /// a header. Refused with `issue_unavailable` and a `next` step when it
    /// cannot be fetched. See `GET /v1/workspaces/{id}/issue`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    /// Configured harness.
    pub harness: String,
    /// Model choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Supported model effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Explicit permission policy, using the existing task policy contract.
    /// Omitted policies retain the server's deny default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<crate::api::PolicySpec>,
    /// Existing provisioning contract, with operator-known secret names only.
    /// The server retains control of executable, connector and secret sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<branchyard::Provisioning>,
    /// Native current-turn result contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_contract: Option<ResultContract>,
    /// Writer or constrained reviewer.
    pub access: SessionAccess,
    /// Explicit selected immutable source identifiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<String>>,
    /// Frozen workspace artifact identifiers for reviewer input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<String>>,
    /// Exact immutable candidate commit from this workspace's native task.
    /// A reviewer may select this instead of artifact inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    /// Connectors this session may use, each as an object or the
    /// `CONNECTOR[@ACCOUNT][:MODE[:OPS]]` form. Works beside `preset`;
    /// refused beside `provision.connectors`. Each must be served by the
    /// gateway, connected by the caller and within the caller's ceiling.
    /// A reviewer's are read only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connectors: Vec<branchyard::connectors::GrantEntry>,
    /// Built-in skills to stage, by id, from `GET
    /// /v1/workspaces/{workspace}/tools`. Unset: every built-in skill.
    /// Connectors bring their own skills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Other harnesses this session may hand parts of its work to. Needs
    /// the server's `allow_delegation`; refused for a reviewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helpers: Option<Helpers>,
    /// Everything this session chooses, as one list: connectors (with
    /// `mode`, default `read`), built-in skills and helper harnesses. The
    /// server expands it into `connectors`, `skills` and `helpers`, so a
    /// kind left out of the list is not chosen (no `skill` entries stage no
    /// built-in skills). Refused beside any of those three fields. Each
    /// entry must be within the caller's effective tool policy
    /// (`tool_not_allowed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolChoice>>,
}

/// What a tool is.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// An upstream API through the connector gateway.
    Connector,
    /// A built-in skill.
    Skill,
    /// A harness a session may delegate to.
    Helper,
}

/// One tool a session or a workspace default chooses.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolChoice {
    /// What kind of tool.
    pub kind: ToolKind,
    /// The connector id, built-in skill id or harness id.
    pub id: String,
    /// A connector's mode: `read` (the default) or `write`. Refused on a
    /// skill or helper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<branchyard::connectors::GrantMode>,
    /// A connector's connected account; unset is the person's default.
    /// Refused on a skill or helper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The most helpers the session may have at once, 1 to 16. Only the
    /// helper entry with `id` `*` carries it (`{"kind": "helper", "id": "*",
    /// "max_children": 2}`); that entry names no harness, and alone it
    /// gives helpers that run the session's own harness. Unset is the
    /// default of 4, within the administrator's `max_helpers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_children: Option<u32>,
}

/// A tool named without its settings: what a policy turns off.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRef {
    /// What kind of tool.
    pub kind: ToolKind,
    /// The connector id, built-in skill id or harness id; `*` with kind
    /// `helper` stands for delegation as a whole.
    pub id: String,
}

/// What an administrator allows, at one level (tenant, group or person).
/// An unset field says nothing: no limit from this record.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    /// The widest connector grant, in the grant's form. An empty list
    /// allows no connector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connectors: Option<Vec<branchyard::connectors::GrantEntry>>,
    /// The built-in skill ids allowed. An empty list allows none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// The harness ids a session may delegate to. An empty list allows
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helpers: Option<Vec<String>>,
    /// Whether sessions may delegate at all. `false` turns helpers off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<bool>,
    /// The most helpers a session may have at once, 1 to 16 (default
    /// ceiling 16). Two records' limits narrow to the lesser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_helpers: Option<u32>,
    /// Tools switched off whatever the lists above allow. A person's own
    /// policy (`/v1/me/policy`) uses it to turn a tool off for themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off: Option<Vec<ToolRef>>,
}

/// One stored policy.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecord {
    /// `tenant:<tenant>`, `group:<tenant>:<group>` or
    /// `principal:<tenant>:<subject>`; a person's own is
    /// `self:<tenant>:<subject>`.
    pub key: String,
    /// The policy.
    pub policy: ToolPolicy,
    /// Compare-and-set revision, from one.
    pub revision: u64,
}

/// `GET /v1/admin/policies`: the caller's tenant's policies.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyList {
    /// Every stored policy of the caller's tenant, by key.
    pub policies: Vec<PolicyRecord>,
    /// The tenant's policy version: it rises by one with every change to any
    /// of its policies, and a session records the one it was admitted
    /// under.
    #[serde(default)]
    pub version: u64,
}

/// `PUT /v1/admin/policies/{key}`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutPolicyRequest {
    /// The new policy, replacing the old.
    pub policy: ToolPolicy,
    /// The revision being replaced; unset to create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// A workspace's tools that sessions start with.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefaults {
    /// The default tools.
    #[serde(default)]
    pub tools: Vec<ToolChoice>,
    /// Compare-and-set revision; zero until the first save.
    #[serde(default)]
    pub revision: u64,
}

/// `PUT /v1/workspaces/{workspace}/tools`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutToolDefaults {
    /// The default tools, replacing the old.
    pub tools: Vec<ToolChoice>,
    /// The revision being replaced; zero (or unset) before the first save.
    #[serde(default)]
    pub revision: u64,
}

/// How far a tool is along the path to use: the first state it fails, else
/// `ready`, or `default` when the workspace starts sessions with it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolState {
    /// Policy does not allow it for this person.
    NotAllowed,
    /// Allowed, but not set up for this person.
    NotReady,
    /// Allowed and set up.
    Ready,
    /// Ready, and sessions here start with it.
    Default,
}

/// What a person can do about a tool that is not ready.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAction {
    /// Ask an administrator to allow it.
    AskAdmin,
    /// Connect an account.
    Connect,
    /// Turn it back on in Settings > Tools: the person switched it off.
    TurnOn,
    /// Install it.
    Install,
    /// Sign in.
    SignIn,
}

/// One tool, as the caller sees it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEntry {
    /// What kind of tool.
    pub kind: ToolKind,
    /// The id a `tools` entry takes.
    pub id: String,
    /// A display name.
    pub name: String,
    /// What it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A connector's icon symbol in the console's sprite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// A skill's topic heading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Where it stands.
    pub state: ToolState,
    /// Policy allows it for this caller.
    pub allowed: bool,
    /// It is set up for this caller (never true when not allowed).
    pub ready: bool,
    /// Sessions in the workspace start with it.
    pub default: bool,
    /// A connector's most the caller may do: `read` or `write`.
    #[serde(default)]
    pub max_mode: Option<branchyard::connectors::GrantMode>,
    /// A connector's accounts for the caller.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<ConnectedAccount>,
    /// What the caller can do next, if anything.
    #[serde(default)]
    pub action: Option<ToolAction>,
    /// One plain sentence when it is not ready.
    #[serde(default)]
    pub reason: Option<String>,
    /// The caller switched it off for themselves (their own policy,
    /// `/v1/me/policy`). `allowed` is then false, `state` is `not_allowed`
    /// and `action` is `turn_on`; the administrator's policy would allow it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub off_by_me: bool,
    /// A helper's harness can be installed on the server's host by an
    /// administrator now (`POST /v1/tools/helpers/{id}/install`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub installable: bool,
}

/// `GET /v1/tools`: every tool of the three kinds.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCatalogue {
    /// Whether the connector gateway answers.
    pub gateway: GatewayState,
    /// One plain sentence when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The most children a session's helpers may have at once for the
    /// caller: 16, or the administrator's lower `max_helpers`.
    pub max_children: u32,
    /// The children a session has when its `tools` give no number.
    #[serde(default)]
    pub default_children: u32,
    /// Connectors, then skills, then helpers.
    pub tools: Vec<ToolEntry>,
}

/// The harnesses a session may delegate to, one level deep.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Helpers {
    /// Harness IDs its children may run. Empty: only its own harness.
    #[serde(default)]
    pub harnesses: Vec<String>,
    /// Children it may have at once. Default 4, at most 16.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_children: Option<u32>,
}

/// Whether the connector gateway answers.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayState {
    /// It answered.
    Ready,
    /// This server has no `[connectors]` gateway.
    NotConfigured,
    /// It is configured and did not answer.
    Unreachable,
}

/// Where the caller stands with one connector.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    /// An account is connected and usable.
    Connected,
    /// The provider refused a refresh; connect again.
    NeedsReconnect,
    /// No account is connected.
    NotConnected,
    /// The caller's ceiling allows nothing of it.
    Blocked,
    /// The gateway's status is not known.
    Unknown,
}

/// One connected account, without any secret.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectedAccount {
    /// The account name; `default` unless the caller named one.
    pub account: String,
    /// `connected` or `needs_reconnect`.
    pub state: ConnectionState,
    /// `oauth` or `static` (an API key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// When the gateway last changed it, as it reports the time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// One connector the gateway serves, as the caller sees it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    /// The connector ID a grant names.
    pub connector: String,
    /// Its display name.
    pub name: String,
    /// What it gives a session access to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The symbol in the console's `connector-icons.svg` that draws its
    /// mark (`ci-<id>`), when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The caller's overall state with it.
    pub state: ConnectionState,
    /// The caller's accounts.
    #[serde(default)]
    pub accounts: Vec<ConnectedAccount>,
    /// The most a session may do with it under the caller's ceiling;
    /// absent when blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_mode: Option<branchyard::connectors::GrantMode>,
    /// One plain sentence when it cannot be used now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `GET /v1/connections`: the caller's connections.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionList {
    /// Whether the gateway answered.
    pub gateway: GatewayState,
    /// One plain sentence when it is not ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Every connector it serves.
    pub connections: Vec<Connection>,
}

/// `POST /v1/connections/{connector}/connect`.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectRequest {
    /// The account to connect; `default` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// How to finish connecting.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnectStart {
    /// Open `url` to sign in with the provider; it expires at
    /// `expires_at` (epoch milliseconds).
    Oauth {
        /// The provider's authorization URL.
        url: String,
        /// When the sign-in expires.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<u64>,
    },
    /// The connector takes an API key: send it to
    /// `POST /v1/connections/{connector}/key`.
    Key,
}

/// `POST /v1/connections/{connector}/key`: an API key or personal token
/// for a key-based connector. The server passes it to the gateway's vault
/// once and keeps no copy.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyRequest {
    /// The key.
    pub api_key: String,
    /// The account to store it as; `default` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

impl std::fmt::Debug for ApiKeyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyRequest")
            .field("api_key", &"<redacted>")
            .field("account", &self.account)
            .finish()
    }
}

/// One built-in skill a session may choose.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillOption {
    /// The ID `skills` takes.
    pub id: String,
    /// What it does.
    pub description: String,
    /// Its topic's heading.
    pub topic: String,
    /// The skills it brings with it.
    #[serde(default)]
    pub requires: Vec<String>,
}

/// Whether a session may delegate, and how widely.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperOptions {
    /// Whether `helpers` is accepted.
    pub allowed: bool,
    /// One plain sentence when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The most `max_children` may be.
    pub max_children: u32,
}

/// `GET /v1/workspaces/{workspace}/tools`: what the caller may choose for
/// a session here. Advice for the form; starting a session checks again.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTools {
    /// The caller's connections.
    pub connections: ConnectionList,
    /// The built-in skills, in catalogue order.
    pub skills: Vec<SkillOption>,
    /// Delegation.
    pub helpers: HelperOptions,
    /// The tools sessions in this workspace start with.
    #[serde(default)]
    pub defaults: ToolDefaults,
}

/// Continue, steer or resume one native session.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageRequest {
    /// User instruction.
    pub prompt: String,
    /// send, steer or resume; omitted means send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// Text derived from native conversation or event records.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Stable native record identifier.
    pub id: String,
    /// Native message role.
    pub role: String,
    /// Message text.
    pub text: String,
    /// Native record time in epoch milliseconds.
    pub time: u64,
}

/// Native messages after a cursor.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessagePage {
    /// Native conversation messages.
    pub messages: Vec<Message>,
    /// Cursor for the next read.
    pub next_cursor: String,
}

/// An immutable original uploaded before or during work.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// Immutable source identifier.
    pub id: String,
    /// Source title.
    pub title: String,
    /// Declared media type.
    pub media_type: String,
    /// SHA-256 digest of original bytes.
    pub digest: String,
    /// Original byte length.
    pub size_bytes: u64,
    /// Upload time in epoch milliseconds.
    pub created_at_ms: u64,
}

/// Accept deliverables at a checked workspace revision.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizeRequest {
    /// Expected workspace revision.
    pub revision: u64,
    /// Immutable eligible artifact identifiers.
    pub artifacts: Vec<String>,
}

/// Attributed membership bound to a stable authenticated subject.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Member {
    /// Owning workspace.
    pub workspace_id: String,
    /// Verified stable subject, separate from device name.
    pub subject: String,
    /// Workspace role.
    pub role: Role,
}

/// Change membership at a checked revision.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberRequest {
    /// Expected workspace revision.
    pub revision: u64,
    /// New role.
    pub role: Role,
}

/// An attributed collaboration comment.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    /// Stable comment identifier.
    pub id: String,
    /// Verified author subject.
    pub subject: String,
    /// Acting credential display name.
    pub credential: String,
    /// Comment text.
    pub text: String,
    /// Creation time in epoch milliseconds.
    pub created_at_ms: u64,
}

/// Add an attributed comment.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommentRequest {
    /// Expected workspace revision.
    pub revision: u64,
    /// Comment text.
    pub text: String,
}

/// Expiring advisory presence, never a writer lock.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Presence {
    /// Verified subject.
    pub subject: String,
    /// Expiry in epoch milliseconds.
    pub expires_at_ms: u64,
}

/// An existing durable operation with discoverable console ownership.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceOperation {
    /// Existing operation envelope, serialized without another nesting layer.
    #[serde(flatten)]
    pub operation: crate::api::Operation,
    /// Stable owning workspace.
    pub workspace_id: String,
    /// Named native attempt, when this command acts on one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Where an execution preset came from.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetOrigin {
    /// Written by the operator in `execution_presets`.
    Operator,
    /// The implicit `local` preset of a server that configures none.
    Default,
}

/// What a readiness value found.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyState {
    /// Checked and usable.
    Ready,
    /// Checked and not usable from where it was measured.
    NotReady,
    /// Not checked before a session starts.
    Unknown,
}

/// Where a readiness value was measured.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessScope {
    /// The server answering the request, which may not be the worker that
    /// runs the session.
    ApiHost,
    /// The preset's execution environment.
    Environment,
}

/// Whether a preset supplies a harness's key.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetCredentials {
    /// The preset supplies a secret the harness accepts as its key.
    Configured,
    /// The preset supplies no key for the harness.
    None,
}

/// One readiness observation. It informs a person and never refuses a start.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readiness {
    /// What was found.
    pub state: ReadyState,
    /// Where it was measured.
    pub scope: ReadinessScope,
    /// Fixed text chosen by the server; never an error message, path,
    /// endpoint or secret name.
    pub reason: String,
    /// Fixed text saying what an operator can do, when anything.
    pub action: Option<String>,
    /// Age of a cached check, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_ms_ago: Option<u64>,
}

/// A harness an execution preset offers.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetHarnessView {
    /// Harness or profile ID a session start names.
    pub harness: String,
    /// Models a session may choose; absent means it may not choose one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    /// Model a session that names none runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// The harness routes tool permission requests to the session's policy.
    pub tool_approvals: bool,
    /// A writer session can start on it.
    pub writer: bool,
    /// A reviewer session can start on it.
    pub reviewer: bool,
    /// A session may choose an effort.
    pub effort: bool,
    /// Whether the preset supplies the harness's key.
    pub credentials: PresetCredentials,
    /// Whether the harness's executable is where the session runs.
    pub executable: Readiness,
    /// Whether the harness can sign in; verified when a session starts.
    pub auth: Readiness,
}

/// An execution preset as the console offers it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPresetView {
    /// ID a session start names.
    pub id: String,
    /// Plain-language name.
    pub label: String,
    /// Plain-language description.
    pub description: String,
    /// Provider kind: `local`, `substrate` or `microsandbox`.
    pub provider: String,
    /// Operator-written or the implicit default.
    pub origin: PresetOrigin,
    /// Whether the caller's workspace role can start a session on it; only
    /// when the request named a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_start: Option<bool>,
    /// Harnesses it offers.
    pub harnesses: Vec<PresetHarnessView>,
    /// Whether the environment's settings are usable.
    pub environment: Readiness,
    /// Whether the environment can be reached; only for remote ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reachability: Option<Readiness>,
}

/// `GET /v1/execution-presets` response.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPresetList {
    /// Workspace session starts must name a preset.
    pub required: bool,
    /// Presets offered to the caller's tenant.
    pub presets: Vec<ExecutionPresetView>,
}

/// Project listing response.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectList {
    /// Authorized projects.
    pub projects: Vec<Project>,
}

/// Workspace listing response.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceList {
    /// Authorized workspace summaries.
    pub workspaces: Vec<Workspace>,
}

/// Session listing response.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionList {
    /// Named native attempts.
    pub sessions: Vec<Session>,
}

impl crate::Client {
    /// List projects available to the authenticated caller.
    pub fn projects(&self) -> Result<Vec<Project>, crate::Error> {
        Ok(self.get::<ProjectList>("/v1/projects")?.projects)
    }

    /// Read one project by its stable identifier.
    pub fn project(&self, id: &str) -> Result<Project, crate::Error> {
        self.get(&format!("/v1/projects/{}", crate::http::encode(id)))
    }

    /// What waits for a person in one project's workspaces, in the order to
    /// take it: outputs waiting to be accepted, then sessions that stopped
    /// ([`crate::effects_api::DecisionList`]).
    pub fn project_decisions(
        &self,
        id: &str,
    ) -> Result<Vec<crate::effects_api::Decision>, crate::Error> {
        Ok(self
            .get::<crate::effects_api::DecisionList>(&format!(
                "/v1/projects/{}/decisions",
                crate::http::encode(id)
            ))?
            .decisions)
    }

    /// Create a project using a durable idempotency key.
    pub fn create_project(
        &self,
        request: &CreateProjectRequest,
        key: &str,
    ) -> Result<Project, crate::Error> {
        self.post("/v1/projects", request, key)
    }

    /// List workspace summaries, optionally within a project.
    pub fn workspaces(&self, project: Option<&str>) -> Result<Vec<Workspace>, crate::Error> {
        let path = project.map_or_else(
            || "/v1/workspaces".to_owned(),
            |id| format!("/v1/workspaces?project={}", crate::http::encode(id)),
        );
        Ok(self.get::<WorkspaceList>(&path)?.workspaces)
    }

    /// Read workspace detail and its immutable source and result references.
    pub fn workspace(&self, id: &str) -> Result<Workspace, crate::Error> {
        self.get(&format!("/v1/workspaces/{}", crate::http::encode(id)))
    }

    /// Reserve a workspace and return the existing durable initialization operation.
    pub fn create_workspace(
        &self,
        request: &CreateWorkspaceRequest,
        key: &str,
    ) -> Result<WorkspaceOperation, crate::Error> {
        self.post("/v1/workspaces", request, key)
    }

    /// The issue `reference` names as a session with
    /// `StartSessionRequest::issue` would start from it, fetched with the
    /// server's own `gh` login and tracker credentials. Starts nothing.
    pub fn workspace_issue(
        &self,
        workspace: &str,
        reference: &str,
    ) -> Result<crate::api::IssuePreview, crate::Error> {
        self.get(&format!(
            "/v1/workspaces/{}/issue?ref={}",
            crate::http::encode(workspace),
            crate::http::encode(reference)
        ))
    }

    /// List isolated native attempts within a workspace.
    pub fn workspace_sessions(&self, workspace: &str) -> Result<Vec<Session>, crate::Error> {
        Ok(self
            .get::<SessionList>(&format!(
                "/v1/workspaces/{}/sessions",
                crate::http::encode(workspace)
            ))?
            .sessions)
    }

    /// Execution presets offered to the caller's tenant. With a workspace,
    /// the caller must be a member and each preset says whether its role
    /// can start a session there.
    pub fn execution_presets(
        &self,
        workspace: Option<&str>,
    ) -> Result<ExecutionPresetList, crate::Error> {
        let path = workspace.map_or_else(
            || "/v1/execution-presets".to_owned(),
            |id| {
                format!(
                    "/v1/execution-presets?workspace={}",
                    crate::http::encode(id)
                )
            },
        );
        self.get(&path)
    }

    /// The caller's connections at the connector gateway, without secrets.
    pub fn connections(&self) -> Result<ConnectionList, crate::Error> {
        self.get("/v1/connections")
    }

    /// Start connecting `connector` as the caller: a sign-in URL, or word
    /// that it takes a key ([`Self::connect_key`]).
    pub fn connect(
        &self,
        connector: &str,
        request: &ConnectRequest,
        key: &str,
    ) -> Result<ConnectStart, crate::Error> {
        self.post(
            &format!("/v1/connections/{}/connect", crate::http::encode(connector)),
            request,
            key,
        )
    }

    /// Store an API key for a key-based connector in the gateway's vault.
    pub fn connect_key(
        &self,
        connector: &str,
        request: &ApiKeyRequest,
        key: &str,
    ) -> Result<serde_json::Value, crate::Error> {
        self.post(
            &format!("/v1/connections/{}/key", crate::http::encode(connector)),
            request,
            key,
        )
    }

    /// What a session in `workspace` may choose: connections, built-in
    /// skills and helpers.
    pub fn session_tools(&self, workspace: &str) -> Result<SessionTools, crate::Error> {
        self.get(&format!(
            "/v1/workspaces/{}/tools",
            crate::http::encode(workspace)
        ))
    }

    /// Every connector, skill and helper as the caller sees it, with the
    /// workspace's defaults when `workspace` is given.
    pub fn tools(&self, workspace: Option<&str>) -> Result<ToolCatalogue, crate::Error> {
        match workspace {
            Some(id) => self.get(&format!("/v1/tools?workspace={}", crate::http::encode(id))),
            None => self.get("/v1/tools"),
        }
    }

    /// Replace the tools sessions in `workspace` start with. The workspace's
    /// owner or an editor may.
    pub fn workspace_tool_defaults(
        &self,
        workspace: &str,
        request: &PutToolDefaults,
    ) -> Result<ToolDefaults, crate::Error> {
        self.put(
            &format!("/v1/workspaces/{}/tools", crate::http::encode(workspace)),
            request,
        )
    }

    /// The tool policies of the caller's tenant (`admin` scope).
    pub fn policies(&self) -> Result<PolicyList, crate::Error> {
        self.get("/v1/admin/policies")
    }

    /// Create or replace the policy at `key` (`admin` scope).
    pub fn put_policy(
        &self,
        key: &str,
        request: &PutPolicyRequest,
    ) -> Result<PolicyRecord, crate::Error> {
        self.put(
            &format!("/v1/admin/policies/{}", crate::http::encode(key)),
            request,
        )
    }

    /// Delete the policy at `key` (`admin` scope); `revision` guards it.
    pub fn delete_policy(
        &self,
        key: &str,
        revision: Option<u64>,
    ) -> Result<serde_json::Value, crate::Error> {
        let mut path = format!("/v1/admin/policies/{}", crate::http::encode(key));
        if let Some(revision) = revision {
            path.push_str(&format!("?revision={revision}"));
        }
        self.delete(&path)
    }

    /// The caller's own policy: what they have switched off for themselves.
    /// An empty policy at revision 0 before the first save.
    pub fn my_policy(&self) -> Result<PolicyRecord, crate::Error> {
        self.get("/v1/me/policy")
    }

    /// Replace the caller's own policy. It can only narrow what the
    /// administrator's allows; `request.revision` is the saved one (unset
    /// or 0 before the first save).
    pub fn put_my_policy(&self, request: &PutPolicyRequest) -> Result<PolicyRecord, crate::Error> {
        self.put("/v1/me/policy", request)
    }

    /// Ask the server to install helper `harness` on its own host (`admin`
    /// scope, and `harness_install.allow` in the server's configuration).
    /// Returns at once; follow it with `helper_install`.
    pub fn install_helper(&self, harness: &str) -> Result<HelperInstall, crate::Error> {
        self.post(
            &format!("/v1/tools/helpers/{}/install", crate::http::encode(harness)),
            &serde_json::json!({}),
            &crate::new_key(),
        )
    }

    /// Where the latest install of helper `harness` on the server stands.
    pub fn helper_install(&self, harness: &str) -> Result<HelperInstall, crate::Error> {
        self.get(&format!(
            "/v1/tools/helpers/{}/install",
            crate::http::encode(harness)
        ))
    }

    /// Start a named attempt through the existing operation queue.
    pub fn start_workspace_session(
        &self,
        workspace: &str,
        request: &StartSessionRequest,
        key: &str,
    ) -> Result<WorkspaceOperation, crate::Error> {
        self.post(
            &format!("/v1/workspaces/{}/sessions", crate::http::encode(workspace)),
            request,
            key,
        )
    }

    /// Retrieve native conversation text without a console transcript copy.
    pub fn workspace_messages(
        &self,
        workspace: &str,
        session: &str,
        after: Option<&str>,
    ) -> Result<MessagePage, crate::Error> {
        let mut path = format!(
            "/v1/workspaces/{}/sessions/{}/messages",
            crate::http::encode(workspace),
            crate::http::encode(session)
        );
        if let Some(cursor) = after {
            path.push_str(&format!("?after={}", crate::http::encode(cursor)));
        }
        self.get(&path)
    }

    /// Continue, steer or resume one native conversation.
    pub fn send_workspace_message(
        &self,
        workspace: &str,
        session: &str,
        request: &MessageRequest,
        key: &str,
    ) -> Result<WorkspaceOperation, crate::Error> {
        self.post(
            &format!(
                "/v1/workspaces/{}/sessions/{}/messages",
                crate::http::encode(workspace),
                crate::http::encode(session)
            ),
            request,
            key,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_never_accept_browser_paths() {
        assert!(serde_json::from_value::<ProjectSource>(serde_json::json!({
            "kind": "knowledge", "path": "/private/source"
        }))
        .is_err());
        assert_eq!(
            serde_json::to_value(ProjectSource::Knowledge {}).unwrap(),
            serde_json::json!({"kind": "knowledge"})
        );
    }

    #[test]
    fn session_policy_and_provisioning_use_existing_task_contracts() {
        let request:StartSessionRequest=serde_json::from_value(serde_json::json!({"name":"Writer","prompt":"Use the supplied source","harness":"gemini-cli","access":"writer","policy":{"preset":"read-only"},"provision":{"secrets":[{"name":"OPERATOR_KNOWN"}],"auth":"api-key","model":"configured-model"}})).unwrap();
        assert_eq!(
            request.policy.unwrap().preset,
            Some(branchyard::PolicyPreset::ReadOnly)
        );
        let provision = request.provision.unwrap();
        assert_eq!(provision.secrets[0].name, "OPERATOR_KNOWN");
        assert!(provision.secrets[0].from.is_none());
        assert_eq!(provision.model.as_deref(), Some("configured-model"));
        let minimal:StartSessionRequest=serde_json::from_value(serde_json::json!({"name":"Writer","prompt":"Read evidence","harness":"gemini-cli","access":"writer"})).unwrap();
        assert!(minimal.policy.is_none());
        assert!(minimal.provision.is_none());
    }

    #[test]
    fn session_result_distinguishes_present_null_from_an_absent_result() {
        let mut value = serde_json::json!({"id":"session","workspace_id":"workspace","name":"Writer","branch":"attempt","harness":"gemini-cli","status":"no_changes","access":"writer"});
        let absent: Session = serde_json::from_value(value.clone()).unwrap();
        assert!(absent.result.is_none());
        assert!(absent.result_event.is_none());
        assert!(serde_json::to_value(absent)
            .unwrap()
            .get("result")
            .is_none());
        value["result"] = serde_json::Value::Null;
        value["result_event"] = serde_json::json!(42);
        value["native_session"] = serde_json::json!("native-conversation");
        value["subject"] = serde_json::json!("verified-subject");
        let present: Session = serde_json::from_value(value).unwrap();
        assert_eq!(present.result, Some(serde_json::Value::Null));
        assert_eq!(present.result_event, Some(42));
        assert_eq!(
            present.native_session.as_deref(),
            Some("native-conversation")
        );
        assert_eq!(present.subject.as_deref(), Some("verified-subject"));
        let serialized = serde_json::to_value(&present).unwrap();
        assert_eq!(serialized.get("result"), Some(&serde_json::Value::Null));
        assert_eq!(
            serde_json::from_value::<Session>(serialized).unwrap(),
            present
        );
    }

    #[test]
    fn reviewer_candidate_round_trips_as_an_immutable_native_reference() {
        let candidate = "0123456789abcdef0123456789abcdef01234567";
        let value = serde_json::json!({"name":"Code reviewer","prompt":"Review this frozen candidate","harness":"gemini-cli","access":"reviewer","candidate":candidate});
        let request: StartSessionRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(request.access, SessionAccess::Reviewer);
        assert_eq!(request.candidate.as_deref(), Some(candidate));
        assert_eq!(serde_json::to_value(request).unwrap(), value);
    }

    #[test]
    fn execution_presets_round_trip_as_the_contract_writes_them() {
        let value = serde_json::json!({"required":true,"presets":[{
            "id":"substrate-gke","label":"Substrate on GKE","description":"Runs each session in its own Substrate actor.",
            "provider":"substrate","origin":"operator","can_start":true,
            "harnesses":[{"harness":"antigravity","models":["gemini-3.8-flash-low"],"default_model":"gemini-3.8-flash-low",
                "tool_approvals":false,"writer":true,"reviewer":false,"effort":false,"credentials":"configured",
                "executable":{"state":"unknown","scope":"environment","reason":"The Substrate template provides the harness; discovery does not contact the cluster.","action":null},
                "auth":{"state":"unknown","scope":"environment","reason":"Credentials are configured on the server; they are verified when a session starts.","action":null}}],
            "environment":{"state":"ready","scope":"api_host","checked_ms_ago":12000,"reason":"This server can read the Substrate settings and credential files.","action":null},
            "reachability":{"state":"unknown","scope":"environment","reason":"The cluster is contacted when a session starts, not before.","action":null}}]});
        let list: ExecutionPresetList = serde_json::from_value(value.clone()).unwrap();
        let preset = &list.presets[0];
        assert_eq!(preset.origin, PresetOrigin::Operator);
        assert_eq!(preset.environment.state, ReadyState::Ready);
        assert_eq!(preset.environment.scope, ReadinessScope::ApiHost);
        assert_eq!(preset.environment.checked_ms_ago, Some(12000));
        assert_eq!(
            preset.harnesses[0].credentials,
            PresetCredentials::Configured
        );
        assert_eq!(serde_json::to_value(&list).unwrap(), value);
        let local: ExecutionPresetView = serde_json::from_value(serde_json::json!({
            "id":"local","label":"This server","description":"","provider":"local","origin":"default","harnesses":[],
            "environment":{"state":"ready","scope":"api_host","reason":"Sessions run here.","action":null}})).unwrap();
        assert!(local.can_start.is_none() && local.reachability.is_none());
        let start: StartSessionRequest = serde_json::from_value(serde_json::json!({
            "name":"Analyst","prompt":"Summarise","preset":"substrate-gke","harness":"antigravity","access":"writer"})).unwrap();
        assert_eq!(start.preset.as_deref(), Some("substrate-gke"));
        assert_eq!(
            serde_json::to_value(&start).unwrap()["preset"],
            "substrate-gke"
        );
        let session: Session = serde_json::from_value(serde_json::json!({
            "id":"s","workspace_id":"w","name":"Analyst","branch":"s","harness":"antigravity","status":"queued",
            "access":"writer","preset":"substrate-gke"})).unwrap();
        assert_eq!(session.preset.as_deref(), Some("substrate-gke"));
    }

    #[test]
    fn result_and_access_choices_follow_console_contract() {
        assert_eq!(
            serde_json::to_string(&ResultMode::Artifacts).unwrap(),
            "\"artifacts\""
        );
        assert_eq!(
            serde_json::to_string(&SessionAccess::Reviewer).unwrap(),
            "\"reviewer\""
        );
        let request: CreateWorkspaceRequest = serde_json::from_value(serde_json::json!({
            "project_id":"project", "title":"Research"
        }))
        .unwrap();
        assert_eq!(request.title, "Research");
    }
}

/// Where a helper harness's install on the server's host stands.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallState {
    /// Accepted; waiting to start.
    Queued,
    /// The install command is running.
    Running,
    /// The harness was found after the install.
    Succeeded,
    /// The command failed, or the harness was not found afterwards.
    Failed,
}

impl InstallState {
    /// Whether it has ended.
    pub fn is_terminal(self) -> bool {
        matches!(self, InstallState::Succeeded | InstallState::Failed)
    }
}

/// `POST` and `GET /v1/tools/helpers/{id}/install`: one install of a
/// helper's harness. It runs on the server's own host only; a worker on
/// another machine keeps what it has.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperInstall {
    /// The harness being installed.
    pub harness: String,
    /// Where it stands.
    pub state: InstallState,
    /// The machine it installs on: always `server` for now.
    pub host: String,
    /// The shell command the catalog gave, once chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// When it was accepted, in epoch milliseconds.
    pub created_at_ms: u64,
    /// When it ended, in epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// The end of the command's output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
}
