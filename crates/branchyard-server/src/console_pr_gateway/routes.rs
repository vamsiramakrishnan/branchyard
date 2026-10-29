//! HTTP adapters for the console-owned PR gateway.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use super::{valid_key, PrGateway};
use crate::api::{blocking, Caller};
use crate::console_pr::Publish;
use crate::error::ApiError;

/// Extract the ordinary authenticated caller; this router never authenticates a token itself.
fn caller(value: Option<Extension<Caller>>) -> Result<Caller, ApiError> {
    value
        .map(|value| value.0)
        .ok_or_else(ApiError::unauthorized)
}

/// Require bounded idempotency before admitting any durable mutation.
fn key(headers: &HeaderMap) -> Result<String, ApiError> {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|key| valid_key(key))
        .map(str::to_owned)
        .ok_or_else(|| {
            ApiError::bad_request("Idempotency-Key must be 1 to 255 visible ASCII characters")
        })
}

/// Parse closed request types and preserve the body-size failure category.
fn request<T>(value: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    value.map(|value| value.0).map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "PR JSON must fit within 4 KiB",
            )
        } else {
            ApiError::bad_request(error.body_text())
        }
    })
}

/// Disable shared caching and content sniffing for membership-bound PR state.
fn private(value: impl Serialize, status: StatusCode) -> Response {
    let mut response = Json(value).into_response();
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Mount behind the existing authentication middleware; central state stays with its owner.
pub fn router<S>(gateway: Arc<PrGateway>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/v1/workspaces/{workspace}/sessions/{session}/pr",
            get(inspect).post(publish),
        )
        .route(
            "/v1/workspaces/{workspace}/sessions/{session}/pr/refresh",
            post(refresh),
        )
        .layer(DefaultBodyLimit::max(4 * 1024))
        .layer(Extension(gateway))
}

/// Read authorized cached observations on the ordinary blocking pool.
async fn inspect(
    Extension(gateway): Extension<Arc<PrGateway>>,
    authenticated: Option<Extension<Caller>>,
    Path((workspace, session)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let caller = caller(authenticated)?;
    let result = blocking(move || gateway.inspect(&caller, &workspace, &session)).await??;
    Ok(private(result, StatusCode::OK))
}

/// Admit a closed publication request; no subprocess runs on the HTTP task.
async fn publish(
    Extension(gateway): Extension<Arc<PrGateway>>,
    authenticated: Option<Extension<Caller>>,
    Path((workspace, session)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<Publish>, JsonRejection>,
) -> Result<Response, ApiError> {
    let caller = caller(authenticated)?;
    let key = key(&headers)?;
    let request = request(body)?;
    let operation =
        blocking(move || gateway.admit(&caller, &workspace, &session, Some(request), &key))
            .await??;
    Ok(private(operation, StatusCode::ACCEPTED))
}

/// Empty refresh input, rejecting host paths, executable overrides and commands.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Refresh {}

/// Refresh through the same durable queue, rather than an untracked request task.
async fn refresh(
    Extension(gateway): Extension<Arc<PrGateway>>,
    authenticated: Option<Extension<Caller>>,
    Path((workspace, session)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<Refresh>, JsonRejection>,
) -> Result<Response, ApiError> {
    let caller = caller(authenticated)?;
    let key = key(&headers)?;
    request(body)?;
    let operation =
        blocking(move || gateway.admit(&caller, &workspace, &session, None, &key)).await??;
    Ok(private(operation, StatusCode::ACCEPTED))
}
