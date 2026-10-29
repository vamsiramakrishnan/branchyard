//! Workspace membership checks shared by console and legacy resource routes.

use std::collections::BTreeSet;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::StreamExt as _;
use serde_json::Value;

use crate::api::{App, Caller, Shared};
use crate::console_store::{Collection, Record};
use crate::error::ApiError;

fn denied() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "workspace_access_denied",
        "workspace membership does not permit this action",
    )
}

/// Resolve a workspace only after intersecting membership, tenant and scope policy.
/// Operator admin scope deliberately does not bypass workspace membership.
pub(crate) fn authorize_workspace(
    app: &App,
    caller: &Caller,
    id: &str,
    scope: &str,
) -> Result<Record, ApiError> {
    let required_scope = if scope == "comment" { "run" } else { scope };
    if !caller.0.allows(required_scope) {
        return Err(crate::api::scope_required(required_scope));
    }
    let workspace = app
        .registry
        .console_get(Collection::Workspaces, id)?
        .ok_or_else(denied)?;
    if workspace.tenant != caller.tenant() {
        return Err(denied());
    }
    let repo = workspace
        .body
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or_else(denied)?;
    if repo != "__console_managed__" {
        app.authorized_repo(caller, repo, required_scope)?;
    } else if !caller.0.repo_allowed(&app.tenant_policy(caller), repo) {
        return Err(denied());
    }
    let key = format!("{id}:{}", caller.0.connector_subject());
    let member = app
        .registry
        .console_get(Collection::Members, &key)?
        .ok_or_else(denied)?;
    if member.tenant != caller.tenant()
        || member.body.get("workspace_id").and_then(|v| v.as_str()) != Some(id)
        || member.body.get("subject").and_then(|v| v.as_str())
            != Some(caller.0.connector_subject().as_str())
    {
        return Err(denied());
    }
    let role = member
        .body
        .get("role")
        .and_then(|v| v.as_str())
        .ok_or_else(denied)?;
    if !role_permits(role, scope) {
        return Err(denied());
    }
    Ok(workspace)
}

fn role_permits(role: &str, scope: &str) -> bool {
    match scope {
        "read" => matches!(role, "owner" | "editor" | "reviewer" | "viewer"),
        "comment" => matches!(role, "owner" | "editor" | "reviewer"),
        "admin" => role == "owner",
        _ => matches!(role, "owner" | "editor"),
    }
}

fn binding(app: &App, caller: &Caller, key: &str, scope: &str) -> Result<bool, ApiError> {
    let Some(record) = app.registry.console_get(Collection::Bindings, key)? else {
        return Ok(false);
    };
    if record.tenant != caller.tenant() {
        return Err(denied());
    }
    let id = record
        .body
        .get("workspace_id")
        .and_then(|v| v.as_str())
        .ok_or_else(denied)?;
    let workspace = authorize_workspace(app, caller, id, scope)?;
    if record.collection != Collection::Bindings || record.id != key {
        return Err(denied());
    }
    let repo = record
        .body
        .get("repo")
        .and_then(Value::as_str)
        .ok_or_else(denied)?;
    if workspace.body.get("repo").and_then(Value::as_str) != Some(repo) {
        return Err(denied());
    }
    if let Some(operation_id) = key.strip_prefix("operation:") {
        if record.body.get("operation_id").and_then(Value::as_str) != Some(operation_id) {
            return Err(denied());
        }
    } else {
        let branch = record
            .body
            .get("branch")
            .and_then(Value::as_str)
            .ok_or_else(denied)?;
        if key != format!("{repo}:{branch}") {
            return Err(denied());
        }
    }
    Ok(true)
}

