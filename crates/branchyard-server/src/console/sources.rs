//! Immutable original inputs, explicit attempt copies and artifact acceptance.
use super::*;
use crate::api::{App, Caller, RepoState, Shared};
use crate::console_store::{Collection, Record};
use crate::error::ApiError;
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Extension, Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path as FsPath;

pub(super) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/workspaces/{workspace}/sources", get(list).post(upload))
        .route(
            "/v1/workspaces/{workspace}/sources/{source}/content",
            get(download),
        )
        .route("/v1/workspaces/{workspace}/artifacts", get(artifacts))
        .route(
            "/v1/workspaces/{workspace}/artifacts/{artifact}/content",
            get(artifact_content),
        )
}
async fn list(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, ApiError> {
    auth::authorize_workspace(&app, &caller, &wid, "read")?;
    Ok(Json(
        json!({"sources":related(&app,Collection::Sources,&caller,&wid)?.iter().map(public).collect::<Vec<_>>()}),
    ))
}
fn source(app: &App, caller: &Caller, wid: &str, sid: &str) -> Result<Record, ApiError> {
    let row = super::get(app, Collection::Sources, sid)?;
    if row.tenant != caller.tenant() || row.body["workspace_id"] != wid {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such source",
        ));
    }
    Ok(row)
}
fn bytes(row: &Record) -> Result<Vec<u8>, ApiError> {
    let data = STANDARD
        .decode(
            row.body
                .get("_bytes")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::internal("source original bytes missing"))?,
        )
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if row.body["digest"] != hex::encode(Sha256::digest(&data)) {
        return Err(ApiError::internal("immutable source digest mismatch"));
    }
    Ok(data)
}
fn source_bytes(app: &App, metadata: &Record) -> Result<Vec<u8>, ApiError> {
    let blob = super::get(app, Collection::SourceBlobs, &metadata.id)?;
    if blob.tenant != metadata.tenant
        || blob.body["workspace_id"] != metadata.body["workspace_id"]
        || blob.body["digest"] != metadata.body["digest"]
    {
        return Err(ApiError::internal(
            "immutable source content binding mismatch",
        ));
    }
    bytes(&blob)
}

async fn upload(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    request: axum::extract::Request,
) -> Result<Json<Value>, ApiError> {
    let workspace = auth::authorize_workspace(&app, &caller, &wid, "run")?;
    let data = axum::body::to_bytes(
        request.into_body(),
        app.config.managed_knowledge.max_upload_bytes,
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "source_too_large",
            "source exceeds the configured upload limit",
        )
    })?;
    if data.is_empty() || data.len() > app.config.managed_knowledge.max_upload_bytes {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "source_too_large",
            "source must be nonempty and within the configured upload limit",
        ));
    }
    let title = headers
        .get("x-source-title")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 512)
        .ok_or_else(|| ApiError::bad_request("X-Source-Title is required, at most 512 bytes"))?;
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() <= 200)
        .unwrap_or("application/octet-stream");
    let digest = hex::encode(Sha256::digest(&data));
    let request = json!({"title":title,"media_type":media,"digest":digest});
    let key = idem_key(&caller, &headers, &format!("{wid}/sources"))?;
    if let Some(old) = replay(&app, &caller, &key, &request)? {
        return Ok(Json(old));
    }
    let asked = headers
        .get("x-workspace-revision")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    revision(&workspace, &json!({"revision":asked}))?;
    let total = workspace.body["_source_bytes"]
        .as_u64()
        .unwrap_or(0)
        .checked_add(data.len() as u64)
        .ok_or_else(|| ApiError::bad_request("source quota overflow"))?;
    if total > app.config.managed_knowledge.max_source_bytes {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            "workspace original source byte quota reached",
        ));
    }
    let sid = id()?;
    let row = new(
        Collection::Sources,
        &sid,
        &caller,
        json!({"id":sid,"workspace_id":wid,"title":title,"media_type":media,"digest":digest,"size_bytes":data.len(),"created_at_ms":branchyard_support::time::now_ms(),"uploaded_by":caller.0.connector_subject(),"credential":caller.name()}),
    );
    let blob = new(
        Collection::SourceBlobs,
        &sid,
        &caller,
        json!({"id":sid,"workspace_id":wid,"digest":digest,"_bytes":STANDARD.encode(&data)}),
    );
    let response = public(&row);
    let mut body = workspace.body.clone();
    body["_source_bytes"] = json!(total);
    let changes = [
        insert(row),
        insert(blob),
        update(workspace, body),
        receipt(&caller, &key, &request, &response),
    ];
    match app.registry.console_mutate(&changes) {
        Ok(()) => Ok(Json(response)),
        Err(e) => match replay(&app, &caller, &key, &request)? {
            Some(old) => Ok(Json(old)),
            None => Err(e),
        },
    }
}
fn content(data: Vec<u8>, media: &str) -> Result<Response, ApiError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        media
            .parse()
            .map_err(|_| ApiError::bad_request("invalid media type"))?,
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static("attachment"),
    );
    headers.insert(
        "x-content-type-options",
        axum::http::HeaderValue::from_static("nosniff"),
    );
    Ok((headers, data).into_response())
}
/// Read an authorized immutable original for download or the rendering service.
pub(crate) fn source_original(
    app: &App,
    caller: &Caller,
    workspace: &str,
    source_id: &str,
) -> Result<(branchyard_client::workspace_api::Source, Vec<u8>), ApiError> {
    auth::authorize_workspace(app, caller, workspace, "read")?;
    let row = source(app, caller, workspace, source_id)?;
    let metadata = serde_json::from_value(public(&row))
        .map_err(|error| ApiError::internal(error.to_string()))?;
    Ok((metadata, source_bytes(app, &row)?))
}

