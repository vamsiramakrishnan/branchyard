use crate::{config::Principal, Error, Result, Store};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use branchyard_protocol::*;
use serde::Deserialize;

fn authenticate(store: &Store, headers: &HeaderMap) -> Result<Principal> {
    if headers.get_all("authorization").iter().count() != 1 {
        return Err(Error::Unauthorized);
    }
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or(Error::Unauthorized)?;
    store.config.authenticate(token)
}
pub fn router(store: Store) -> Router {
    Router::new()
        .route("/v1alpha1/info", get(info))
        .route("/v1alpha1/commands", post(submit))
        .route("/v1alpha1/operations/{id}", get(operation))
        .route("/v1alpha1/tasks/{id}", get(task))
        .route("/v1alpha1/tasks/{id}/events", get(events))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            std::sync::Arc::new(tokio::sync::Semaphore::new(128)),
            |State(slots): State<std::sync::Arc<tokio::sync::Semaphore>>,
             request: axum::extract::Request,
             next: axum::middleware::Next| async move {
                let Ok(_permit) = slots.try_acquire_owned() else {
                    return Error::Capacity.into_response();
                };
                next.run(request).await
            },
        ))
        .with_state(store)
}
async fn info(State(store): State<Store>, headers: HeaderMap) -> Result<Json<ServerInfo>> {
    let principal = authenticate(&store, &headers)?;
    principal.allows("read")?;
    Ok(Json(ServerInfo {
        schema: Version::V1Alpha1,
        execution_ready: false,
        operations: principal
            .actions
            .iter()
            .filter(|a| {
                a.as_str() != "read"
                    && !(principal.subtree.is_some() && a.as_str() == "create_task")
            })
            .cloned()
            .collect(),
        harness_profiles: store.config.tenants[&principal.tenant]
            .harnesses
            .values()
            .cloned()
            .collect(),
    }))
}
async fn submit(
    State(store): State<Store>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Receipt>)> {
    let principal = authenticate(&store, &headers)?;
    let command: Command = serde_json::from_slice(&body).map_err(|_| Error::Invalid)?;
    let fingerprint = command.fingerprint().map_err(|_| Error::Invalid)?;
    for (key, expected) in [
        ("idempotency-key", command.operation_id.to_string()),
        ("x-branchyard-request-sha256", fingerprint),
    ] {
        if headers.get_all(key).iter().count() != 1
            || headers.get(key).and_then(|h| h.to_str().ok()) != Some(expected.as_str())
        {
            return Err(Error::Invalid);
        }
    }
    let receipt = store.submit(&principal, &command).await?;
    Ok((StatusCode::ACCEPTED, Json(receipt)))
}
async fn operation(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(id): Path<OperationId>,
) -> Result<Json<Operation>> {
    Ok(Json(
        store
            .operation(&authenticate(&store, &headers)?, id)
            .await?,
    ))
}
async fn task(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(id): Path<TaskId>,
) -> Result<Json<Task>> {
    Ok(Json(
        store.task(&authenticate(&store, &headers)?, id).await?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    after: u64,
    limit: u16,
}
async fn events(
    State(store): State<Store>,
    headers: HeaderMap,
    Path(id): Path<TaskId>,
    Query(page): Query<Page>,
) -> Result<Json<EventPage>> {
    Ok(Json(
        store
            .events(&authenticate(&store, &headers)?, id, page.after, page.limit)
            .await?,
    ))
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict => StatusCode::CONFLICT,
            Self::Capacity => StatusCode::TOO_MANY_REQUESTS,
            Self::Unsupported => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Invalid => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(serde_json::json!({"error":self.to_string()}))).into_response()
    }
}