/// Check a legacy branch and its complete ancestry for workspace ownership.
pub(crate) fn authorize_branch(
    app: &App,
    caller: &Caller,
    repo: &str,
    branch: &str,
    scope: &str,
) -> Result<(), ApiError> {
    if repo == "__console_managed__" {
        // Managed yards are resolved exclusively through an explicit workspace capability.
        return Err(denied());
    }
    let state = app.authorized_repo(caller, repo, scope)?;
    let mut current = Some(branch.to_owned());
    let mut seen = BTreeSet::new();
    while let Some(name) = current {
        if !seen.insert(name.clone()) {
            return Err(denied());
        }
        if binding(app, caller, &format!("{repo}:{name}"), scope)? {
            return Ok(());
        }
        for session in app.registry.console_list_all(Collection::Sessions)? {
            if session.body.get("branch").and_then(Value::as_str) != Some(name.as_str()) {
                continue;
            }
            let workspace_id = session
                .body
                .get("workspace_id")
                .and_then(Value::as_str)
                .ok_or_else(denied)?;
            let workspace = app
                .registry
                .console_get(Collection::Workspaces, workspace_id)?
                .ok_or_else(denied)?;
            if workspace.body.get("repo").and_then(Value::as_str) == Some(repo) {
                // Session metadata without its atomic resource binding is damaged.
                return Err(denied());
            }
        }
        for task in
            branchyard::tasks::list(&state.yard).map_err(|error| crate::error::sdk(&error))?
        {
            if task.attempts.iter().any(|attempt| attempt.name == name) {
                if binding(app, caller, &format!("{repo}:task:{}", task.task.id), scope)? {
                    return Ok(());
                }
                if app
                    .registry
                    .console_list_all(Collection::Workspaces)?
                    .iter()
                    .any(|workspace| {
                        workspace.body.get("repo").and_then(Value::as_str) == Some(repo)
                            && workspace.body.get("task_id").and_then(Value::as_str)
                                == Some(task.task.id.as_str())
                    })
                {
                    return Err(denied());
                }
            }
        }
        if name.starts_with("s-")
            && name.len() == 28
            && name[2..].bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(denied());
        }
        current = match state.yard.branch(&name) {
            Ok(branch) => branch.info().parent.clone(),
            Err(error) => return Err(crate::error::sdk(&error)),
        };
    }
    Ok(())
}

/// Check operation branches and explicit workspace allocation bindings.
pub(crate) fn authorize_operation(
    app: &App,
    caller: &Caller,
    operation: &branchyard_client::api::Operation,
    scope: &str,
) -> Result<(), ApiError> {
    if binding(app, caller, &format!("operation:{}", operation.id), scope)? {
        return Ok(());
    }
    if operation.repo == "__console_managed__" {
        // Every managed operation reserves its explicit ownership in admission.
        // A missing row is damage, never a reason to infer a fallback grant.
        return Err(denied());
    }
    app.authorized_repo(caller, &operation.repo, scope)?;
    for branch in &operation.branches {
        if binding(app, caller, &format!("{}:{branch}", operation.repo), scope)? {
            continue;
        }
        // A task-creation operation may name an attempt before its yard exists.
        if app
            .repos
            .get(&operation.repo)
            .is_some_and(|r| r.yard.branch(branch).is_ok())
        {
            authorize_branch(app, caller, &operation.repo, branch, scope)?;
        }
    }
    Ok(())
}

/// Remove control-server filesystem locations from managed operation responses.
pub(crate) fn public_operation(
    mut operation: branchyard_client::api::Operation,
) -> branchyard_client::api::Operation {
    if operation.repo == "__console_managed__" {
        if let Some(result) = &mut operation.result {
            for branch in result
                .branches
                .iter_mut()
                .chain(result.descendants.iter_mut())
            {
                branch.worktree.clear();
                if let Some(branchyard::Guard::Landlock { writable, .. }) = &mut branch.guard {
                    writable.clear();
                }
            }
        }
        if let Some(error) = &mut operation.error {
            error.message =
                "managed workspace operation failed; inspect the server diagnostics".into();
            error.detail = None;
        }
    }
    operation
}