async fn download(
    State(app): State<Shared>,
    Path((wid, sid)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Response, ApiError> {
    let (metadata, original) = source_original(&app, &caller, &wid, &sid)?;
    content(original, &metadata.media_type)
}

pub(super) fn selection(
    app: &App,
    caller: &Caller,
    wid: &str,
    body: &Value,
) -> Result<Vec<String>, ApiError> {
    let mut ids = Vec::new();
    if let Some(value) = body.get("sources").filter(|value| !value.is_null()) {
        let rows = value
            .as_array()
            .ok_or_else(|| ApiError::bad_request("sources must be source IDs"))?;
        for value in rows {
            let sid = value
                .as_str()
                .ok_or_else(|| ApiError::bad_request("sources must be source IDs"))?;
            source(app, caller, wid, sid)?;
            if !ids.contains(&sid.to_owned()) {
                ids.push(sid.into());
            }
        }
    }
    Ok(ids)
}
pub(super) fn review_inputs(
    app: &App,
    caller: &Caller,
    workspace: &Record,
    repo: &RepoState,
    body: &Value,
) -> Result<Vec<Value>, ApiError> {
    let mut result = Vec::new();
    if let Some(value) = body.get("artifacts").filter(|value| !value.is_null()) {
        for value in value
            .as_array()
            .ok_or_else(|| ApiError::bad_request("artifacts must be artifact IDs"))?
        {
            let aid = value
                .as_str()
                .ok_or_else(|| ApiError::bad_request("artifacts must be artifact IDs"))?;
            let (artifact, reader) = find_artifact(app, caller, workspace, repo, aid)?;
            result.push(json!({"id":artifact.id,"digest":artifact.digest,"reader":reader,"media_type":artifact.media_type,"name":artifact.name}));
        }
    }
    Ok(result)
}
fn write_input(root: &FsPath, relative: &str, data: &[u8]) -> Result<(), ApiError> {
    let path = root.join(relative);
    let parent = path
        .parent()
        .ok_or_else(|| ApiError::internal("input has no parent"))?;
    let mut current = root.to_path_buf();
    for part in parent
        .strip_prefix(root)
        .map_err(|e| ApiError::internal(e.to_string()))?
        .components()
    {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                return Err(ApiError::bad_request(
                    "source copy parent is not a plain directory",
                ))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let metadata = std::fs::symlink_metadata(&current).map_err(io)?;
                        if !metadata.is_dir() || metadata.file_type().is_symlink() {
                            return Err(ApiError::bad_request(
                                "source copy parent is not a plain directory",
                            ));
                        }
                    }
                    Err(error) => return Err(io(error)),
                }
            }
            Err(e) => return Err(io(e)),
        }
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(data).map_err(io)?;
            file.sync_all().map_err(io)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = std::fs::symlink_metadata(&path).map_err(io)?;
            if !meta.is_file()
                || meta.file_type().is_symlink()
                || std::fs::read(&path).map_err(io)? != data
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "source_copy_conflict",
                    "the attempt input copy differs from its immutable original",
                ));
            }
        }
        Err(e) => return Err(io(e)),
    }
    Ok(())
}
pub(super) fn prepare(
    app: &App,
    caller: &Caller,
    workspace: &Record,
    session: &Record,
    repo: &RepoState,
    options: &mut branchyard::TaskOptions,
) -> Result<(), ApiError> {
    let mut paths = Vec::new();
    let mut provenance = String::from("\nExplicit immutable inputs for this attempt:\n");
    if let Some(sources) = session.body["sources"].as_array() {
        for sid in sources {
            let sid = sid
                .as_str()
                .ok_or_else(|| ApiError::internal("invalid stored source grant"))?;
            let row = source(app, caller, &workspace.id, sid)?;
            let relative = format!(".console-inputs/{}/{}/{}", workspace.id, session.id, sid);
            write_input(repo.yard.root(), &relative, &source_bytes(app, &row)?)?;
            paths.push(relative.clone());
            provenance.push_str(&format!(
                "{relative}, source {sid}, sha256 {}\n",
                text(&row.body, "digest")?
            ));
        }
    }
    if let Some(artifacts) = session.body["artifacts"].as_array() {
        for artifact in artifacts {
            let aid = text(artifact, "id")?;
            let reader = text(artifact, "reader")?;
            let relative = format!(
                ".console-inputs/{}/{}/artifact-{aid}",
                workspace.id, session.id
            );
            let temp = source_temp(app)?;
            let verified = repo
                .yard
                .read_artifact(reader, aid, temp.path())
                .map_err(sdk)?;
            if artifact["digest"] != verified.digest {
                return Err(ApiError::internal("frozen artifact digest changed"));
            }
            let data = std::fs::read(temp.path()).map_err(io)?;

            write_input(repo.yard.root(), &relative, &data)?;
            paths.push(relative.clone());
            provenance.push_str(&format!(
                "{relative}, artifact {aid}, sha256 {}\n",
                verified.digest
            ));
        }
    }
    if !paths.is_empty() {
        let spec = options.workspace.get_or_insert_with(Default::default);
        spec.copy.extend(paths);
    }
    let provision = options.provision.get_or_insert_with(Default::default);
    let instructions = provision.instructions.get_or_insert_with(String::new);
    instructions.push_str(&provenance);
    Ok(())
}
fn source_temp(app: &App) -> Result<crate::storage_routes::TempFile, ApiError> {
    let dir = app.config.data_dir.join("console-staging");
    std::fs::create_dir_all(&dir).map_err(io)?;
    Ok(crate::storage_routes::TempFile(dir.join(id()?)))
}
/// Whether a producing branch belongs to this workspace through verified ancestry.
pub(super) fn branch_owned(
    app: &App,
    caller: &Caller,
    workspace: &Record,
    repo: &RepoState,
    publisher: &str,
) -> Result<bool, ApiError> {
    let attempts: std::collections::BTreeSet<String> =
        related(app, Collection::Sessions, caller, &workspace.id)?
            .iter()
            .filter_map(|r| {
                r.body
                    .get("branch")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
    let mut current = Some(publisher.to_owned());
    let mut seen = std::collections::BTreeSet::new();
    while let Some(branch) = current {
        if !seen.insert(branch.clone()) {
            return Ok(false);
        }
        let binding = app
            .registry
            .console_get(Collection::Bindings, &format!("{}:{branch}", repo.name))?;
        if let Some(binding) = &binding {
            if binding.tenant != workspace.tenant
                || binding.body["workspace_id"] != workspace.id
                || binding.collection != Collection::Bindings
                || binding.id != format!("{}:{branch}", repo.name)
                || binding.body.get("repo").and_then(Value::as_str) != Some(repo.name.as_str())
                || binding.body.get("branch").and_then(Value::as_str) != Some(branch.as_str())
            {
                return Ok(false);
            }
        }
        if attempts.contains(&branch) {
            return Ok(binding.is_some());
        }
        current = match repo.yard.branch(&branch) {
            Ok(value) => value.info().parent.clone(),
            Err(branchyard::Error::UnknownBranch(_)) => return Ok(false),
            Err(error) => return Err(sdk(error)),
        };
    }
    Ok(false)
}
// A queued or failed initialization has no native reader yet. That absence does
// not hide artifacts produced by the workspace's other existing attempts.
fn native_artifacts(
    repo: &RepoState,
    reader: &str,
) -> Result<Vec<branchyard::ArtifactRef>, ApiError> {
    match repo.yard.artifacts(reader) {
        Ok(artifacts) => Ok(artifacts),
        Err(branchyard::Error::UnknownBranch(_)) => Ok(Vec::new()),
        Err(error) => Err(sdk(error)),
    }
}

pub(super) fn find_artifact(
    app: &App,
    caller: &Caller,
    workspace: &Record,
    repo: &RepoState,
    aid: &str,
) -> Result<(branchyard::ArtifactRef, String), ApiError> {
    for session in related(app, Collection::Sessions, caller, &workspace.id)? {
        let branch = text(&session.body, "branch")?;
        if let Some(artifact) = native_artifacts(repo, branch)?
            .into_iter()
            .find(|a| a.id == aid)
        {
            if branch_owned(app, caller, workspace, repo, &artifact.publisher_branch)? {
                return Ok((artifact, branch.into()));
            }
        }
    }
    Err(ApiError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        "no such workspace artifact",
    ))
}
async fn artifacts(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, ApiError> {
    let workspace = auth::authorize_workspace(&app, &caller, &wid, "read")?;
    let repo = super::runtime::resolve(&app, &workspace)?;
    let mut artifacts = std::collections::BTreeMap::new();
    for session in related(&app, Collection::Sessions, &caller, &wid)? {
        for artifact in native_artifacts(&repo, text(&session.body, "branch")?)? {
            if branch_owned(&app, &caller, &workspace, &repo, &artifact.publisher_branch)? {
                artifacts.insert(artifact.id.clone(), artifact);
            }
        }
    }
    Ok(Json(
        json!({"artifacts":artifacts.into_values().collect::<Vec<_>>()}),
    ))
}
async fn artifact_content(
    State(app): State<Shared>,
    Path((wid, aid)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Response, ApiError> {
    let workspace = auth::authorize_workspace(&app, &caller, &wid, "read")?;
    let repo = super::runtime::resolve(&app, &workspace)?;
    let (artifact, reader) = find_artifact(&app, &caller, &workspace, &repo, &aid)?;
    let temp = source_temp(&app)?;
    repo.yard
        .read_artifact(&reader, &aid, temp.path())
        .map_err(sdk)?;
    let data = std::fs::read(temp.path()).map_err(io)?;

    content(data, &artifact.media_type)
}
pub(super) async fn finalize(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    crate::api::JsonBody(request, _): crate::api::JsonBody<
        branchyard_client::workspace_api::FinalizeRequest,
    >,
) -> Result<Json<Value>, ApiError> {
    let body =
        serde_json::to_value(request).map_err(|error| ApiError::internal(error.to_string()))?;
    let workspace = auth::authorize_workspace(&app, &caller, &wid, "run")?;
    let key = idem_key(&caller, &headers, &format!("{wid}/finalize"))?;
    if let Some(old) = replay(&app, &caller, &key, &body)? {
        return Ok(Json(old));
    }
    revision(&workspace, &body)?;
    let selected = body["artifacts"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 1024)
        .ok_or_else(|| ApiError::bad_request("artifacts must be a nonempty bounded list of IDs"))?;
    let repo = super::runtime::resolve(&app, &workspace)?;
    let task =
        branchyard::tasks::view(&repo.yard, text(&workspace.body, "task_id")?).map_err(sdk)?;
    let pending = app.registry.unfinished(caller.tenant())?;
    let mut changes = Vec::new();
    let mut accepted = Vec::new();
    for value in selected {
        let aid = value
            .as_str()
            .ok_or_else(|| ApiError::bad_request("artifact ID must be text"))?;
        if accepted.contains(&aid.to_owned()) {
            continue;
        }
        let (artifact, reader) = find_artifact(&app, &caller, &workspace, &repo, aid)?;
        if pending.iter().any(|operation| {
            operation.operation.repo == repo.name
                && operation
                    .operation
                    .branches
                    .contains(&artifact.publisher_branch)
        }) {
            return Err(ApiError::new(StatusCode::CONFLICT, "producer_busy", "wait for the artifact producer's queued or running operation before accepting its output"));
        }
        let staging = source_temp(&app)?;
        repo.yard
            .read_artifact(&reader, aid, staging.path())
            .map_err(sdk)?;
        let attempt = task
            .attempts
            .iter()
            .find(|a| a.name == artifact.publisher_branch)
            .ok_or_else(|| ApiError::bad_request("artifact producer is not a workspace attempt"))?;
        if attempt.result_error.is_some()
            || attempt.result.is_none()
            || attempt.result_event.is_none()
            || artifact.turn != u64::from(attempt.turns)
            || !matches!(
                attempt.status,
                Some(
                    branchyard::BranchStatus::Ready
                        | branchyard::BranchStatus::NoChanges
                        | branchyard::BranchStatus::Merged { .. }
                )
            )
        {
            return Err(ApiError::new(StatusCode::CONFLICT,"deliverable_not_current","acceptance requires a completed producer with valid current-turn output and current-turn artifacts"));
        }
        let contract = attempt
            .result_contract
            .as_ref()
            .filter(|c| c.is_deliverable())
            .ok_or_else(|| {
                ApiError::bad_request("artifact producer needs a deliverable result contract")
            })?;
        contract
            .validate(
                attempt
                    .result
                    .as_ref()
                    .ok_or_else(|| ApiError::bad_request("missing result"))?,
            )
            .map_err(sdk)?;
        let row = new(
            Collection::Acceptance,
            &format!("{wid}:{aid}"),
            &caller,
            json!({"workspace_id":wid,"artifact_id":aid,"digest":artifact.digest,"producer":artifact.publisher_branch,"turn":artifact.turn,"result_event":attempt.result_event,"accepted_at_revision":workspace.revision,"accepted_by":caller.0.connector_subject(),"credential":caller.name(),"created_at_ms":branchyard_support::time::now_ms()}),
        );
        if app
            .registry
            .console_get(Collection::Acceptance, &row.id)?
            .is_none()
        {
            changes.push(insert(row));
        }
        accepted.push(aid.to_owned());
    }
    let response = json!({"workspace_id":wid,"revision":workspace.revision+1,"artifacts":accepted,"code_integration":"independent"});
    changes.push(update(workspace.clone(), workspace.body.clone()));
    changes.push(receipt(&caller, &key, &body, &response));
    match app.registry.console_mutate(&changes) {
        Ok(()) => Ok(Json(response)),
        Err(e) => match replay(&app, &caller, &key, &body)? {
            Some(old) => Ok(Json(old)),
            None => Err(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_bytes_above_text_limit_round_trip_and_verify_digest() {
        let original = vec![42; 100_000];
        let row = Record {
            collection: Collection::Sources,
            id: "source".into(),
            tenant: "tenant".into(),
            revision: 1,
            body: json!({"_bytes":STANDARD.encode(&original),"digest":hex::encode(Sha256::digest(&original))}),
        };
        assert_eq!(bytes(&row).unwrap(), original);
        let mut corrupted = row;
        corrupted.body["digest"] = json!("wrong");
        assert!(bytes(&corrupted).is_err());
    }
}

#[cfg(test)]
mod native_reader_tests {
    #[test]
    fn queued_attempts_do_not_hide_other_workspace_artifacts() {
        let scratch = branchyard_testkit::Scratch::new("console-empty-reader");
        let (_, yard) = branchyard::tasks::create(&branchyard::tasks::NewTask {
            prompt: "knowledge".into(),
            by: Some("test".into()),
            home: Some(scratch.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let repo = crate::api::RepoState {
            name: super::MANAGED_REPO.into(),
            feed: std::sync::Arc::new(crate::feed::Feed::open(yard.clone()).unwrap()),
            yard,
            wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        };
        assert!(super::native_artifacts(&repo, "queued-session")
            .unwrap()
            .is_empty());
    }
}
