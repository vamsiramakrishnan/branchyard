//! HTTP routes for [shared storage](../../../docs/storage.md): artifacts
//! and scratch areas, reached remotely for the first time here. Kept in
//! its own module, merged into the main router with one line in
//! [`crate::api::router`], so this feature's routes are easy to merge
//! alongside unrelated work on `api.rs` (inbox messages, branch
//! lifecycle).
//!
//! Every route acts with the server's authority as a person, exactly as
//! local `by artifact … --branch B` and `by scratch … --branch B` do
//! outside a harness: `{branch}` in the path is the acting branch, not a
//! harness's own (there is no harness here to act as anything else).
//!
//! A publish's bytes are the request body, bounded by
//! [`crate::config::Config::max_artifact_bytes`] (`413 body_too_large`
//! over it); a download streams them back with a digest header the client
//! checks against the metadata it already fetched. Everything else is
//! ordinary JSON, matching the style of the other routes.
//!
//! Every handler here goes through `Yard`'s public storage methods
//! (`publish_artifact`, `artifacts`, `read_artifact`, `share_artifact`,
//! and the scratch ones) — never `branchyard::storage`'s own internals —
//! round-tripping a publish's and a download's bytes through a
//! `TempFile`, since those methods are path-based. That keeps this
//! module clean of grant, hashing and GC details that belong to
//! `branchyard::storage` alone and may change there independently.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};

use branchyard::{ArtifactRef, ScratchArea, ScratchLock, Yard};
use branchyard_client::storage_api::{
    Ack, ArtifactList, CreateScratchRequest, Empty, LockState, ScratchList, ShareRequest,
    DIGEST_HEADER,
};

use crate::api::{blocking, Caller, JsonBody, Shared};
use crate::error::{self, ApiError};

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route(
            "/v1/repos/{repo}/branches/{branch}/artifacts",
            post(publish).get(list_artifacts),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/artifacts/{id}",
            get(artifact_meta),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/artifacts/{id}/content",
            get(artifact_content),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/artifacts/{id}/share",
            post(share_artifact),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/scratch",
            post(create_scratch).get(list_scratch),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/scratch/{name}/share",
            post(share_scratch),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/scratch/{name}/lock",
            post(lock_scratch),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/scratch/{name}/unlock",
            post(unlock_scratch),
        )
        // Not scoped to an acting branch, but only for a repository the
        // caller's tenant owns, with the `read` scope.
        .route("/v1/repos/{repo}/scratch/{name}/lock", get(lock_state))
}

/// A too-large artifact upload: `413`, with the limit in `detail`, like
/// `body_too_large` for an oversized JSON request body.
fn too_large(limit: usize) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "body_too_large",
        format!("an artifact must be at most {limit} bytes"),
    )
    .detail(serde_json::json!({ "limit": limit }))
}

/// `Idempotency-Key`, validated like the operation registry's own (1 to
/// 255 visible ASCII characters).
fn idem_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let key = value
        .to_str()
        .ok()
        .filter(|k| !k.is_empty() && k.len() <= 255 && k.bytes().all(|b| b.is_ascii_graphic()))
        .ok_or_else(|| {
            ApiError::bad_request("Idempotency-Key must be 1 to 255 visible ASCII characters")
        })?;
    Ok(Some(key.to_owned()))
}

/// FNV-1a, 64-bit, over a route, its query string and a body: a stable
/// fingerprint for [`StorageIdem`], not a security boundary. Independent
/// of `api::fingerprint`, which only fingerprints JSON request bodies.
fn fingerprint_bytes(route: &str, query: &str, body: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    };
    for byte in route.bytes().chain(std::iter::once(b'\n')) {
        mix(byte);
    }
    for byte in query.bytes().chain(std::iter::once(b'\n')) {
        mix(byte);
    }
    for byte in body {
        mix(*byte);
    }
    format!("{hash:016x}")
}

/// A quick, synchronous idempotency cache for the storage routes: unlike
/// the operation registry's, it is not durable across a restart (like
/// branch locks, see `docs/server.md`'s "What is durable" table), since a
/// publish, unlike a task or a merge, finishes within the request. Used
/// for `publish` only: `create_scratch`, the shares, and lock/unlock are
/// naturally safe to repeat (a share or unlock is a no-op the second
/// time, a lock is re-entrant for its own holder, and a repeated
/// `create_scratch` gets a clear "already exists" the caller can treat as
/// success).
type IdemEntry = (String, StatusCode, serde_json::Value);

#[derive(Default)]
pub struct StorageIdem(Mutex<HashMap<(String, String), IdemEntry>>);