/// Guard old URLs before their handlers, including aggregate feeds and records.
pub(crate) fn authorize_legacy(
    app: &App,
    caller: &Caller,
    method: &Method,
    path: &str,
) -> Result<(), ApiError> {
    let parts: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(branchyard_client::http::decode)
        .collect();
    let scope = if matches!(*method, Method::GET | Method::HEAD) {
        "read"
    } else if *method == Method::DELETE {
        "admin"
    } else if parts.get(3).is_some_and(|part| part == "branches")
        && parts
            .get(5)
            .is_some_and(|part| matches!(part.as_str(), "merge" | "integrate"))
    {
        "merge"
    } else {
        "run"
    };
    if parts.len() >= 3
        && parts[0] == "v1"
        && parts[1] == "workspaces"
        && matches!(*method, Method::GET | Method::HEAD)
    {
        authorize_workspace(app, caller, &parts[2], "read")?;
    }
    if parts.len() >= 3 && parts[0] == "v1" && parts[1] == "operations" {
        if let Some(operation) = app.registry.get_for_tenant(&parts[2], caller.tenant())? {
            authorize_operation(app, caller, &operation, scope)?;
        }
    }
    if parts.len() < 4 || parts[0] != "v1" || parts[1] != "repos" {
        return Ok(());
    }
    let repo = &parts[2];
    if repo == "__console_managed__" {
        return Err(denied());
    }
    if parts[3] == "branches" && parts.len() >= 5 {
        let reader = &parts[4];
        authorize_branch(app, caller, repo, reader, scope)?;
        if parts.get(5).map(String::as_str) == Some("artifacts")
            && (parts.len() > 6 || matches!(*method, Method::GET | Method::HEAD))
        {
            let state = app.authorized_repo(caller, repo, "read")?;
            for artifact in state
                .yard
                .artifacts(reader)
                .map_err(|error| crate::error::sdk(&error))?
            {
                if parts.get(6).is_none_or(|id| id == &artifact.id) {
                    authorize_branch(app, caller, repo, &artifact.publisher_branch, "read")?;
                }
            }
        }
        return Ok(());
    }
    // Aggregate old URLs expose multiple resources. Until they can filter each native
    // record, require access to every workspace sharing the repository.
    for workspace in app.registry.console_list_all(Collection::Workspaces)? {
        if workspace.body.get("repo").and_then(|v| v.as_str()) == Some(repo) {
            authorize_workspace(app, caller, &workspace.id, scope)?;
        }
    }
    Ok(())
}

/// Redact operator-owned storage roots from bounded workspace JSON errors.
/// Success bodies and other routes retain their existing representation.
pub(crate) async fn redact_workspace_error(app: &App, path: &str, response: Response) -> Response {
    let root = app
        .config
        .managed_knowledge
        .root
        .clone()
        .unwrap_or_else(|| app.config.data_dir.join("knowledge"));
    redact_error(path, &root.to_string_lossy(), response).await
}

async fn redact_error(path: &str, root: &str, response: Response) -> Response {
    let is_json = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !(path == "/v1/workspaces" || path.starts_with("/v1/workspaces/"))
        || !response.status().is_client_error() && !response.status().is_server_error()
        || !is_json
        || root.is_empty()
    {
        return response;
    }
    const FALLBACK:&[u8]=br#"{"error":{"code":"workspace_error","message":"workspace request failed; inspect the server diagnostics"}}"#;
    let (mut parts, body) = response.into_parts();
    let encoded = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(mut value) => {
                let code = value.pointer("/error/code").cloned();
                redact_paths(&mut value, root);
                if let Some(code) = code {
                    value["error"]["code"] = code;
                }
                match serde_json::to_vec(&value) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(%error,"could not encode a redacted workspace error");
                        FALLBACK.to_vec()
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error,"could not parse a workspace error for redaction");
                FALLBACK.to_vec()
            }
        },
        Err(error) => {
            tracing::warn!(%error,"could not read a bounded workspace error for redaction");
            FALLBACK.to_vec()
        }
    };
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    if let Ok(length) = axum::http::HeaderValue::from_str(&encoded.len().to_string()) {
        parts
            .headers
            .insert(axum::http::header::CONTENT_LENGTH, length);
    }
    Response::from_parts(parts, axum::body::Body::from(encoded))
}

