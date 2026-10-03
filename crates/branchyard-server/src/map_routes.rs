//! HTTP routes for [wide maps](../../../docs/map.md), merged into the main
//! router with one line in [`crate::api::router`], like
//! [`crate::knowledge_routes`].
//!
//! A map runs as one operation (`map`), admitted like a task and claimed by
//! any worker serving the repository, which runs the map's branches on
//! threads of its own process. Its record is in that worker's
//! `.branchyard/maps/`; reading it (`GET .../maps`) needs the `read` scope,
//! starting, resuming and forgetting one the `run` scope. The operation
//! locks the map's name, so one map never runs twice at once.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};

use branchyard::MapReport;
use branchyard_client::api::{MapList, MapRequest, MapResumeRequest, OperationKind, Removed};

use crate::api::{blocking, Caller, JsonBody, Shared};
use crate::error::{self, ApiError};
use crate::ops::NewOperation;
use crate::work::Work;

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/repos/{repo}/maps", get(list_maps).post(post_map))
        .route(
            "/v1/repos/{repo}/maps/{name}",
            get(map_report).delete(remove_map),
        )
        .route("/v1/repos/{repo}/maps/{name}/resume", post(resume_map))
}

fn sdk<T>(result: Result<T, branchyard::Error>) -> Result<T, ApiError> {
    result.map_err(|e| error::sdk(&e))
}

/// The lock an operation running map `name` holds: not a branch name, as
/// branch names have no `:`.
fn lock(name: &str) -> String {
    format!("map:{name}")
}

async fn list_maps(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<MapList>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let maps = sdk(blocking(move || yard.maps()).await?)?;
    Ok(Json(MapList { maps }))
}

async fn map_report(
    State(app): State<Shared>,
    Path((repo, name)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<MapReport>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    Ok(Json(sdk(blocking(move || yard.map_report(&name)).await?)?))
}

async fn remove_map(
    State(app): State<Shared>,
    Path((repo, name)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Removed>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let removed = name.clone();
    sdk(blocking(move || yard.remove_map(&name)).await?)?;
    Ok(Json(Removed { removed }))
}

/// Admit `request` on `repo` as a map operation; `route` names the
/// request for its idempotency key.
async fn admit_map(
    app: Shared,
    caller: Caller,
    repo: String,
    headers: HeaderMap,
    (route, canonical): (String, String),
    request: MapRequest,
) -> Result<Response, ApiError> {
    let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
    let idem = crate::api::idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(response) = crate::api::replayed(&app, &caller, idem.as_ref()).await? {
        return Ok(response);
    }
    let policy = app.tenant_policy(&caller);
    app.check_admission_quotas(&caller, &policy).await?;
    // Refused now, not as a failed operation: what running it would refuse.
    let spec = crate::work::map_spec(&request)?;
    crate::work::task_options(&app, &repo, &request.task)?;
    let cursor = crate::api::sync_feed(&repo.feed).await?;
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Map,
        creates: Vec::new(),
        branches: Vec::new(),
        cursor,
        locks: vec![lock(&spec.name)],
        idempotency: idem,
        principal: caller.0.clone(),
        quota: app.admission_quota(&caller, &policy),
        requires: crate::api::required_labels(&request.task.require_labels)?,
        priority: crate::api::admitted_priority(&policy, request.task.priority, 0)?,
        trace: crate::api::incoming_trace(&headers),
    };
    crate::api::admit(&app, new, Work::Map { request }).await
}

async fn post_map(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(request, canonical): JsonBody<MapRequest>,
) -> Result<Response, ApiError> {
    let route = format!("POST /v1/repos/{repo}/maps");
    admit_map(app, caller, repo, headers, (route, canonical), request).await
}

async fn resume_map(
    State(app): State<Shared>,
    Path((repo, name)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(resume, canonical): JsonBody<MapResumeRequest>,
) -> Result<Response, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let spec = {
        let name = name.clone();
        sdk(blocking(move || yard.map_spec(&name)).await?)?
    };
    let mut request: MapRequest = serde_json::from_value(spec.launch.clone()).map_err(|_| {
        ApiError::bad_request(format!(
            "map {name} was not started through this server's API, so it has no request to \
             resume; resume it where it was started"
        ))
    })?;
    request.items = spec.items;
    request.retry_failed = resume.retry_failed;
    let route = format!("POST /v1/repos/{repo}/maps/{name}/resume");
    admit_map(app, caller, repo, headers, (route, canonical), request).await
}
