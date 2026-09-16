//! Versioned, transport-independent control contract. No execution or authorization lives here.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use uuid::Uuid;

pub const MAX_REQUEST_BYTES: usize = 256 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_GRAPH_EDITS: usize = 64;
pub const MAX_EVENT_PAGE: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Version {
    #[serde(rename = "branchyard/v1alpha1")]
    V1Alpha1,
}

macro_rules! identity {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name {
            pub fn new() -> Self { Self(Uuid::new_v4()) }
        }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
        }
        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> { Uuid::parse_str(s).map(Self) }
        }
    )+};
}
identity!(
    OperationId,
    TaskId,
    RunId,
    AttemptId,
    SessionId,
    WorkspaceId,
    SandboxId,
    CheckpointId,
    ArtifactId
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub schema: Version,
    /// Caller-generated and persisted BEFORE submission. Also the idempotency key.
    pub operation_id: OperationId,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    CreateTask {
        task_id: TaskId,
        spec: TaskSpec,
    },
    ApplyGraph {
        root_id: TaskId,
        expected_revision: u64,
        edits: Vec<GraphEdit>,
    },
    CancelTask {
        task_id: TaskId,
        expected_revision: u64,
        cascade: bool,
    },
}

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Self::CreateTask { .. } => "create_task",
            Self::ApplyGraph { .. } => "apply_graph",
            Self::CancelTask { .. } => "cancel_task",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub goal: String,
    /// Server registry IDs, never executable paths or shell commands.
    pub harness_profile: String,
    pub environment_profile: String,
    pub policy_profile: String,
    pub workspace: WorkspaceSource,
    pub components: Vec<ComponentBinding>,
    pub required_capabilities: BTreeSet<String>,
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceSource {
    Repository {
        repository_id: String,
        commit: String,
    },
    /// Storage fork only; does not imply native conversation or full VM fork.
    Checkpoint { checkpoint_id: CheckpointId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ComponentBinding {
    pub component_id: String,
    pub access: ComponentAccess,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComponentAccess {
    ReadOnly,
    ExclusiveWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_children: u16,
    pub max_depth: u16,
    pub wall_seconds: u32,
    pub cpu_millis: u32,
    pub memory_mib: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GraphEdit {
    Spawn {
        task_id: TaskId,
        parent_id: TaskId,
        spec: Box<TaskSpec>,
    },
    AddDependency {
        task_id: TaskId,
        depends_on: TaskId,
    },
    RemoveDependency {
        task_id: TaskId,
        depends_on: TaskId,
    },
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct Invalid(pub &'static str);

fn non_nil(id: Uuid) -> Result<(), Invalid> {
    if id.is_nil() {
        Err(Invalid("nil identity"))
    } else {
        Ok(())
    }
}
fn bounded(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.len() <= max && !s.contains('\0')
}
fn name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:/".contains(&b))
}
impl TaskSpec {
    pub fn validate(&self) -> Result<(), Invalid> {
        if !bounded(&self.goal, 16 * 1024) {
            return Err(Invalid("goal must contain 1..16384 bytes"));
        }
        if ![
            &self.harness_profile,
            &self.environment_profile,
            &self.policy_profile,
        ]
        .iter()
        .all(|s| name(s))
        {
            return Err(Invalid("invalid registered profile ID"));
        }
        match &self.workspace {
            WorkspaceSource::Repository {
                repository_id,
                commit,
            } => {
                if !name(repository_id)
                    || ![40, 64].contains(&commit.len())
                    || !commit
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                {
                    return Err(Invalid(
                        "workspace requires a registered repository and full lowercase commit hash",
                    ));
                }
            }
            WorkspaceSource::Checkpoint { checkpoint_id } => non_nil(checkpoint_id.0)?,
        }
        if self.components.len() > 32
            || self.required_capabilities.len() > 32
            || !self.required_capabilities.iter().all(|s| name(s))
        {
            return Err(Invalid(
                "invalid or excessive component/capability requirements",
            ));
        }
        let mut seen = BTreeSet::new();
        for c in &self.components {
            if !name(&c.component_id) || !seen.insert(&c.component_id) {
                return Err(Invalid("invalid or duplicate component binding"));
            }
        }
        if self.limits.wall_seconds == 0
            || self.limits.cpu_millis == 0
            || self.limits.memory_mib == 0
        {
            return Err(Invalid("execution limits must be positive"));
        }
        Ok(())
    }
}
impl Command {
    /// Syntactic checks only. Server performs authorization, graph and budget checks atomically.
    pub fn validate(&self) -> Result<(), Invalid> {
        non_nil(self.operation_id.0)?;
        match &self.action {
            Action::CreateTask { task_id, spec } => {
                non_nil(task_id.0)?;
                spec.validate()?;
            }
            Action::CancelTask { task_id, .. } => non_nil(task_id.0)?,
            Action::ApplyGraph { root_id, edits, .. } => {
                non_nil(root_id.0)?;
                if edits.is_empty() || edits.len() > MAX_GRAPH_EDITS {
                    return Err(Invalid("graph proposal requires 1..64 edits"));
                }
                let mut spawned = BTreeSet::new();
                let mut edges = BTreeSet::new();
                for edit in edits {
                    match edit {
                        GraphEdit::Spawn {
                            task_id,
                            parent_id,
                            spec,
                        } => {
                            non_nil(task_id.0)?;
                            non_nil(parent_id.0)?;
                            if task_id == parent_id
                                || task_id == root_id
                                || !spawned.insert(task_id)
                            {
                                return Err(Invalid("invalid or duplicate child identity"));
                            }
                            spec.validate()?;
                        }
                        GraphEdit::AddDependency {
                            task_id,
                            depends_on,
                        }
                        | GraphEdit::RemoveDependency {
                            task_id,
                            depends_on,
                        } => {
                            non_nil(task_id.0)?;
                            non_nil(depends_on.0)?;
                            if task_id == depends_on || !edges.insert((task_id, depends_on)) {
                                return Err(Invalid("self dependency or duplicate edge edit"));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
    /// Normalized UTF-8 JSON: sorted object keys, compact encoding, integer numbers.
    /// Servers bind an operation ID to these exact bytes; whitespace in input files is irrelevant.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, Invalid> {
        self.validate()?;
        let mut value =
            serde_json::to_value(self).map_err(|_| Invalid("cannot serialize command"))?;
        value.sort_all_objects();
        let bytes = serde_json::to_vec(&value).map_err(|_| Invalid("cannot serialize command"))?;
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(Invalid("command exceeds 256 KiB"));
        }
        Ok(bytes)
    }
    pub fn fingerprint(&self) -> Result<String, Invalid> {
        Ok(sha256(&self.canonical_bytes()?))
    }
}
pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema: Version,
    pub operation_id: OperationId,
    pub request_sha256: String,
    pub disposition: Disposition,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Accepted,
    Replay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub schema: Version,
    pub operation_id: OperationId,
    pub request_sha256: String,
    pub state: OperationState,
    pub task_ids: Vec<TaskId>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub schema: Version,
    pub task_id: TaskId,
    pub root_id: TaskId,
    pub parent_id: Option<TaskId>,
    pub revision: u64,
    pub graph_revision: u64,
    pub state: TaskState,
    pub effective_capabilities: BTreeSet<String>,
    pub artifacts: Vec<ArtifactId>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    Blocked,
    Completed,
    Failed,
    CancelRequested,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerInfo {
    pub schema: Version,
    pub execution_ready: bool,
    pub operations: BTreeSet<String>,
    pub harness_profiles: Vec<HarnessProfile>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HarnessProfile {
    pub id: String,
    pub driver: String,
    pub qualified: bool,
    pub capabilities: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventPage {
    pub schema: Version,
    pub task_id: TaskId,
    pub events: Vec<Event>,
    /// Exclusive per-task sequence cursor; never advance past undelivered events.
    pub next_after: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub sequence: u64,
    pub kind: String,
    pub summary: String,
    pub artifacts: Vec<ArtifactId>,
}

/// Generated schemas describe shape. `Command::validate` adds documented semantic bounds.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "schema": "branchyard/contract/v1alpha1",
        "command": schemars::schema_for!(Command),
        "receipt": schemars::schema_for!(Receipt),
        "operation": schemars::schema_for!(Operation),
        "task": schemars::schema_for!(Task),
        "server_info": schemars::schema_for!(ServerInfo),
        "event_page": schemars::schema_for!(EventPage),
        "limits": {"request_bytes": MAX_REQUEST_BYTES, "response_bytes": MAX_RESPONSE_BYTES,
            "graph_edits": MAX_GRAPH_EDITS, "event_page": MAX_EVENT_PAGE},
    })
}
