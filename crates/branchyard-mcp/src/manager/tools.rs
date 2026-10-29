use std::sync::Arc;

use rmcp::model::{Tool, ToolAnnotations};
use serde_json::{json, Map, Value};

pub(super) const NAMES: &[&str] = &[
    "list_harnesses",
    "worker_inventory",
    "list_projects",
    "create_project",
    "list_workspaces",
    "create_workspace",
    "inspect_workspace",
    "list_sessions",
    "start_session",
    "session_messages",
    "send_message",
    "steer_session",
    "resume_session",
    "cancel_session",
    "list_sources",
    "list_artifacts",
    "inspect_artifact",
    "preview_artifact",
    "finalize_deliverables",
    "workspace_action",
    "operation_status",
    "wait_operation",
];

fn text(max: u64) -> Value {
    json!({"type":"string","minLength":1,"maxLength":max})
}

fn choices(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}

fn strings() -> Value {
    json!({"type":"array","maxItems":256,"items":text(256)})
}

fn definition(name: &str) -> (&'static str, Value, Vec<&'static str>, bool) {
    let id = || text(256);
    match name {
        "list_harnesses" => ("Read configured native agent profiles and reported readiness before selecting a session harness.", json!({}), vec![], true),
        "worker_inventory" => ("Read workers and advertised capabilities available to this principal; catalog presence is not live qualification.", json!({}), vec![], true),
        "list_projects" => ("List projects this authenticated principal can access.", json!({}), vec![], true),
        "create_project" => ("Create a knowledge or registered-repository project. Reuse idempotency_key when retrying.",
            json!({"name":text(500),"source":{"oneOf":[
                {"type":"object","properties":{"kind":{"const":"knowledge"}},"required":["kind"],"additionalProperties":false},
                {"type":"object","properties":{"kind":{"const":"repository"},"repo":id()},"required":["kind","repo"],"additionalProperties":false}
            ]},"result_mode":choices(&["code","artifacts","mixed"]),"idempotency_key":id()}),
            vec!["name","source","result_mode","idempotency_key"],false),
        "list_workspaces" => ("List accessible workspaces, optionally for one project.",json!({"project_id":id()}),vec![],true),
        "create_workspace" => ("Reserve a task-backed workspace. Knowledge work does not require a user repository.",
            json!({"project_id":id(),"title":text(500),"idempotency_key":id()}),vec!["project_id","title","idempotency_key"],false),
        "inspect_workspace" => ("Read workspace state, conversations, source references and accepted deliverables.",json!({"workspace_id":id()}),vec!["workspace_id"],true),
        "list_sessions" => ("List native conversations attached to the workspace.",json!({"workspace_id":id()}),vec!["workspace_id"],true),
        "start_session" => ("Start an isolated writer or constrained reviewer through the ordinary durable queue. Native plans and subagents remain native.",
            json!({"workspace_id":id(),"name":text(500),"prompt":text(131072),"harness":id(),"model":text(500),"effort":choices(&["low","medium","high","xhigh","max","ultra"]),
                "access":choices(&["writer","reviewer"]),"result_contract":{"type":"object"},"sources":strings(),"artifacts":strings(),"idempotency_key":id()}),
            vec!["workspace_id","name","prompt","harness","access","idempotency_key"],false),
        "session_messages" => ("Read a native conversation without copying or replacing its session state. Pass next_cursor as after.",
            json!({"workspace_id":id(),"session_id":id(),"after":text(1024)}),vec!["workspace_id","session_id"],true),
        "send_message" | "steer_session" | "resume_session" => ("Continue or steer the existing native session; this does not create a replacement lead.",
            json!({"workspace_id":id(),"session_id":id(),"prompt":text(131072),"idempotency_key":id()}),vec!["workspace_id","session_id","prompt","idempotency_key"],false),
        "cancel_session" => ("Request cancellation of a native turn. Existing side effects are not undone.",
            json!({"workspace_id":id(),"session_id":id(),"idempotency_key":id()}),vec!["workspace_id","session_id","idempotency_key"],false),
        "list_sources" => ("List immutable input source metadata; no host paths or file reads are accepted.",json!({"workspace_id":id()}),vec!["workspace_id"],true),
        "list_artifacts" => ("List permitted immutable output artifacts, digests and provenance.",json!({"workspace_id":id()}),vec!["workspace_id"],true),
        "inspect_artifact" => ("Read one artifact's metadata. Use the normal authenticated content endpoint to download its original bytes.",
            json!({"workspace_id":id(),"artifact_id":id()}),vec!["workspace_id","artifact_id"],true),
        "preview_artifact" => ("Read a bounded CSV/workbook table or document/slide preview, defaulting to 25 rows. Use a narrower preview or focused compute when the context budget is exceeded. Conversion is not visual approval. Missing tools are explicit.",
            json!({"workspace_id":id(),"artifact_id":id(),"view":choices(&["auto","table","pages"]),"page":{"type":"integer","minimum":1,"maximum":100000},
                "sheet":text(500),"offset":{"type":"integer","minimum":0,"maximum":4294967295_u64},"limit":{"type":"integer","minimum":1,"maximum":1000}}),
            vec!["workspace_id","artifact_id"],true),
        "finalize_deliverables" => ("Accept selected immutable outputs at the current workspace revision. Knowledge completion does not require a Git diff.",
            json!({"workspace_id":id(),"revision":{"type":"integer","minimum":0},"artifacts":strings(),"idempotency_key":id()}),vec!["workspace_id","revision","artifacts","idempotency_key"],false),
        "workspace_action" => ("Request a supported sleep, wake, archive or restore action. Unsupported capabilities return a reason, never a weaker operation.",
            json!({"workspace_id":id(),"action":choices(&["sleep","wake","archive","restore"]),"idempotency_key":id()}),vec!["workspace_id","action","idempotency_key"],false),
        "operation_status" => ("Read one operation using its normal server authorization.",json!({"operation_id":id()}),vec!["operation_id"],true),
        "wait_operation" => ("Long-poll an operation for up to 30 seconds. A nonterminal result means it is still running; observing does not cancel it.",
            json!({"operation_id":id(),"seconds":{"type":"integer","minimum":0,"maximum":30}}),vec!["operation_id"],true),
        _ => ("Unknown manager tool.",json!({}),vec![],true),
    }
}

