//! Original artifact grant checks at every preview and derivative request.

use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde_json::Value;

use super::cache::read_regular;
use super::{io_error, PreviewInput, PreviewOptions, PreviewService, MAX_INPUT};
use crate::api::{blocking, Caller, Shared};
use crate::error::{self, ApiError};

/// Legacy branch-artifact routes, mounted with one shared renderer instance.
pub(crate) fn router(service: Arc<PreviewService>) -> Router<Shared> {
    Router::new()
        .route(
            "/v1/repos/{repo}/branches/{branch}/artifacts/{id}/preview",
            get(preview_route),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/artifacts/{id}/preview/assets/{asset}",
            get(asset_route),
        )
        .layer(Extension(service))
}

async fn preview_route(
    State(app): State<Shared>,
    Path((repo, branch, id)): Path<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    Extension(service): Extension<Arc<PreviewService>>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let options = PreviewOptions::parse(query.as_deref())?;
    let input = authorized_input(&app, &caller, &repo, branch, id).await?;
    Ok(Json(service.preview(input, options).await?))
}

async fn asset_route(
    State(app): State<Shared>,
    Path((repo, branch, id, asset)): Path<(String, String, String, String)>,
    Extension(caller): Extension<Caller>,
    Extension(service): Extension<Arc<PreviewService>>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let options = PreviewOptions::parse(query.as_deref())?;
    let input = authorized_input(&app, &caller, &repo, branch, id).await?;
    let mut response = service.asset(input, options, &asset).await?.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

/// Reauthorize and digest-check the original before every derivative request.
pub(crate) async fn authorized_input(
    app: &Shared,
    caller: &Caller,
    repo: &str,
    branch: String,
    id: String,
) -> Result<PreviewInput, ApiError> {
    let yard = app.authorized_repo(caller, repo, "read")?.yard.clone();
    let tenant = caller.tenant().to_owned();
    blocking(move || {
        let metadata = yard
            .artifacts(&branch)
            .map_err(|e| error::sdk(&e))?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "unknown_artifact",
                    "artifact not found",
                )
            })?;
        if metadata.size > MAX_INPUT {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "preview_too_large",
                "document exceeds the 64 MiB preview limit",
            ));
        }
        let temp = tempfile::tempdir().map_err(io_error)?;
        let path = temp.path().join("original");
        let metadata = yard
            .read_artifact(&branch, &id, &path)
            .map_err(|e| error::sdk(&e))?;
        let bytes = read_regular(&path, MAX_INPUT)?;
        Ok(PreviewInput {
            bytes,
            name: metadata.name,
            media_type: metadata.media_type,
            digest: metadata.digest,
            tenant,
        })
    })
    .await?
}
