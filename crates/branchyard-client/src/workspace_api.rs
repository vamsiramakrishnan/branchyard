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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<branchyard::Provider>,
    /// Human title.
    pub name: String,
    /// Initial instruction.
    pub prompt: String,
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

    /// List isolated native attempts within a workspace.
    pub fn workspace_sessions(&self, workspace: &str) -> Result<Vec<Session>, crate::Error> {
        Ok(self
            .get::<SessionList>(&format!(
                "/v1/workspaces/{}/sessions",
                crate::http::encode(workspace)
            ))?
            .sessions)
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
