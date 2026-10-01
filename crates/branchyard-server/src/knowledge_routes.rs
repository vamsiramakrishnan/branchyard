//! HTTP routes for [repository knowledge](../../../docs/knowledge.md) and
//! [plan approval](../../../docs/plans-and-goals.md), merged into the main
//! router with one line in [`crate::api::router`], like
//! [`crate::storage_routes`].
//!
//! Knowledge is a person's decision, made with the caller's authority:
//! reading needs the `read` scope, adding, adopting, rejecting, editing,
//! removing and distilling the `run` scope. The caller's name is who
//! adopted an entry. A server distills with the deterministic extractor
//! only; a harness distiller runs where `by knowledge distill --harness`
//! does.
//!
//! A plan's approval or re-planning rejection runs the branch's next turn,
//! so it is an operation, admitted and run like a send; a rejection that
//! ends the branch is one too, so both are uniform for clients.

use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};

use branchyard::{
    Distilled, KnowledgeEdit, KnowledgeEntry, KnowledgeStatus, NewKnowledge, PlanInfo, TaskKind,
};
use branchyard_client::api::OperationKind;
use branchyard_client::knowledge_api::{
    DistillRequest, KnowledgeAddRequest, KnowledgeDecisionRequest, KnowledgeEditRequest,
    KnowledgeExport, KnowledgeList, PlanApproveRequest, PlanRejectRequest,
};

use crate::api::{blocking, Caller, JsonBody, Shared};
use crate::error::{self, ApiError};
use crate::ops::NewOperation;
use crate::work::Work;

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route(
            "/v1/repos/{repo}/knowledge",
            get(list_knowledge).post(add_knowledge),
        )
        .route("/v1/repos/{repo}/knowledge/export", get(export_knowledge))
        .route(
            "/v1/repos/{repo}/knowledge/{id}",
            get(knowledge_entry).delete(remove_knowledge),
        )
        .route(
            "/v1/repos/{repo}/knowledge/{id}/adopt",
            post(adopt_knowledge),
        )
        .route(
            "/v1/repos/{repo}/knowledge/{id}/reject",
            post(reject_knowledge),
        )
        .route("/v1/repos/{repo}/knowledge/{id}/edit", post(edit_knowledge))
        .route("/v1/repos/{repo}/branches/{branch}/distill", post(distill))
        .route("/v1/repos/{repo}/branches/{branch}/plan", get(plan))
        .route(
            "/v1/repos/{repo}/branches/{branch}/plan/approve",
            post(approve_plan),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/plan/reject",
            post(reject_plan),
        )
}

fn entry_id(id: &str) -> Result<u64, ApiError> {
    id.trim_start_matches('k')
        .parse()
        .map_err(|_| ApiError::bad_request(format!("{id:?} is not a knowledge entry id")))
}

fn sdk<T>(result: Result<T, branchyard::Error>) -> Result<T, ApiError> {
    result.map_err(|e| error::sdk(&e))
}

async fn list_knowledge(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    RawQuery(query): RawQuery,
) -> Result<Json<KnowledgeList>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let mut status = None;
    for pair in query.as_deref().unwrap_or("").split('&') {
        if let Some(value) = pair.strip_prefix("status=") {
            status = Some(
                value
                    .parse::<KnowledgeStatus>()
                    .map_err(|e| ApiError::bad_request(e.to_string()))?,
            );
        }
    }
    let entries = sdk(blocking(move || yard.knowledge(status)).await?)?;
    Ok(Json(KnowledgeList { entries }))
}

async fn knowledge_entry(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<KnowledgeEntry>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let id = entry_id(&id)?;
    Ok(Json(
        sdk(blocking(move || yard.knowledge_entry(id)).await?)?,
    ))
}

async fn export_knowledge(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<KnowledgeExport>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let entries = sdk(blocking(move || yard.knowledge(Some(KnowledgeStatus::Adopted))).await?)?;
    Ok(Json(KnowledgeExport {
        markdown: branchyard::export_knowledge(&entries),
        entries: entries.iter().map(|e| e.id).collect(),
    }))
}

async fn add_knowledge(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<KnowledgeAddRequest>,
) -> Result<Json<KnowledgeEntry>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let by = caller.name().to_owned();
    let new = NewKnowledge {
        text: request.text,
        scope: request.scope,
        propose: request.propose,
        note: None,
    };
    Ok(Json(sdk(
        blocking(move || yard.add_knowledge(&new, &by)).await?
    )?))
}

