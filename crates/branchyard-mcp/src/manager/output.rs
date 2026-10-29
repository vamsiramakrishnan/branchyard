//! Bound model-facing responses without truncating JSON or changing the HTTP API.

use std::borrow::Cow;

use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData;
use serde_json::{json, Map, Value};

/// Each textual and structured copy stays within this serialized JSON budget.
const JSON_BUDGET: usize = 32 * 1024;

/// Encode valid JSON with the protocol's ordinary internal-error mapping.
fn encode(value: &Value) -> Result<String, ErrorData> {
    serde_json::to_string(value)
        .map_err(|error| ErrorData::internal_error(Cow::Owned(error.to_string()), None))
}

/// Retain small scalar identifiers and continuation state, never response bodies.
fn metadata(value: &Value) -> Result<Value, ErrorData> {
    let mut metadata = Map::new();
    let mut omitted = false;
    for (container, names) in [
        (
            None,
            &[
                "id",
                "workspace_id",
                "project_id",
                "session_id",
                "artifact_id",
                "operation_id",
                "cursor",
                "next_cursor",
                "revision",
                "offset",
                "limit",
                "total_rows",
                "has_more",
                "status",
            ][..],
        ),
        (
            Some("operation"),
            &["id", "workspace_id", "state", "status"][..],
        ),
        (Some("artifact"), &["id"][..]),
        (Some("error"), &["code"][..]),
    ] {
        let source = container.map_or(value, |key| &value[key]);
        let mut fields = Map::new();
        for name in names {
            if let Some(field) = source.get(*name) {
                if !field.is_array() && !field.is_object() && encode(field)?.len() <= 1024 {
                    fields.insert((*name).into(), field.clone());
                } else {
                    omitted = true;
                }
            }
        }
        match container {
            Some(key) if !fields.is_empty() => {
                metadata.insert(key.into(), fields.into());
            }
            None => metadata.extend(fields),
            Some(_) => {}
        }
    }
    if omitted {
        metadata.insert("oversized_metadata_omitted".into(), true.into());
    }
    Ok(metadata.into())
}

/// Return a bounded, explicit error for oversized success or failure responses.
/// A successful mutation can already have happened; its operation ID survives.
pub(super) fn bounded_result(value: Value, failed: bool) -> Result<CallToolResult, ErrorData> {
    let text = encode(&value)?;
    let (value, text, failed) = if text.len() > JSON_BUDGET {
        let bounded = json!({
            "code": "manager_output_too_large",
            "message": "The server response exceeds the manager's context budget.",
            "max_json_bytes": JSON_BUDGET,
            "response_json_bytes": text.len(),
            "server_success_response": !failed,
            "metadata": metadata(&value)?,
            "next": [
                "Use preview_artifact with a lower row limit, page or offset.",
                "Inspect artifact metadata and download the original through the authorized content endpoint.",
                "For a large table or conversation, start a session to compute a focused result.",
                "A mutation may already have succeeded. Inspect its operation and reuse its original idempotency_key; do not create a new request."
            ]
        });
        let text = encode(&bounded)?;
        (bounded, text, true)
    } else {
        (value, text, failed)
    };
    let content = vec![ContentBlock::text(text)];
    let mut result = if failed {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    result.structured_content = Some(value);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_tables_and_strings_return_valid_bounded_errors_with_continuation_state() {
        for body in [
            json!("x".repeat(JSON_BUDGET)),
            json!(vec![vec!["cell"; 100]; 100]),
        ] {
            let result = bounded_result(
                json!({
                    "rows": body, "id":"artifact-a", "next_cursor":"page-two",
                    "operation":{"id":"operation-a","state":"done"},
                }),
                false,
            )
            .unwrap();
            assert_eq!(result.is_error, Some(true));
            let protocol = serde_json::to_value(&result).unwrap();
            let text = protocol["content"][0]["text"].as_str().unwrap();
            assert!(text.len() <= JSON_BUDGET);
            assert_eq!(
                serde_json::from_str::<Value>(text).unwrap(),
                result.structured_content.clone().unwrap()
            );
            let structured = result.structured_content.unwrap();
            assert_eq!(structured["code"], "manager_output_too_large");
            assert_eq!(structured["metadata"]["next_cursor"], "page-two");
            assert_eq!(structured["metadata"]["operation"]["id"], "operation-a");
            assert_eq!(structured["server_success_response"], true);
            assert!(encode(&structured).unwrap().len() <= JSON_BUDGET);
            assert!(!encode(&structured).unwrap().contains("cell"));
        }
    }

    #[test]
    fn oversized_api_errors_and_metadata_cannot_escape_the_context_budget() {
        let result = bounded_result(
            json!({
                "status":403, "error":{"code":"denied","message":"x".repeat(JSON_BUDGET)},
                "next_cursor":"z".repeat(JSON_BUDGET),
            }),
            true,
        )
        .unwrap();
        let value = result.structured_content.unwrap();
        assert_eq!(value["code"], "manager_output_too_large");
        assert_eq!(value["server_success_response"], false);
        assert_eq!(value["metadata"]["error"]["code"], "denied");
        assert_eq!(value["metadata"]["oversized_metadata_omitted"], true);
        assert!(encode(&value).unwrap().len() <= JSON_BUDGET);
        let small = json!({"id":"op","next_cursor":"next"});
        let result = bounded_result(small.clone(), false).unwrap();
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content, Some(small));
    }
}