pub(super) fn tools() -> Vec<Tool> {
    NAMES
        .iter()
        .map(|name| {
            let (description, properties, required, read_only) = definition(name);
            let schema = Map::from_iter([
                ("type".into(), json!("object")),
                ("properties".into(), properties),
                ("required".into(), json!(required)),
                ("additionalProperties".into(), json!(false)),
            ]);
            let mut tool = Tool::new(*name, description, Arc::new(schema));
            let mut annotations = ToolAnnotations::default();
            annotations.read_only_hint = Some(read_only);
            annotations.destructive_hint = Some(*name == "cancel_session");
            annotations.idempotent_hint = Some(true);
            annotations.open_world_hint = Some(!read_only);
            tool.annotations = Some(annotations);
            tool
        })
        .collect()
}

pub(super) fn validate(name: &str, arguments: &Value) -> Result<(), String> {
    if !NAMES.contains(&name) {
        return Err(format!("no manager tool named {name}"));
    }
    let (_, properties, required, _) = definition(name);
    let schema = branchyard::JsonSchema::new(
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
    )?;
    let errors = schema.validate(arguments);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_are_closed_and_catalog_is_separate_from_branch_tools() {
        let listed = tools();
        assert_eq!(listed.len(), NAMES.len());
        for tool in listed {
            assert_eq!(
                tool.input_schema.get("additionalProperties"),
                Some(&json!(false))
            );
        }
        assert!(!NAMES.contains(&"delegate"));
        assert!(!NAMES.contains(&"propose_integration"));
    }

    #[test]
    fn arguments_reject_unknown_fields_wrong_shapes_and_unbounded_values() {
        assert!(validate("create_project",&json!({"name":"Research","source":{"kind":"knowledge"},"result_mode":"artifacts","idempotency_key":"k"})).is_ok());
        for value in [
            json!({"workspace_id":"w","command":"rm"}),
            json!({"workspace_id":3}),
            json!({}),
        ] {
            assert!(validate("inspect_workspace", &value).is_err());
        }
        assert!(validate("create_project",&json!({"name":"R","source":{"kind":"knowledge","path":"/home"},"result_mode":"artifacts","idempotency_key":"k"})).is_err());
        assert!(validate(
            "preview_artifact",
            &json!({"workspace_id":"w","artifact_id":"a","limit":1001})
        )
        .is_err());
        assert!(validate("wait_operation", &json!({"operation_id":"x","seconds":-1})).is_err());
        assert!(validate(
            "workspace_action",
            &json!({"workspace_id":"w","action":"delete","idempotency_key":"k"})
        )
        .is_err());
        assert!(validate("unknown", &json!({})).is_err());
    }
}