async fn adopt_knowledge(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(_, _): JsonBody<KnowledgeDecisionRequest>,
) -> Result<Json<KnowledgeEntry>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let id = entry_id(&id)?;
    let by = caller.name().to_owned();
    Ok(Json(sdk(
        blocking(move || yard.adopt_knowledge(id, &by)).await?
    )?))
}

async fn reject_knowledge(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<KnowledgeDecisionRequest>,
) -> Result<Json<KnowledgeEntry>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let id = entry_id(&id)?;
    let by = caller.name().to_owned();
    Ok(Json(sdk(blocking(move || {
        yard.reject_knowledge(id, &by, request.reason.as_deref())
    })
    .await?)?))
}

async fn edit_knowledge(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<KnowledgeEditRequest>,
) -> Result<Json<KnowledgeEntry>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let id = entry_id(&id)?;
    let kind = match request.kind.as_deref() {
        None => None,
        Some("") => Some(None),
        Some(kind) => Some(Some(
            kind.parse::<TaskKind>()
                .map_err(|e| ApiError::bad_request(e.to_string()))?,
        )),
    };
    let change = KnowledgeEdit {
        text: request.text,
        path: request
            .path
            .map(|p| Some(p).filter(|p| !p.trim().is_empty())),
        kind,
    };
    let by = caller.name().to_owned();
    Ok(Json(sdk(blocking(move || {
        yard.edit_knowledge(id, &change, &by)
    })
    .await?)?))
}

async fn remove_knowledge(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<KnowledgeEntry>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let id = entry_id(&id)?;
    Ok(Json(sdk(
        blocking(move || yard.remove_knowledge(id)).await?
    )?))
}

async fn distill(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(_, _): JsonBody<DistillRequest>,
) -> Result<Json<Distilled>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    Ok(Json(sdk(
        blocking(move || yard.distill(&branch, None)).await?
    )?))
}

async fn plan(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<PlanInfo>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    Ok(Json(sdk(blocking(move || yard.plan(&branch)).await?)?))
}

/// Admit a plan decision on `branch` as an operation that locks it.
async fn admit_plan(
    app: Shared,
    caller: Caller,
    repo: String,
    branch: String,
    headers: HeaderMap,
    canonical: String,
    send: &branchyard_client::api::SendRequest,
    kind: OperationKind,
    work: Work,
) -> Result<Response, ApiError> {
    let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
    let policy = app.tenant_policy(&caller);
    let route = match kind {
        OperationKind::ApprovePlan => "approve",
        _ => "reject",
    };
    let route = format!(
        "POST /v1/repos/{}/branches/{branch}/plan/{route}",
        repo.name
    );
    let idem = crate::api::idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(response) = crate::api::replayed(&app, &caller, idem.as_ref()).await? {
        return Ok(response);
    }
    crate::work::plan_send_options(&app, &repo, send)?;
    // Refused now, not as a failed operation, when there is nothing to
    // decide.
    {
        let (yard, name) = (repo.yard.clone(), branch.clone());
        let info = sdk(blocking(move || yard.plan(&name)).await?)?;
        if info.phase != branchyard::PlanPhase::Awaiting {
            return Err(error::sdk(&branchyard::Error::NoPlan(format!(
                "{branch} has no plan awaiting approval"
            ))));
        }
    }
    let cursor = crate::api::sync_feed(&repo.feed).await?;
    let new = NewOperation {
        repo: repo.name.clone(),
        kind,
        creates: Vec::new(),
        branches: vec![branch.clone()],
        cursor,
        locks: vec![branch],
        idempotency: idem,
        principal: caller.0.clone(),
        quota: app.admission_quota(&caller, &policy),
        requires: crate::api::required_labels(&send.require_labels)?,
        priority: crate::api::admitted_priority(&policy, send.priority, 0)?,
        trace: crate::api::incoming_trace(&headers),
    };
    crate::api::admit(&app, new, work).await
}

async fn approve_plan(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(request, canonical): JsonBody<PlanApproveRequest>,
) -> Result<Response, ApiError> {
    let send = request.send.clone();
    let work = Work::ApprovePlan {
        branch: branch.clone(),
        request,
    };
    admit_plan(
        app,
        caller,
        repo,
        branch,
        headers,
        canonical,
        &send,
        OperationKind::ApprovePlan,
        work,
    )
    .await
}

async fn reject_plan(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(request, canonical): JsonBody<PlanRejectRequest>,
) -> Result<Response, ApiError> {
    let send = request.send.clone();
    let work = Work::RejectPlan {
        branch: branch.clone(),
        request,
    };
    admit_plan(
        app,
        caller,
        repo,
        branch,
        headers,
        canonical,
        &send,
        OperationKind::RejectPlan,
        work,
    )
    .await
}
