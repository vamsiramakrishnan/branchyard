//! HTTP routes for [tasks](../../../docs/task-repos.md): a repository's
//! tasks and their attempts, read from the yard's records. Reading needs
//! the `read` scope. The routes are `task-records`, since `POST .../tasks`
//! already starts a branch (and with it a task); accepting an attempt is a
//! merge (`POST .../branches/{branch}/merge`).

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Extension, Json, Router};

use branchyard::tasks::{self, TaskView};
use branchyard_client::api::TaskList;

use crate::api::{blocking, Caller, Shared};
use crate::error::{self, ApiError};

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/repos/{repo}/task-records", get(list_tasks))
        .route("/v1/repos/{repo}/task-records/{task}", get(task))
}

fn sdk<T>(result: Result<T, branchyard::Error>) -> Result<T, ApiError> {
    result.map_err(|e| error::sdk(&e))
}

/// `GET /v1/repos/{repo}/task-records`.
async fn list_tasks(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<TaskList>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let tasks = sdk(blocking(move || tasks::list(&yard)).await?)?;
    Ok(Json(TaskList { tasks }))
}

/// `GET /v1/repos/{repo}/task-records/{task}`: by ID, a prefix of one, or
/// an attempt's name.
async fn task(
    State(app): State<Shared>,
    Path((repo, key)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<TaskView>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    Ok(Json(
        sdk(blocking(move || tasks::view(&yard, &key)).await?)?,
    ))
}
