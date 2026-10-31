//! Project, workspace and native conversation HTTP endpoints.
use super::*;
use crate::api::{Caller, Shared};
use crate::console_store::Collection;
use crate::error::ApiError;
use axum::{
    extract::{Path, RawQuery, State},
    http::HeaderMap,
    routing::{get, post},
    Extension, Json, Router,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/projects", get(projects).post(create_project))
        .route("/v1/projects/{project}", get(project))
        .route("/v1/workspaces", get(workspaces).post(create_workspace))
        .route("/v1/workspaces/{workspace}", get(workspace))
        .route(
            "/v1/workspaces/{workspace}/sessions",
            get(sessions).post(start_session),
        )
        .route(
            "/v1/workspaces/{workspace}/sessions/{session}/messages",
            get(messages).post(send),
        )
        .route(
            "/v1/workspaces/{workspace}/sessions/{session}/cancel",
            post(cancel),
        )
        .route("/v1/workspaces/{workspace}/actions/{action}", post(action))
        .route("/v1/execution-presets", get(execution_presets))
        .route(
            "/v1/workspaces/{workspace}/finalize",
            post(super::sources::finalize),
        )
        .merge(super::sources::router())
        .merge(super::previews::router())
        .merge(super::collaboration::router())
}
fn project_access(app: &App, caller: &Caller, p: &Record, required: &str) -> Result<(), ApiError> {
    scope(caller, required)?;
    if p.tenant != caller.tenant() {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "not_found",
            "no such project",
        ));
    }
    if p.body["source"]["kind"] == "repository" {
        app.authorized_repo(caller, text(&p.body["source"], "repo")?, required)?;
    } else {
        knowledge_allowed(app, caller)?;
    }
    Ok(())
}
/// Require enabled knowledge storage and both principal and tenant source grants.
pub(crate) fn knowledge_allowed(app: &App, caller: &Caller) -> Result<(), ApiError> {
    if !app.config.managed_knowledge.enabled {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "knowledge_disabled",
            "the operator must enable managed_knowledge",
        ));
    }
    let policy = app.config.tenant_policy(caller.tenant());
    if !caller.0.repo_allowed(&policy, MANAGED_REPO) {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "repo_not_allowed",
            "knowledge storage needs an explicit managed-source grant for restricted callers",
        ));
    }
    Ok(())
}
async fn projects(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, ApiError> {
    scope(&caller, "read")?;
    let rows = app
        .registry
        .console_list(Collection::Projects, caller.tenant())?;
    Ok(Json(
        json!({"projects":rows.iter().filter(|r|project_access(&app,&caller,r,"read").is_ok()).map(public).collect::<Vec<_>>()}),
    ))
}
async fn project(
    State(app): State<Shared>,
    Path(key): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, ApiError> {
    let p = super::get(&app, Collection::Projects, &key)?;
    project_access(&app, &caller, &p, "read")?;
    Ok(Json(public(&p)))
}
async fn create_project(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    crate::api::JsonBody(request, _): crate::api::JsonBody<
        branchyard_client::workspace_api::CreateProjectRequest,
    >,
) -> Result<Json<Value>, ApiError> {
    let body =
        serde_json::to_value(request).map_err(|error| ApiError::internal(error.to_string()))?;
    scope(&caller, "run")?;
    let key = idem_key(&caller, &headers, "projects")?;
    let name = text(&body, "name")?;
    let source = &body["source"];
    match source["kind"].as_str() {
        Some("knowledge") => {
            knowledge_allowed(&app, &caller)?;
            if source.as_object().is_none_or(|o| o.len() != 1) {
                return Err(ApiError::bad_request("knowledge source accepts only kind"));
            }
        }
        Some("repository") => {
            app.authorized_repo(&caller, text(source, "repo")?, "run")?;
        }
        _ => {
            return Err(ApiError::bad_request(
                "source.kind must be knowledge or repository",
            ))
        }
    }
    if let Some(old) = replay(&app, &caller, &key, &body)? {
        return Ok(Json(old));
    }
    if !matches!(
        body["result_mode"].as_str(),
        Some("code" | "artifacts" | "mixed")
    ) {
        return Err(ApiError::bad_request(
            "result_mode must be code, artifacts or mixed",
        ));
    }
    let key_id = id()?;
    let row = new(
        Collection::Projects,
        &key_id,
        &caller,
        json!({"id":key_id,"name":name,"source":source,"result_mode":body["result_mode"],"created_at_ms":branchyard_support::time::now_ms()}),
    );
    let response = public(&row);
    let mutations = [insert(row), receipt(&caller, &key, &body, &response)];
    match app.registry.console_mutate(&mutations) {
        Ok(()) => Ok(Json(response)),
        Err(e) => match replay(&app, &caller, &key, &body)? {
            Some(old) => Ok(Json(old)),
            None => Err(e),
        },
    }
}
async fn workspaces(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let query = query(raw_query);
    scope(&caller, "read")?;
    let rows = app
        .registry
        .console_list(Collection::Workspaces, caller.tenant())?;
    let mut views = Vec::new();
    for row in rows {
        if query
            .get("project")
            .is_some_and(|p| row.body["project_id"] != *p)
        {
            continue;
        }
        match auth::authorize_workspace(&app, &caller, &row.id, "read") {
            Ok(_) => {
                let sessions = super::runtime::session_views(&app, &caller, &row)?;
                views.push(super::runtime::workspace_view(&app, &row, &sessions)?);
            }
            Err(error) if error.status.is_server_error() => return Err(error),
            Err(_) => {}
        }
    }
    Ok(Json(json!({"workspaces":views})))
}
// The counter CAS participates in the exact allocation/queue transaction. Even
// requests for different projects therefore cannot consume the last slot twice.
fn workspace_quota(app: &App, caller: &Caller) -> Result<Mutation, ApiError> {
    let key = format!("quota:{}", caller.tenant());
    let current = app.registry.console_get(Collection::Idempotency, &key)?;
    let count = match &current {
        Some(record) => {
            if record.tenant != caller.tenant() {
                return Err(ApiError::internal("workspace quota tenant mismatch"));
            }
            usize::try_from(
                record
                    .body
                    .get("workspaces")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ApiError::internal("invalid workspace quota counter"))?,
            )
            .map_err(|e| ApiError::internal(e.to_string()))?
        }
        None => app
            .registry
            .console_list(Collection::Workspaces, caller.tenant())?
            .len(),
    };
    if count >= app.config.managed_knowledge.max_workspaces {
        return Err(ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            "workspace allocation quota reached",
        ));
    }
    let next = count
        .checked_add(1)
        .ok_or_else(|| ApiError::internal("workspace quota counter exhausted"))?;
    let mut body = current
        .as_ref()
        .map_or_else(|| json!({}), |record| record.body.clone());
    body.as_object_mut()
        .ok_or_else(|| ApiError::internal("invalid workspace quota counter"))?
        .insert("workspaces".into(), json!(next));
    Ok(match current {
        Some(record) => update(record, body),
        None => insert(new(Collection::Idempotency, &key, caller, body)),
    })
}
async fn create_workspace(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    crate::api::JsonBody(request, _): crate::api::JsonBody<
        branchyard_client::workspace_api::CreateWorkspaceRequest,
    >,
) -> Result<Json<Value>, ApiError> {
    let body =
        serde_json::to_value(request).map_err(|error| ApiError::internal(error.to_string()))?;
    let project = super::get(&app, Collection::Projects, text(&body, "project_id")?)?;
    project_access(&app, &caller, &project, "run")?;
    let canonical = serde_json::to_string(&body).map_err(|e| ApiError::internal(e.to_string()))?;
    let route = "/v1/workspaces";
    let idem = crate::api::idempotency(&headers, &caller, route, &canonical)?;
    if idem.is_none() {
        return Err(ApiError::bad_request("Idempotency-Key is required"));
    }
    if let Some(old) = super::runtime::replayed(&app, &caller, idem.as_ref())? {
        return Ok(Json(old));
    }
    let quota = workspace_quota(&app, &caller)?;
    let wid = id()?;
    let title = text(&body, "title")?;
    let repo = if project.body["source"]["kind"] == "knowledge" {
        MANAGED_REPO
    } else {
        text(&project.body["source"], "repo")?
    };
    let allocation = if repo == MANAGED_REPO {
        let reservation = branchyard::tasks::reserve(&branchyard::tasks::NewTask {
            prompt: title.into(),
            by: Some(caller.0.connector_subject()),
            home: Some(super::runtime::managed_root(&app)?),
            ..Default::default()
        })
        .map_err(sdk)?;
        json!({"kind":"knowledge","task_id":reservation.id(),"reservation":reservation})
    } else {
        let task = branchyard::tasks::reserve_repository(title, &caller.0.connector_subject(), "")
            .map_err(sdk)?;
        json!({"kind":"repository","task_id":task.id,"task":task})
    };
    let row = new(
        Collection::Workspaces,
        &wid,
        &caller,
        json!({"id":wid,"project_id":project.id,"title":title,"state":"pending","repo":repo,"task_id":allocation["task_id"],"result_mode":project.body["result_mode"],"created_at_ms":branchyard_support::time::now_ms(),"_allocation":allocation}),
    );
    let member = new(
        Collection::Members,
        &format!("{}:{}", wid, caller.0.connector_subject()),
        &caller,
        json!({"workspace_id":wid,"subject":caller.0.connector_subject(),"role":"owner"}),
    );
    let mutations = vec![
        quota,
        insert(row),
        insert(member),
        binding(
            &caller,
            repo,
            &format!(
                "task:{}",
                allocation["task_id"].as_str().unwrap_or_default()
            ),
            &wid,
        ),
    ];
    super::runtime::admit(
        &app,
        &caller,
        &headers,
        repo,
        ConsoleWork::Initialize { workspace_id: wid },
        idem,
        mutations,
    )
    .map(Json)
}
async fn workspace(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, ApiError> {
    let row = auth::authorize_workspace(&app, &caller, &wid, "read")?;
    let sessions = super::runtime::session_views(&app, &caller, &row)?;
    let mut body = super::runtime::workspace_view(&app, &row, &sessions)?;
    let subject = caller.0.connector_subject();
    let member = super::get(&app, Collection::Members, &format!("{wid}:{subject}"))?;
    body["current_subject"] = json!(subject);
    body["current_role"] = member.body["role"].clone();
    body["sessions"] = json!(sessions);
    for (key, c) in [
        ("members", Collection::Members),
        ("sources", Collection::Sources),
        ("accepted_deliverables", Collection::Acceptance),
    ] {
        body[key] = json!(related(&app, c, &caller, &wid)?
            .iter()
            .map(public)
            .collect::<Vec<_>>());
    }
    body["capabilities"] = json!({"isolated_writers":true,"source_upload":true,"artifact_acceptance":true,"terminal":{"available":false,"reason":"runtime_owner_required"},"sleep":{"available":false,"reason":"provider_capability_required"}});
    body["capabilities"]["max_upload_bytes"] = json!(app.config.managed_knowledge.max_upload_bytes);
    body["capabilities"]["max_source_bytes"] = json!(app.config.managed_knowledge.max_source_bytes);
    body["capabilities"]["can_admin"] =
        json!(caller.0.allows("admin") && member.body["role"] == "owner");
    body["capabilities"]["can_run"] = json!(
        caller.0.allows("run") && matches!(member.body["role"].as_str(), Some("owner" | "editor"))
    );
    body["capabilities"]["can_comment"] = json!(
        caller.0.allows("run")
            && matches!(
                member.body["role"].as_str(),
                Some("owner" | "editor" | "reviewer")
            )
    );
    Ok(Json(body))
}
async fn sessions(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, ApiError> {
    let row = auth::authorize_workspace(&app, &caller, &wid, "read")?;
    Ok(Json(
        json!({"sessions":super::runtime::session_views(&app,&caller,&row)?}),
    ))
}
async fn start_session(
    State(app): State<Shared>,
    Path(wid): Path<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    crate::api::JsonBody(request, _): crate::api::JsonBody<
        branchyard_client::workspace_api::StartSessionRequest,
    >,
) -> Result<Json<Value>, ApiError> {
    let body =
        serde_json::to_value(request).map_err(|error| ApiError::internal(error.to_string()))?;
    super::runtime::start(&app, &caller, &wid, &headers, body).map(Json)
}
async fn execution_presets(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<super::presets::PresetList>, ApiError> {
    let workspace = query(raw_query).remove("workspace");
    crate::api::blocking(move || super::presets::discover(&app, &caller, workspace.as_deref()))
        .await?
        .map(Json)
}
async fn messages(
    State(app): State<Shared>,
    Path((wid, sid)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let query = query(raw_query);
    let workspace = auth::authorize_workspace(&app, &caller, &wid, "read")?;
    let session = super::runtime::session(&app, &caller, &wid, &sid)?;
    let repo = super::resolve_source(&app, &workspace)?;
    let branch = repo
        .yard
        .branch(text(&session.body, "branch")?)
        .map_err(sdk)?;
    let cursor = query
        .get("after")
        .map(|s| {
            s.parse::<usize>()
                .map_err(|_| ApiError::bad_request("invalid message cursor"))
        })
        .transpose()?
        .unwrap_or(0);
    let events = branch.events().map_err(sdk)?;
    if cursor > events.len() {
        return Err(ApiError::bad_request(
            "message cursor is past the native event log",
        ));
    }
    let contract = branch.result_contract().map_err(sdk)?;
    let first = super::harness_input::Additions::first(&wid, &session, contract.as_ref());
    let later = super::harness_input::Additions::later(contract.as_ref());
    let start = events
        .iter()
        .position(|event| matches!(event.activity, branchyard::Activity::Prompt(_)));
    let mut next = cursor;
    let mut messages = Vec::new();
    for (index, event) in events.into_iter().enumerate().skip(cursor).take(1000) {
        next = index + 1;
        let (role, text) = match event.activity {
            branchyard::Activity::Prompt(t) => {
                let additions = if start == Some(index) { &first } else { &later };
                ("user", additions.typed(&t).to_owned())
            }
            branchyard::Activity::Harness(branchyard::Event::MessageDelta { text, .. }) => {
                ("assistant", text)
            }
            branchyard::Activity::Steered { text, .. } => ("user", text),
            _ => continue,
        };
        messages.push(json!({"id":format!("{sid}:{index}"),"role":role,"text":text,"time":event.at_ms,"at_ms":event.at_ms}));
    }
    let mut response = json!({"messages":messages,"next_cursor":next.to_string()});
    if let Some(preset) = super::presets::stored(&session)? {
        super::presets::project_value(&mut response["messages"], &preset.hidden);
    }
    if workspace.body["repo"] == MANAGED_REPO {
        auth::redact_paths(
            &mut response,
            &super::runtime::managed_root(&app)?.to_string_lossy(),
        );
    }
    Ok(Json(response))
}
async fn send(
    State(app): State<Shared>,
    Path((wid, sid)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    crate::api::JsonBody(request, _): crate::api::JsonBody<
        branchyard_client::workspace_api::MessageRequest,
    >,
) -> Result<Json<Value>, ApiError> {
    let body =
        serde_json::to_value(request).map_err(|error| ApiError::internal(error.to_string()))?;
    super::runtime::continue_session(&app, &caller, &wid, &sid, &headers, body).map(Json)
}
async fn cancel(
    State(app): State<Shared>,
    Path((wid, sid)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    super::runtime::control(&app, &caller, &wid, &sid, &headers, "cancel", json!({})).map(Json)
}
async fn action(
    State(app): State<Shared>,
    Path((wid, action)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    crate::api::JsonBody(body, _): crate::api::JsonBody<Value>,
) -> Result<Json<Value>, ApiError> {
    super::runtime::lifecycle(&app, &caller, &wid, &headers, &action, body).map(Json)
}

fn query(raw: Option<String>) -> BTreeMap<String, String> {
    raw.as_deref()
        .unwrap_or("")
        .split('&')
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| {
            (
                branchyard_client::http::decode_form(key),
                branchyard_client::http::decode_form(value),
            )
        })
        .collect()
}