impl StorageIdem {
    #[allow(clippy::expect_used, clippy::unwrap_in_result)] // ratchet: branchyard-server
    fn get(
        &self,
        caller: &str,
        key: &str,
        fingerprint: &str,
    ) -> Result<Option<(StatusCode, serde_json::Value)>, ApiError> {
        let map = self.0.lock().expect("not poisoned");
        match map.get(&(caller.to_owned(), key.to_owned())) {
            Some((stored, status, value)) if stored == fingerprint => {
                Ok(Some((*status, value.clone())))
            }
            Some(_) => Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency_key_reused",
                "Idempotency-Key was already used for a different request",
            )),
            None => Ok(None),
        }
    }

    #[allow(clippy::expect_used)] // ratchet: branchyard-server
    fn put(
        &self,
        caller: &str,
        key: &str,
        fingerprint: &str,
        status: StatusCode,
        value: &ArtifactRef,
    ) {
        let value = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
        self.0.lock().expect("not poisoned").insert(
            (caller.to_owned(), key.to_owned()),
            (fingerprint.to_owned(), status, value),
        );
    }
}

/// A cached response, replayed with `Idempotent-Replayed: true` like the
/// operation registry's own replays.
fn replayed(status: StatusCode, value: serde_json::Value) -> Response {
    let mut response = (status, Json(value)).into_response();
    response
        .headers_mut()
        .insert("idempotent-replayed", HeaderValue::from_static("true"));
    response
}

/// `name`, `media_type` and repeated `label=KEY=VALUE` query parameters,
/// as [`branchyard_client::Repo::publish_artifact`] sends them.
fn parse_publish_query(query: &str) -> (Option<String>, Option<String>, BTreeMap<String, String>) {
    let mut name = None;
    let mut media_type = None;
    let mut labels = BTreeMap::new();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = branchyard_client::http::decode(value);
        match key {
            "name" => name = Some(value),
            "media_type" => media_type = Some(value),
            "label" => {
                if let Some((k, v)) = value.split_once('=') {
                    labels.insert(k.to_owned(), v.to_owned());
                }
            }
            _ => {}
        }
    }
    (name, media_type, labels)
}

/// A path under the system temp directory, removed on drop: round-trips
/// bytes through the public, path-based `Yard::publish_artifact` and
/// `Yard::read_artifact`, the same ones local mode and every delegation
/// surface use. Kept off `branchyard::storage`'s own internals (grants,
/// hashing, GC) on purpose, so a change there needs no matching change
/// here.
struct TempFile(std::path::PathBuf);

impl TempFile {
    fn new(prefix: &str) -> TempFile {
        let name = format!(
            "branchyard-server-{prefix}-{}-{}",
            std::process::id(),
            branchyard_client::new_key()
        );
        TempFile(std::env::temp_dir().join(name))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        branchyard_support::cleanup_file(&self.0);
    }
}

/// `POST /v1/repos/{repo}/branches/{branch}/artifacts`: the request body
/// is the artifact's bytes, bounded by `max_artifact_bytes`; `name`,
/// `media_type` and `label` are query parameters, since the body is not
/// JSON. Written to a temporary file and published through
/// [`branchyard::Yard::publish_artifact`], so digest dedup, grants and GC
/// stay exactly local mode's.
async fn publish(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
    let query = request.uri().query().unwrap_or("").to_owned();
    let idem_key = idem_key(request.headers())?;
    let limit = app.config.max_artifact_bytes as usize;
    let body = axum::body::to_bytes(request.into_body(), limit)
        .await
        .map_err(|_| too_large(limit))?;
    let route = format!("POST /v1/repos/{}/branches/{branch}/artifacts", repo.name);
    let fingerprint = idem_key
        .as_ref()
        .map(|_| fingerprint_bytes(&route, &query, &body));
    if let (Some(key), Some(fp)) = (&idem_key, &fingerprint) {
        if let Some((status, value)) = app.storage_idem.get(&caller.idempotency_scope(), key, fp)? {
            return Ok(replayed(status, value));
        }
    }
    // After the replay: a retried upload that already published is
    // answered as before, not refused for the bytes it already added.
    app.check_artifact_upload(&caller, body.len() as u64)
        .await?;
    let (name, media_type, labels) = parse_publish_query(&query);
    let (yard, acting) = (repo.yard.clone(), branch.clone());
    let artifact = blocking(move || {
        let temp = TempFile::new("upload");
        std::fs::write(temp.path(), &body).map_err(|e| {
            branchyard::Error::State(format!("write {}: {e}", temp.path().display()))
        })?;
        yard.publish_artifact(&acting, temp.path(), name, media_type, labels)
    })
    .await?
    .map_err(|e| error::sdk(&e))?;
    if let (Some(key), Some(fp)) = (&idem_key, &fingerprint) {
        app.storage_idem.put(
            &caller.idempotency_scope(),
            key,
            fp,
            StatusCode::CREATED,
            &artifact,
        );
    }
    Ok((StatusCode::CREATED, Json(artifact)).into_response())
}

