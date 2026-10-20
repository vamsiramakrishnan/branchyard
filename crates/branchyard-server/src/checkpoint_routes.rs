//! HTTP routes for [checkpoints](../../../docs/checkpoints.md), merged into
//! the main router with one line in [`crate::api::router`], like
//! [`crate::effects_routes`].
//!
//! A rewind is quick (a reset to a recorded commit), so it is not an
//! operation: like a discard, it holds its branch in the operation store
//! while it runs, so no operation on any server sharing the store starts a
//! turn of the branch meanwhile, and answers with what it did. It needs the
//! `run` scope. A fork at a checkpoint is a fork operation
//! (`POST …/fork` with `at`), whose checkpoint [`checkpoint_refusal`]
//! checks at admission.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Extension, Json, Router};

use branchyard::{Branch, Rewound};
use branchyard_client::api::RewindRequest;

use crate::api::{blocking, Caller, JsonBody, Shared};
use crate::error::{self, ApiError};

pub(crate) fn router() -> Router<Shared> {
    Router::new().route(
        "/v1/repos/{repo}/branches/{branch}/rewind",
        post(post_rewind),
    )
}

/// `error`, from a rewind or fork of `branch` at checkpoint `turn`, as the
/// server answers it: `404 unknown_checkpoint` for a checkpoint the branch
/// does not have, `409 checkpoint_unavailable` for one whose ref no longer
/// names its commit, else the SDK's own kind. The message is the SDK's.
pub(crate) fn checkpoint_refusal(
    branch: &Branch,
    turn: u32,
    error: &branchyard::Error,
) -> ApiError {
    let sdk = error::sdk(error);
    if turn == 0 || !matches!(error, branchyard::Error::State(_)) {
        return sdk;
    }
    let Ok(list) = branch.checkpoints() else {
        return sdk;
    };
    match list.checkpoints.iter().find(|e| e.checkpoint.turn == turn) {
        None => ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_checkpoint",
            error.to_string(),
        ),
        Some(entry) if !entry.available => ApiError::new(
            StatusCode::CONFLICT,
            "checkpoint_unavailable",
            error.to_string(),
        ),
        Some(_) => sdk,
    }
}

/// Reset settled `branch`, its worktree and its candidate to checkpoint
/// `to`, like `by rewind --to N`; the answer is the SDK's [`Rewound`].
/// Refused as the SDK refuses it (`409 running` while a turn runs,
/// `403 denied` for a merged branch or over worktree changes no checkpoint
/// has), `404 unknown_checkpoint` for a checkpoint it does not have, and
/// `409 branch_busy` while an operation holds it.
async fn post_rewind(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(RewindRequest { to }, _): JsonBody<RewindRequest>,
) -> Result<Json<Rewound>, ApiError> {
    let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
    let registry = app.registry.clone();
    blocking(move || {
        let hold = registry.hold(&repo.name, &branch, "a rewind")?;
        let rewound = match repo.yard.branch(&branch) {
            Ok(target) => target
                .rewind(to)
                .map_err(|e| checkpoint_refusal(&target, to, &e)),
            Err(e) => Err(error::sdk(&e)),
        };
        drop(hold);
        rewound
    })
    .await?
    .map(Json)
}
