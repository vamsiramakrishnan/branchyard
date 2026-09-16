use branchyard_protocol::*;
use branchyard_server::config::*;
use std::collections::BTreeMap;

pub const TOKEN: &str = "fixture-token-for-database-integration-only-0123456789";
pub fn setup() -> (Config, Principal, Command) {
    let mut command: Command = serde_json::from_str(include_str!(
        "../../../../examples/commands/create-task.json"
    ))
    .unwrap();
    command.operation_id = OperationId::new();
    let Action::CreateTask { task_id, spec } = &mut command.action else {
        unreachable!()
    };
    *task_id = TaskId::new();
    spec.limits.max_depth = 3;
    spec.limits.max_children = 4;
    let principal = Principal {
        tenant: uuid::Uuid::new_v4().simple().to_string(),
        subject: "controller".into(),
        expires_at_unix: 4_102_444_800,
        actions: ["read", "create_task", "apply_graph", "cancel_task"]
            .into_iter()
            .map(String::from)
            .collect(),
        subtree: None,
    };
    let profile = HarnessProfile {
        id: spec.harness_profile.clone(),
        driver: "fixture-only".into(),
        qualified: true,
        capabilities: spec.required_capabilities.clone(),
    };
    let policy = TenantPolicy {
        max_reserved_tasks: 32,
        max_reserved_cpu_millis: 64000,
        max_reserved_memory_mib: 131072,
        max_tasks_per_root: 16,
        max_root_cpu_millis: 32000,
        max_root_memory_mib: 65536,
        max_wall_seconds: 1200,
        max_depth: 4,
        max_children: 8,
        harnesses: BTreeMap::from([(profile.id.clone(), profile)]),
        environments: [spec.environment_profile.clone()].into(),
        policies: [spec.policy_profile.clone()].into(),
        repositories: ["branchyard".into()].into(),
    };
    let config = Config {
        tenants: BTreeMap::from([(principal.tenant.clone(), policy)]),
        credentials: vec![Credential {
            token_sha256: sha256(TOKEN.as_bytes()),
            principal: principal.clone(),
        }],
    };
    (config, principal, command)
}
pub fn root(command: &Command) -> (TaskId, TaskSpec) {
    let Action::CreateTask { task_id, spec } = &command.action else {
        unreachable!()
    };
    (*task_id, spec.clone())
}
pub fn spawn(command: &Command, expected: u64) -> (Command, TaskId) {
    let (root_id, mut spec) = root(command);
    spec.limits.max_depth -= 1;
    let child = TaskId::new();
    (
        Command {
            schema: Version::V1Alpha1,
            operation_id: OperationId::new(),
            action: Action::ApplyGraph {
                root_id,
                expected_revision: expected,
                edits: vec![GraphEdit::Spawn {
                    task_id: child,
                    parent_id: root_id,
                    spec: Box::new(spec),
                }],
            },
        },
        child,
    )
}