pub(super) fn redact_paths(value: &mut Value, root: &str) {
    match value {
        Value::String(text) => *text = text.replace(root, "<managed-storage>"),
        Value::Array(values) => {
            for value in values {
                redact_paths(value, root);
            }
        }
        Value::Object(values) => {
            let original = std::mem::take(values);
            for (key, mut value) in original {
                redact_paths(&mut value, root);
                values.insert(key.replace(root, "<managed-storage>"), value);
            }
        }
        _ => {}
    }
}

/// End an open legacy stream when any previously required membership is revoked.
pub(crate) fn bound_stream(
    app: Shared,
    caller: Caller,
    path: String,
    response: Response,
) -> Response {
    let streaming = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    if !streaming {
        return response;
    }
    let ended = async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            if authorize_legacy(&app, &caller, &Method::GET, &path).is_err() {
                return;
            }
        }
    };
    let (parts, body) = response.into_parts();
    let body = axum::body::Body::from_stream(body.into_data_stream().take_until(Box::pin(ended)));
    Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_operation_responses_hide_host_paths() {
        let operation:branchyard_client::api::Operation=serde_json::from_value(serde_json::json!({
            "id":"operation","repo":"__console_managed__","kind":"task","state":"failed","branches":["session"],"cursor":0,"created_at_ms":0,
            "result":{"branches":[{"name":"session","git_branch":"by/session","worktree":"/operator/private/work","prompt":"question","harness":"fake","profile":"default","parent":null,"base":"base","candidate":null,"status":{"state":"ready"},"turns":1,"cost_usd":null,"created_at":0}]},
            "error":{"code":"initialization_failed","message":"cannot create /operator/private/work","detail":{"path":"/operator/private/work"}}
        })).unwrap();
        let rendered = serde_json::to_string(&public_operation(operation)).unwrap();
        assert!(!rendered.contains("/operator/private"));
        assert!(rendered.contains("initialization_failed"));
    }

    #[tokio::test]
    async fn workspace_error_redaction_preserves_json_status_code_and_headers() {
        use axum::response::IntoResponse as _;
        let root = "/operator/private \"yard\"/line\nroot";
        let detail = serde_json::json!({"error":{"code":"not_a_repository","message":format!("cannot open {root}/tasks/task/work"),"detail":{"nested":[{"path":root}],(root):"path key"}}});
        let mut response = (StatusCode::CONFLICT, axum::Json(detail)).into_response();
        response
            .headers_mut()
            .insert("x-request-id", axum::http::HeaderValue::from_static("kept"));
        let response = redact_error("/v1/workspaces/workspace", root, response).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(response.headers()["x-request-id"], "kept");
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let error: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error["error"]["code"], "not_a_repository");
        assert!(!String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("operator/private"));
        assert_eq!(
            error["error"]["detail"]["nested"][0]["path"],
            "<managed-storage>"
        );
        let success =
            (StatusCode::OK, axum::Json(serde_json::json!({"text":root}))).into_response();
        let success = redact_error("/v1/workspaces/workspace", root, success).await;
        let bytes = axum::body::to_bytes(success.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["text"],
            root
        );
    }

    #[test]
    fn unknown_roles_fail_closed_and_reviewers_cannot_run() {
        assert!(!role_permits("", "read"));
        assert!(!role_permits("reviewer", "run"));
        assert!(role_permits("reviewer", "read"));
        assert!(!role_permits("viewer", "comment"));
        assert!(role_permits("owner", "admin"));
        assert!(!role_permits("editor", "admin"));
    }
}
