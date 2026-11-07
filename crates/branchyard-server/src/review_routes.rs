//! HTTP routes for reading a branch's diff file by file and reviewing it,
//! merged into the main router with one line in [`crate::api::router`],
//! like [`crate::checkpoint_routes`]. They are what the console's Changes
//! view reads and writes ([`branchyard::Changes`], [`branchyard::Reviews`]).
//!
//! `GET …/changes` and `GET …/reviews` need the `read` scope. `POST
//! …/reviews` needs `run`, as a send does: it writes the comments as one
//! prompt ([`branchyard::review_prompt`]) and delivers it as the branch's
//! next turn (a send operation) or into the running turn (a steer), then
//! records the comments on the branch's log as `by review` records its own,
//! so a later `GET …/reviews` returns them.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Extension, Json, Router};

use branchyard::{
    check_review, review_prompt, Changes, Feedback, FeedbackItem, FeedbackSource, Reviews,
    SteerState,
};
use branchyard_client::api::{ReviewDelivery, ReviewRequest, ReviewSent, SendRequest};

use crate::api::{
    admit_send, blocking, existing, idempotency, incoming_trace, Caller, JsonBody, SendTo, Shared,
};
use crate::error::{self, ApiError};
use crate::store::Lane;

/// How long a steered review waits to leave `pending`, as a steer does.
const STEER_WAIT: Duration = Duration::from_secs(10);

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/repos/{repo}/branches/{branch}/changes", get(changes))
        .route(
            "/v1/repos/{repo}/branches/{branch}/reviews",
            get(reviews).post(post_review),
        )
}

/// The branch's candidate diff against its base, file by file.
async fn changes(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Changes>, ApiError> {
    let target = existing(&app.authorized_repo(&caller, &repo, "read")?.yard, &branch).await?;
    let changes = blocking(move || target.changes())
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(changes))
}

/// The reviews sent to the branch, with the comments a later candidate
/// addressed marked.
async fn reviews(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Reviews>, ApiError> {
    let target = existing(&app.authorized_repo(&caller, &repo, "read")?.yard, &branch).await?;
    let reviews = blocking(move || target.reviews())
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(reviews))
}

/// The feedback record of a review: the comments as data, and the note as
/// an item with no file.
fn feedback(request: &ReviewRequest, commit: Option<String>) -> Feedback {
    let mut items: Vec<FeedbackItem> = request
        .comments
        .iter()
        .map(|c| FeedbackItem {
            path: Some(c.path.clone()),
            line: Some(c.line),
            start_line: c.start_line.filter(|&s| s != c.line),
            side: Some(c.side),
            body: c.body.trim().to_owned(),
            ..FeedbackItem::default()
        })
        .collect();
    if let Some(note) = request
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        items.push(FeedbackItem {
            body: note.to_owned(),
            ..FeedbackItem::default()
        });
    }
    Feedback {
        source: FeedbackSource::Review,
        reference: commit,
        items,
        prompt: None,
    }
}

/// Send a review to the branch.
async fn post_review(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(request, canonical): JsonBody<ReviewRequest>,
) -> Result<Json<ReviewSent>, ApiError> {
    let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
    let route = format!("POST /v1/repos/{}/branches/{branch}/reviews", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    let target = existing(&repo.yard, &branch).await?;
    // The comments are placed on the diff as it is now, and refused when
    // the reviewer read another candidate.
    let (prompt, commit) = {
        let (target, request) = (target.clone(), request.clone());
        blocking(move || -> Result<(String, Option<String>), ApiError> {
            let changes = target.changes().map_err(|e| error::sdk(&e))?;
            if let Some(seen) = &request.commit {
                if changes.commit.as_ref() != Some(seen) {
                    let now = match &changes.commit {
                        Some(now) => format!("is {now} now"),
                        None => "is gone".to_owned(),
                    };
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "stale_candidate",
                        format!(
                            "the branch's candidate {now}, not {seen}; reload the changes and \
                             comment again"
                        ),
                    ));
                }
            }
            check_review(&changes, &request.comments, request.note.as_deref())
                .map_err(|e| error::sdk(&e))?;
            let prompt = review_prompt(&changes, &request.comments, request.note.as_deref());
            Ok((prompt, changes.commit))
        })
        .await??
    };
    let record = feedback(&request, commit);
    let comments = u32::try_from(request.comments.len()).unwrap_or(u32::MAX);
    match request.delivery {
        ReviewDelivery::NextTurn => {
            let to = SendTo {
                branch: branch.clone(),
                request: SendRequest {
                    prompt: prompt.clone(),
                    ..SendRequest::default()
                },
                reincarnate: None,
            };
            let (op, replayed) = admit_send(
                &app,
                &repo,
                &caller,
                idem,
                to,
                incoming_trace(&headers),
                Lane::FollowUp,
            )
            .await?;
            if !replayed {
                let recorded = prompt.clone();
                blocking(move || target.record_feedback(&recorded, record))
                    .await?
                    .map_err(|e| error::sdk(&e))?;
            }
            Ok(Json(ReviewSent {
                delivery: ReviewDelivery::NextTurn,
                comments,
                prompt,
                operation: Some(op.id),
                steer: None,
            }))
        }
        ReviewDelivery::Steer => {
            let yard = repo.yard.clone();
            let by = caller.acting();
            let (text, name) = (prompt.clone(), branch.clone());
            let steer = blocking(move || {
                let steer = yard.steer_as(&name, &text, &by)?;
                yard.wait_steer(&name, steer.id, STEER_WAIT)
            })
            .await?
            .map_err(|e| error::sdk(&e))?;
            if let SteerState::Refused { reason } = &steer.state {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "steer_refused",
                    format!("the running turn did not take the review: {reason}"),
                ));
            }
            let recorded = prompt.clone();
            blocking(move || target.record_feedback(&recorded, record))
                .await?
                .map_err(|e| error::sdk(&e))?;
            Ok(Json(ReviewSent {
                delivery: ReviewDelivery::Steer,
                comments,
                prompt,
                operation: None,
                steer: Some(steer),
            }))
        }
    }
}
