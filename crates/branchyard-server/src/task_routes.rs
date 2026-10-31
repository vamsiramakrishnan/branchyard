//! HTTP routes for [tasks](../../../docs/task-repos.md): a repository's
//! tasks and their attempts, read from the yard's records. Reading needs
//! the `read` scope. The routes are `task-records`, since `POST .../tasks`
//! already starts a branch (and with it a task); accepting an attempt is a
//! merge (`POST .../branches/{branch}/merge`).

use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};

use branchyard::tasks;
use branchyard_client::api::TaskList;

use crate::api::{blocking, branch_response, items, Caller, Shared};
use crate::error::{self, ApiError};

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/repos/{repo}/task-records", get(list_tasks))
        .route("/v1/repos/{repo}/task-records/{task}", get(task))
}

fn sdk<T>(result: Result<T, branchyard::Error>) -> Result<T, ApiError> {
    result.map_err(|e| error::sdk(&e))
}

/// `GET /v1/repos/{repo}/task-records`. Each attempt shows through the
/// projection of the preset session its branch belongs to.
async fn list_tasks(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Response, ApiError> {
    let state = app.authorized_repo(&caller, &repo, "read")?;
    let (yard, repo) = (state.yard.clone(), state.name.clone());
    blocking(move || {
        let tasks = sdk(tasks::list(&yard))?;
        let owners = tasks
            .iter()
            .enumerate()
            .flat_map(|(i, task)| {
                let names = task.attempts.iter().map(|attempt| attempt.name.as_str());
                items(&format!("/tasks/{i}/attempts"), names)
            })
            .collect();
        branch_response(&app, &repo, TaskList { tasks }, owners)
    })
    .await?
}

/// `GET /v1/repos/{repo}/task-records/{task}`: by ID, a prefix of one, or
/// an attempt's name, with each attempt as [`list_tasks`] shows it.
async fn task(
    State(app): State<Shared>,
    Path((repo, key)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Response, ApiError> {
    let state = app.authorized_repo(&caller, &repo, "read")?;
    let (yard, repo) = (state.yard.clone(), state.name.clone());
    blocking(move || {
        let task = sdk(tasks::view(&yard, &key))?;
        let owners = items("/attempts", task.attempts.iter().map(|a| a.name.as_str()));
        branch_response(&app, &repo, task, owners)
    })
    .await?
}
