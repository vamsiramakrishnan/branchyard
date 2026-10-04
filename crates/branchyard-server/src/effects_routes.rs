//! HTTP routes for [approvals and the effect ledger](../../../docs/effects.md),
//! merged into the main router with one line in [`crate::api::router`],
//! like [`crate::knowledge_routes`].
//!
//! Reading the ledger and the waiting asks needs the `read` scope;
//! answering an ask, promoting a staged effect, reconciling and undoing
//! need `run`, and are recorded as the caller's. None locks a branch: an
//! answer is one store write the waiting turn sees, and a promotion or an
//! inverse is one gateway call recorded on its entry.

use axum::extract::{Path, RawQuery, State};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};

use branchyard::effects::reconcile::Reconciled;
use branchyard::effects::undo::UndoPlan;
use branchyard::effects::{ApprovalAsk, EffectEntry};
use branchyard_client::effects_api::{
    ApprovalAnswerRequest, ApprovalList, EffectActionRequest, EffectDetail, EffectList, UndoReport,
    UndoRequest,
};

use crate::api::{blocking, Caller, JsonBody, Shared};
use crate::error::{self, ApiError};

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/repos/{repo}/approvals", get(approvals))
        .route("/v1/repos/{repo}/approvals/{id}/allow", post(allow))
        .route("/v1/repos/{repo}/approvals/{id}/deny", post(deny))
        .route("/v1/repos/{repo}/effects", get(effects))
        .route("/v1/repos/{repo}/effects/reconcile", post(reconcile))
        .route("/v1/repos/{repo}/effects/{id}", get(effect))
        .route("/v1/repos/{repo}/effects/{id}/promote", post(promote))
        .route(
            "/v1/repos/{repo}/branches/{branch}/undo",
            get(undo_plan).post(undo),
        )
}

fn sdk<T>(result: Result<T, branchyard::Error>) -> Result<T, ApiError> {
    result.map_err(|e| error::sdk(&e))
}

/// `key=value` pairs of a query.
fn query(raw: Option<&str>, key: &str) -> Option<String> {
    raw.unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
        .map(branchyard_client::http::decode_form)
}

async fn approvals(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    RawQuery(raw): RawQuery,
) -> Result<Json<ApprovalList>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let all = query(raw.as_deref(), "all").is_some_and(|v| v == "true" || v == "1");
    let approvals = sdk(blocking(move || yard.approvals(!all)).await?)?;
    Ok(Json(ApprovalList { approvals }))
}

/// The surface an answer came through: `api`, unless the request says the
/// companion page or `by watch` sent it.
fn surface(request: &ApprovalAnswerRequest) -> Result<String, ApiError> {
    match request.surface.as_deref() {
        None => Ok("api".into()),
        Some(s @ ("api" | "companion" | "watch")) => Ok(s.to_owned()),
        Some(other) => Err(ApiError::bad_request(format!(
            "{other:?} is not a surface; use api, companion or watch"
        ))),
    }
}

async fn answer(
    app: Shared,
    repo: String,
    id: String,
    caller: Caller,
    request: ApprovalAnswerRequest,
    allow: bool,
) -> Result<Json<ApprovalAsk>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let surface = surface(&request)?;
    let by = caller.name().to_owned();
    Ok(Json(sdk(blocking(move || {
        yard.answer_approval(&id, allow, &by, &surface, request.reason.as_deref())
    })
    .await?)?))
}

async fn allow(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<ApprovalAnswerRequest>,
) -> Result<Json<ApprovalAsk>, ApiError> {
    answer(app, repo, id, caller, request, true).await
}

async fn deny(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<ApprovalAnswerRequest>,
) -> Result<Json<ApprovalAsk>, ApiError> {
    answer(app, repo, id, caller, request, false).await
}

async fn effects(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    RawQuery(raw): RawQuery,
) -> Result<Json<EffectList>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let branch = query(raw.as_deref(), "branch");
    let effects = sdk(blocking(move || yard.effects(branch.as_deref())).await?)?;
    Ok(Json(EffectList { effects }))
}

async fn effect(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<EffectDetail>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let detail = sdk(blocking(move || {
        let entry = yard.effect(&id)?;
        let events = yard.effect_history(&entry.id)?;
        Ok(EffectDetail { entry, events })
    })
    .await?)?;
    Ok(Json(detail))
}

async fn promote(
    State(app): State<Shared>,
    Path((repo, id)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(_, _): JsonBody<EffectActionRequest>,
) -> Result<Json<EffectEntry>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let by = caller.name().to_owned();
    Ok(Json(sdk(blocking(move || {
        yard.promote_effect(&id, &by, "api")
    })
    .await?)?))
}

async fn reconcile(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    JsonBody(_, _): JsonBody<EffectActionRequest>,
) -> Result<Json<Reconciled>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    Ok(Json(
        sdk(blocking(move || yard.reconcile_effects()).await?)?,
    ))
}

async fn undo_plan(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    RawQuery(raw): RawQuery,
) -> Result<Json<UndoPlan>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let to = match query(raw.as_deref(), "to") {
        Some(to) => to
            .parse()
            .map_err(|_| ApiError::bad_request(format!("to={to} is not a turn")))?,
        None => 0,
    };
    Ok(Json(sdk(
        blocking(move || yard.undo_plan(&branch, to)).await?
    )?))
}

async fn undo(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<UndoRequest>,
) -> Result<Json<UndoReport>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let by = caller.name().to_owned();
    let report = sdk(blocking(move || {
        let plan = yard.undo_plan(&branch, request.to)?;
        let chosen = match request.only.is_empty() {
            true => plan.default_choice(),
            false => request.only.clone(),
        };
        let outcomes = yard.undo_effects(&plan, &chosen, &by, "api")?;
        Ok(UndoReport { plan, outcomes })
    })
    .await?)?;
    Ok(Json(report))
}

#[cfg(test)]
mod tests {
    use super::query;

    #[test]
    fn query_values_decode_like_every_other_call_site() {
        assert_eq!(
            query(Some("a=1&k=x+y%2Bz%41"), "k").as_deref(),
            Some("x y+zA")
        );
        assert_eq!(query(Some("k=%zz%4"), "k").as_deref(), Some("%zz%4"));
        assert_eq!(query(None, "k"), None);
    }
}
