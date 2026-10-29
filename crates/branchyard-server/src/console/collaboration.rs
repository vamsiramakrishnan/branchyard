//! Revision-checked membership, attributed comments and advisory presence.
use super::*;
use crate::api::{App, Caller, Shared};
use crate::console_store::Collection;
use crate::error::ApiError;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::get,
    Extension, Json, Router,
};
use serde_json::{json, Value};

pub(super) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/workspaces/{workspace}/members", get(members))
        .route(
            "/v1/workspaces/{workspace}/members/{subject}",
            get(member).post(set_member).delete(remove_member),
        )
        .route(
            "/v1/workspaces/{workspace}/comments",
            get(comments).post(comment),
        )
        .route(
            "/v1/workspaces/{workspace}/presence",
            get(presence).post(touch_presence),
        )
}
async fn members(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, ApiError> {
    auth::authorize_workspace(&app, &caller, &wid, "read")?;
    Ok(Json(
        json!({"members":related(&app,Collection::Members,&caller,&wid)?.iter().map(public).collect::<Vec<_>>()}),
    ))
}
async fn member(
    State(app): State<Shared>,
    Path((wid, subject)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, ApiError> {
    auth::authorize_workspace(&app, &caller, &wid, "read")?;
    let row = super::get(&app, Collection::Members, &format!("{wid}:{subject}"))?;
    if row.tenant != caller.tenant() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such member",
        ));
    }
    Ok(Json(public(&row)))
}
fn target_allowed(
    app: &App,
    caller: &Caller,
    subject: &str,
    workspace: &Record,
    role: &str,
) -> Result<(), ApiError> {
    let mut identities = app
        .config
        .principals
        .values()
        .chain(app.config.credentials.iter().map(|c| &c.principal))
        .cloned()
        .collect::<Vec<_>>();
    identities.extend(
        app.config
            .all_credentials()
            .into_iter()
            .map(|credential| credential.principal),
    );
    if let Some(google) = &app.config.google_identity {
        for binding in &google.bindings {
            let mut principal = binding.principal.clone();
            principal.origin = crate::config::AuthenticationOrigin::GoogleIdToken {
                issuer: binding.issuer.clone(),
                subject: binding.subject.clone(),
                audience: google.audience.clone(),
            };
            identities.push(principal);
        }
    }
    identities.push(caller.0.clone());
    let repo = text(&workspace.body, "repo")?;
    for principal in identities {
        if principal.connector_subject() == subject && principal.tenant == caller.tenant() {
            let policy = app.config.tenant_policy(caller.tenant());
            let required = if matches!(role, "owner" | "editor") {
                "run"
            } else {
                "read"
            };
            if principal.repo_allowed(&policy, repo) && principal.allows(required) {
                return Ok(());
            }
        }
    }
    Err(ApiError::new(StatusCode::FORBIDDEN,"member_policy_denied","membership requires a known verified subject with existing tenant, source and scope grants"))
}
fn last_owner(
    app: &App,
    caller: &Caller,
    wid: &str,
    existing: Option<&Record>,
    role: Option<&str>,
) -> Result<(), ApiError> {
    if existing.is_some_and(|r| r.body["role"] == "owner")
        && role != Some("owner")
        && related(app, Collection::Members, caller, wid)?
            .iter()
            .filter(|r| r.body["role"] == "owner")
            .count()
            <= 1
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "last_owner",
            "a workspace must retain an owner",
        ));
    }
    Ok(())
}
async fn set_member(
    State(app): State<Shared>,
    Path((wid, subject)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    crate::api::JsonBody(body, _): crate::api::JsonBody<Value>,
) -> Result<Json<Value>, ApiError> {
    let workspace = auth::authorize_workspace(&app, &caller, &wid, "admin")?;
    let key = idem_key(&caller, &headers, &format!("{wid}/members/{subject}/set"))?;
    if let Some(old) = replay(&app, &caller, &key, &body)? {
        return Ok(Json(old));
    }
    revision(&workspace, &body)?;
    let role = text(&body, "role")?;
    if !matches!(role, "owner" | "editor" | "reviewer" | "viewer") {
        return Err(ApiError::bad_request("unknown membership role"));
    }
    target_allowed(&app, &caller, &subject, &workspace, role)?;
    let member_id = format!("{wid}:{subject}");
    let existing = app.registry.console_get(Collection::Members, &member_id)?;
    last_owner(&app, &caller, &wid, existing.as_ref(), Some(role))?;
    let value = json!({"workspace_id":wid,"subject":subject,"role":role,"granted_by":caller.0.connector_subject(),"credential":caller.name(),"created_at_ms":branchyard_support::time::now_ms()});
    let row = match existing {
        Some(row) => update(row, value.clone()),
        None => insert(new(Collection::Members, &member_id, &caller, value.clone())),
    };
    app.registry.console_mutate(&[
        row,
        update(workspace.clone(), workspace.body.clone()),
        receipt(&caller, &key, &body, &value),
    ])?;
    Ok(Json(value))
}
async fn remove_member(
    State(app): State<Shared>,
    Path((wid, subject)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    crate::api::JsonBody(body, _): crate::api::JsonBody<Value>,
) -> Result<Json<Value>, ApiError> {
    let workspace = auth::authorize_workspace(&app, &caller, &wid, "admin")?;
    let key = idem_key(
        &caller,
        &headers,
        &format!("{wid}/members/{subject}/delete"),
    )?;
    if let Some(old) = replay(&app, &caller, &key, &body)? {
        return Ok(Json(old));
    }
    revision(&workspace, &body)?;
    let member_id = format!("{wid}:{subject}");
    let row = super::get(&app, Collection::Members, &member_id)?;
    last_owner(&app, &caller, &wid, Some(&row), None)?;
    let response = json!({"workspace_id":wid,"subject":subject,"removed":true,"revision":workspace.revision+1});
    app.registry.console_mutate(&[
        Mutation::Delete {
            collection: Collection::Members,
            id: row.id,
            tenant: row.tenant,
            expected: row.revision,
        },
        update(workspace.clone(), workspace.body.clone()),
        receipt(&caller, &key, &body, &response),
    ])?;
    Ok(Json(response))
}
async fn comments(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, ApiError> {
    auth::authorize_workspace(&app, &caller, &wid, "read")?;
    Ok(Json(
        json!({"comments":related(&app,Collection::Comments,&caller,&wid)?.iter().map(public).collect::<Vec<_>>()}),
    ))
}
async fn comment(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    crate::api::JsonBody(body, _): crate::api::JsonBody<Value>,
) -> Result<Json<Value>, ApiError> {
    let workspace = auth::authorize_workspace(&app, &caller, &wid, "comment")?;
    let key = idem_key(&caller, &headers, &format!("{wid}/comments"))?;
    if let Some(old) = replay(&app, &caller, &key, &body)? {
        return Ok(Json(old));
    }
    revision(&workspace, &body)?;
    let cid = id()?;
    let row = new(
        Collection::Comments,
        &cid,
        &caller,
        json!({"id":cid,"workspace_id":wid,"subject":caller.0.connector_subject(),"credential":caller.name(),"text":text(&body,"text")?,"created_at_ms":branchyard_support::time::now_ms()}),
    );
    let response = public(&row);
    app.registry.console_mutate(&[
        insert(row),
        update(workspace.clone(), workspace.body.clone()),
        receipt(&caller, &key, &body, &response),
    ])?;
    Ok(Json(response))
}
async fn presence(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, ApiError> {
    auth::authorize_workspace(&app, &caller, &wid, "read")?;
    let now = branchyard_support::time::now_ms();
    let rows = related(&app, Collection::Presence, &caller, &wid)?;
    Ok(Json(
        json!({"presence":rows.iter().filter(|r|r.body["expires_at_ms"].as_u64().is_some_and(|t|t>now)).map(public).collect::<Vec<_>>()}),
    ))
}
async fn touch_presence(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    crate::api::JsonBody(body, _): crate::api::JsonBody<Value>,
) -> Result<Json<Value>, ApiError> {
    auth::authorize_workspace(&app, &caller, &wid, "read")?;
    let key = idem_key(&caller, &headers, &format!("{wid}/presence"))?;
    if let Some(old) = replay(&app, &caller, &key, &body)? {
        return Ok(Json(old));
    }
    let ttl = body
        .get("ttl_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(60);
    if !(1..=300).contains(&ttl) {
        return Err(ApiError::bad_request(
            "presence ttl_seconds must be between 1 and 300",
        ));
    }
    let pid = format!("{wid}:{}", caller.0.connector_subject());
    let value = json!({"workspace_id":wid,"subject":caller.0.connector_subject(),"credential":caller.name(),"expires_at_ms":branchyard_support::time::now_ms().saturating_add(ttl*1000)});
    let row = match app.registry.console_get(Collection::Presence, &pid)? {
        Some(row) => update(row, value.clone()),
        None => insert(new(Collection::Presence, &pid, &caller, value.clone())),
    };
    app.registry
        .console_mutate(&[row, receipt(&caller, &key, &body, &value)])?;
    Ok(Json(value))
}
