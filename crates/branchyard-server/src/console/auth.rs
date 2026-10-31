//! Workspace membership checks shared by console and legacy resource routes.

use std::collections::BTreeSet;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::StreamExt as _;
use serde_json::Value;

use super::Projection;
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
    let workspace = workspace_access(app, caller, id, scope)?;
    let repo = workspace.body["repo"].as_str().ok_or_else(denied)?;
    if repo != "__console_managed__" {
        app.authorized_repo(caller, repo, if scope == "comment" { "run" } else { scope })?;
    }
    Ok(workspace)
}

/// Check durable membership and repository grants without resolving native files.
fn workspace_access(app: &App, caller: &Caller, id: &str, scope: &str) -> Result<Record, ApiError> {
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
    if !caller.0.repo_allowed(&app.tenant_policy(caller), repo) {
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
    let workspace = workspace_access(app, caller, id, scope)?;
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
    if !caller.0.allows(scope) {
        return Err(crate::api::scope_required(scope));
    }
    if !caller
        .0
        .repo_allowed(&app.tenant_policy(caller), &operation.repo)
    {
        return Err(denied());
    }

    if binding(app, caller, &format!("operation:{}", operation.id), scope)? {
        return Ok(());
    }
    if operation.repo == "__console_managed__" {
        // Every managed operation reserves its explicit ownership in admission.
        // A missing row is damage, never a reason to infer a fallback grant.
        return Err(denied());
    }
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
            if !caller
                .0
                .repo_allowed(&app.tenant_policy(caller), &operation.repo)
            {
                return Err(crate::api::unknown_operation(parts[2].clone()));
            }
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

/// The largest error body read for redaction.
const ERROR_BOUND: usize = 1024 * 1024;

/// The largest gateway read projected for a preset session: the PR
/// snapshot's own 2 MiB bound and a terminal page (1 MiB of output, as
/// base64) both fit.
const GATEWAY_READ_BOUND: usize = 4 * 1024 * 1024;

const FALLBACK: &[u8] = br#"{"error":{"code":"workspace_error","message":"workspace request failed; inspect the server diagnostics"}}"#;

/// Whose preset projection a response passes through.
enum Owner {
    Session { workspace: String, session: String },
    Branch { repo: String, branch: String },
}

/// What a preset session's projection rewrites in a JSON body.
enum Part {
    Whole,
    /// Only `error`: a terminal page's `data` is the terminal's own bytes.
    ErrorField,
}

/// The projection a JSON response with `status` to `method` on `path`
/// passes through, what it rewrites and how much of the body is read.
/// Errors on a session's console routes and on a legacy branch route do,
/// and so do the runtime and PR gateways' reads, which know nothing of
/// presets. Other success bodies are projected by their handlers.
fn preset_target(method: &Method, path: &str, status: StatusCode) -> Option<(Owner, Part, usize)> {
    let decoded: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(branchyard_client::http::decode)
        .collect();
    let parts: Vec<&str> = decoded.iter().map(String::as_str).collect();
    let error = status.is_client_error() || status.is_server_error();
    match parts.as_slice() {
        ["v1", "workspaces", workspace, "sessions", session, rest @ ..] => {
            let owner = Owner::Session {
                workspace: (*workspace).to_owned(),
                session: (*session).to_owned(),
            };
            match rest {
                _ if error => Some((owner, Part::Whole, ERROR_BOUND)),
                ["runtime"] | ["pr"] if *method == Method::GET => {
                    Some((owner, Part::Whole, GATEWAY_READ_BOUND))
                }
                ["terminals", _, "output"] if *method == Method::GET => {
                    Some((owner, Part::ErrorField, GATEWAY_READ_BOUND))
                }
                _ => None,
            }
        }
        ["v1", "repos", repo, "branches", branch, ..] if error => Some((
            Owner::Branch {
                repo: (*repo).to_owned(),
                branch: (*branch).to_owned(),
            },
            Part::Whole,
            ERROR_BOUND,
        )),
        _ => None,
    }
}

/// What a response shows of the operator's configuration: no managed
/// storage root in a workspace route's JSON error, and no configured value
/// of a preset session in what [`preset_target`] names. Status, headers
/// and `error.code` are kept. A projection that cannot be looked up
/// withholds the body.
pub(crate) async fn redact_response(
    app: &Shared,
    method: &Method,
    path: &str,
    response: Response,
) -> Response {
    let root = app
        .config
        .managed_knowledge
        .root
        .clone()
        .unwrap_or_else(|| app.config.data_dir.join("knowledge"));
    let target = match is_json(&response) {
        true => preset_target(method, path, response.status()),
        false => None,
    };
    let preset = match target {
        Some((owner, part, bound)) => {
            let app = app.clone();
            let found = crate::api::blocking(move || match owner {
                Owner::Session { workspace, session } => {
                    Projection::of_session(&app, &workspace, &session)
                }
                Owner::Branch { repo, branch } => Projection::of_branch(&app, &repo, &branch),
            })
            .await
            .and_then(|found| found);
            match found {
                Ok(projection) => Some((projection, part, bound)),
                Err(error) => {
                    tracing::warn!(error = %error.body.message, "could not look up a preset projection");
                    return withheld(response.into_parts().0);
                }
            }
        }
        None => None,
    };
    redact(path, &root.to_string_lossy(), preset, response).await
}

fn is_json(response: &Response) -> bool {
    response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
}

async fn redact(
    path: &str,
    root: &str,
    preset: Option<(Projection, Part, usize)>,
    response: Response,
) -> Response {
    let managed = (path == "/v1/workspaces" || path.starts_with("/v1/workspaces/"))
        && (response.status().is_client_error() || response.status().is_server_error())
        && is_json(&response)
        && !root.is_empty();
    let preset = preset.filter(|(projection, ..)| !projection.is_empty());
    if !managed && preset.is_none() {
        return response;
    }
    let bound = preset.as_ref().map_or(ERROR_BOUND, |(.., bound)| *bound);
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, bound).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "could not read a bounded response for redaction");
            return withheld(parts);
        }
    };
    let mut value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "could not parse a response for redaction");
            return withheld(parts);
        }
    };
    let code = value.pointer("/error/code").cloned();
    if managed {
        redact_paths(&mut value, root);
    }
    if let Some((projection, part, _)) = &preset {
        match part {
            Part::Whole => projection.apply(&mut value),
            Part::ErrorField => {
                if let Some(error) = value.get_mut("error") {
                    projection.apply(error);
                }
            }
        }
    }
    if let (Some(code), Some(slot)) = (code, value.pointer_mut("/error/code")) {
        *slot = code;
    }
    match serde_json::to_vec(&value) {
        Ok(encoded) => with_body(parts, encoded),
        Err(error) => {
            tracing::warn!(%error, "could not encode a redacted response");
            withheld(parts)
        }
    }
}

/// The fixed error in place of a body that could not be redacted; a
/// success becomes a server error, since its body is gone.
fn withheld(mut parts: axum::http::response::Parts) -> Response {
    if !parts.status.is_client_error() && !parts.status.is_server_error() {
        parts.status = StatusCode::INTERNAL_SERVER_ERROR;
    }
    with_body(parts, FALLBACK.to_vec())
}

fn with_body(mut parts: axum::http::response::Parts, encoded: Vec<u8>) -> Response {
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
        let response = redact("/v1/workspaces/workspace", root, None, response).await;
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
        let success = redact("/v1/workspaces/workspace", root, None, success).await;
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
