//! Durable presentation and access bindings for native tasks and attempts.
pub(crate) mod auth;
mod collaboration;
mod execution;
mod previews;
mod quotas;
mod routes;
mod runtime;
mod scheduling;
pub(crate) mod services;
pub(crate) mod sources;
#[cfg(test)]
mod tests;

use axum::http::StatusCode;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::api::{App, Caller};
use crate::console_store::{Collection, Mutation, Record};
use crate::error::ApiError;

pub(crate) use execution::execute;
pub(crate) use routes::router;
pub(crate) use runtime::operation_value;
pub use runtime::ConsoleWork;
/// Queue routing name for operator-enabled private task yards.
pub(crate) const MANAGED_REPO: &str = "__console_managed__";

fn io(error: std::io::Error) -> ApiError {
    match error.kind() {
        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::WouldBlock => {
            ApiError::new(StatusCode::CONFLICT, "revision_conflict", error.to_string())
        }
        _ => ApiError::internal(error.to_string()),
    }
}
fn sdk(error: branchyard::Error) -> ApiError {
    crate::error::sdk(&error)
}
fn id() -> Result<String, ApiError> {
    branchyard_support::new_ulid().map_err(|e| ApiError::internal(e.to_string()))
}
fn text<'a>(body: &'a Value, key: &str) -> Result<&'a str, ApiError> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && s.len() <= 65536)
        .ok_or_else(|| ApiError::bad_request(format!("{key} must be nonempty text")))
}
fn get(app: &App, collection: Collection, key: &str) -> Result<Record, ApiError> {
    app.registry.console_get(collection, key)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such console resource",
        )
    })
}
fn new(collection: Collection, key: &str, caller: &Caller, body: Value) -> Record {
    Record {
        collection,
        id: key.into(),
        tenant: caller.tenant().into(),
        revision: 1,
        body,
    }
}
fn insert(record: Record) -> Mutation {
    Mutation::Put {
        record,
        expected: None,
    }
}
fn update(mut record: Record, body: Value) -> Mutation {
    let expected = record.revision;
    record.revision = record.revision.saturating_add(1);
    record.body = body;
    Mutation::Put {
        record,
        expected: Some(expected),
    }
}
fn revision(record: &Record, body: &Value) -> Result<(), ApiError> {
    if body.get("revision").and_then(Value::as_u64) != Some(record.revision) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "revision_conflict",
            "reload the workspace and retry at its current revision",
        ));
    }
    Ok(())
}
fn public(record: &Record) -> Value {
    let mut body = record.body.clone();
    if let Some(object) = body.as_object_mut() {
        object.retain(|key, _| !key.starts_with('_'));
        object.insert("revision".into(), json!(record.revision));
    }
    body
}
fn scope(caller: &Caller, required: &str) -> Result<(), ApiError> {
    if caller.0.allows(required) {
        Ok(())
    } else {
        Err(crate::api::scope_required(required))
    }
}
fn related(
    app: &App,
    collection: Collection,
    caller: &Caller,
    workspace: &str,
) -> Result<Vec<Record>, ApiError> {
    Ok(app
        .registry
        .console_list(collection, caller.tenant())?
        .into_iter()
        .filter(|r| r.body["workspace_id"] == workspace)
        .collect())
}
fn binding(caller: &Caller, repo: &str, branch: &str, workspace: &str) -> Mutation {
    insert(new(
        Collection::Bindings,
        &format!("{repo}:{branch}"),
        caller,
        json!({"workspace_id":workspace,"repo":repo,"branch":branch}),
    ))
}
fn idem_key(
    caller: &Caller,
    headers: &axum::http::HeaderMap,
    route: &str,
) -> Result<String, ApiError> {
    let key = headers
        .get("idempotency-key")
        .and_then(|s| s.to_str().ok())
        .filter(|s| !s.is_empty() && s.len() <= 200)
        .ok_or_else(|| ApiError::bad_request("Idempotency-Key is required"))?;
    Ok(hex::encode(Sha256::digest(format!(
        "{}\0{}\0{route}\0{key}",
        caller.tenant(),
        caller.0.connector_subject()
    ))))
}
fn replay(app: &App, caller: &Caller, key: &str, body: &Value) -> Result<Option<Value>, ApiError> {
    match app.registry.console_get(Collection::Idempotency, key)? {
        Some(r) if r.tenant == caller.tenant() && r.body["request"] == *body => {
            Ok(Some(r.body["response"].clone()))
        }
        Some(_) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "the key already names a different request",
        )),
        None => Ok(None),
    }
}
fn receipt(caller: &Caller, key: &str, request: &Value, response: &Value) -> Mutation {
    insert(new(
        Collection::Idempotency,
        key,
        caller,
        json!({"request":request,"response":response}),
    ))
}

/// Resolve a task source after the caller's workspace capability was checked.
/// Provider services use this without changing configured repository grants.
pub(crate) fn resolve_source(
    app: &App,
    workspace: &Record,
) -> Result<crate::api::RepoState, ApiError> {
    runtime::resolve(app, workspace)
}