/// Artifact `id`'s provenance and bytes, for `branch`, through the public
/// `Yard::read_artifact` (which writes to a path): a temporary file
/// stands in for the response body [`artifact_content`] streams, or is
/// discarded for [`artifact_meta`]'s metadata-only answer.
fn read_via_temp(
    yard: &Yard,
    branch: &str,
    id: &str,
) -> Result<(ArtifactRef, Vec<u8>), branchyard::Error> {
    let temp = TempFile::new("download");
    let artifact = yard.read_artifact(branch, id, temp.path())?;
    let bytes = std::fs::read(temp.path())
        .map_err(|e| branchyard::Error::State(format!("read {}: {e}", temp.path().display())))?;
    Ok((artifact, bytes))
}

async fn list_artifacts(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<ArtifactList>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let artifacts = blocking(move || yard.artifacts(&branch))
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(ArtifactList { artifacts }))
}

/// `GET .../artifacts/{id}`: provenance only, so a caller need not
/// download the bytes just to look at the metadata. Reads (and digest
/// checks) the bytes anyway, since that is the only way this reader's
/// grant is checked; the server's HTTP API has no separate metadata-only
/// storage path.
async fn artifact_meta(
    State(app): State<Shared>,
    Path((repo, branch, id)): Path<(String, String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<ArtifactRef>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let artifact = blocking(move || read_via_temp(&yard, &branch, &id).map(|(a, _)| a))
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(artifact))
}

async fn artifact_content(
    State(app): State<Shared>,
    Path((repo, branch, id)): Path<(String, String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Response, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let (artifact, bytes) = blocking(move || read_via_temp(&yard, &branch, &id))
        .await?
        .map_err(|e| error::sdk(&e))?;
    let content_type = HeaderValue::from_str(&artifact.media_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, artifact.size.to_string())
        .header(DIGEST_HEADER, artifact.digest.clone())
        .body(Body::from(bytes))
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(response)
}

async fn share_artifact(
    State(app): State<Shared>,
    Path((repo, branch, id)): Path<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(ShareRequest { to }, _): JsonBody<ShareRequest>,
) -> Result<Json<Ack>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    blocking(move || yard.share_artifact(&branch, &id, &to))
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(Ack { ok: true }))
}

async fn create_scratch(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(CreateScratchRequest { name }, _): JsonBody<CreateScratchRequest>,
) -> Result<Json<ScratchArea>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let area = blocking(move || yard.create_scratch(&branch, &name))
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(area))
}

async fn list_scratch(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<ScratchList>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let areas = blocking(move || yard.scratch_areas(&branch))
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(ScratchList { areas }))
}

async fn share_scratch(
    State(app): State<Shared>,
    Path((repo, branch, name)): Path<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(ShareRequest { to }, _): JsonBody<ShareRequest>,
) -> Result<Json<Ack>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    blocking(move || yard.share_scratch(&branch, &name, &to))
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(Ack { ok: true }))
}

/// Acquire scratch area `name`'s writer lock for `branch`, acting with
/// the server's authority as a person: like `by scratch lock --branch`.
/// Refused with `409 running` while another branch's turn holds it.
async fn lock_scratch(
    State(app): State<Shared>,
    Path((repo, branch, name)): Path<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(Empty {}, _): JsonBody<Empty>,
) -> Result<Json<ScratchLock>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let lock = blocking(move || yard.lock_scratch(&branch, &name))
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(lock))
}

async fn unlock_scratch(
    State(app): State<Shared>,
    Path((repo, branch, name)): Path<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(Empty {}, _): JsonBody<Empty>,
) -> Result<Json<Ack>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    blocking(move || yard.unlock_scratch(&branch, &name))
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(Ack { ok: true }))
}

/// `GET /v1/repos/{repo}/scratch/{name}/lock`: the current holder, if
/// any, whether or not its turn is still running. Not scoped to an
/// acting branch, unlike the rest of the storage routes, but the caller
/// needs `read` on a repository its tenant owns.
async fn lock_state(
    State(app): State<Shared>,
    Path((repo, name)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<LockState>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let lock = blocking(move || yard.scratch_lock_state(&name))
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(LockState { lock }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_decode_like_every_other_call_site() {
        let (name, media_type, _) = parse_publish_query("name=x%41&media_type=%zz%4+%C3%A9");
        assert_eq!(name.as_deref(), Some("xA"));
        assert_eq!(media_type.as_deref(), Some("%zz%4+\u{e9}"));
    }

    #[test]
    fn queries_round_trip_percent_encoding() {
        let query = "name=a%20b&media_type=text%2Fplain&label=k1%3Dv1&label=k2%3Dv%202";
        let (name, media_type, labels) = parse_publish_query(query);
        assert_eq!(name.as_deref(), Some("a b"));
        assert_eq!(media_type.as_deref(), Some("text/plain"));
        assert_eq!(labels.get("k1").map(String::as_str), Some("v1"));
        assert_eq!(labels.get("k2").map(String::as_str), Some("v 2"));
    }

    #[test]
    fn fingerprints_differ_on_body() {
        assert_ne!(
            fingerprint_bytes("r", "q", b"a"),
            fingerprint_bytes("r", "q", b"b")
        );
        assert_eq!(
            fingerprint_bytes("r", "q", b"a"),
            fingerprint_bytes("r", "q", b"a")
        );
    }
}
