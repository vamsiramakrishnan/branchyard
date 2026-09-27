//! Routes and handlers. See `docs/server.md` for the reference.
//!
//! Handlers never run the engine on a request's task: reads go to the
//! blocking pool, and changes become operations in the registry.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{FromRequest, Path, RawQuery, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use branchyard::{BranchEvent, Observer, TaskOptions, Yard};
use branchyard_client::api::{
    BranchEvents, BranchList, Diff, ErrorBody, FeedEntry, ForkRequest, HarnessEntry, HarnessList,
    MergeRequest, MergedInfo, Operation, OperationKind, OperationResult, Removed, RepoEntry,
    RepoList, SendRequest, TaskRequest,
};
use futures_util::stream::{self, Stream, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::{watch, Notify};

use crate::auth::Tokens;
use crate::config::Config;
use crate::error::{self, ApiError};
use crate::feed::Feed;
use crate::ops::{Finished, Job, NewOperation, Registry};
use crate::store::Idempotency;

/// Requests handled at once; more wait.
const MAX_CONCURRENT_REQUESTS: usize = 256;
/// Feed entries read per batch while streaming.
const STREAM_BATCH: usize = 256;

pub struct App {
    pub repos: BTreeMap<String, RepoState>,
    pub registry: Arc<Registry>,
    pub tokens: Tokens,
    pub config: Config,
    pub shutdown: watch::Receiver<bool>,
}

#[derive(Clone)]
pub struct RepoState {
    pub name: String,
    pub yard: Yard,
    pub feed: Arc<Feed>,
    /// Wakes the feed's ingestion when the engine records activity.
    pub wake: Arc<Notify>,
}

type Shared = Arc<App>;

/// The authenticated caller: its token's configured name.
#[derive(Clone)]
pub struct Caller(pub String);

impl App {
    fn repo(&self, name: &str) -> Result<&RepoState, ApiError> {
        self.repos.get(name).ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown_repo",
                format!("no repository named {name}"),
            )
        })
    }

    /// The command for a new branch: the request's own when allowed, else
    /// the configured one for its harness, else the profile's.
    fn command(
        &self,
        requested: Option<Vec<String>>,
        harnesses: &[Option<&str>],
    ) -> Result<Option<Vec<String>>, ApiError> {
        if let Some(command) = requested {
            if !self.config.allow_client_commands {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "command_not_allowed",
                    "this server does not accept a request's own command; its operator can \
                     configure harness_commands or allow_client_commands",
                ));
            }
            if command.is_empty() || command[0].is_empty() {
                return Err(ApiError::bad_request("command needs an executable"));
            }
            return Ok(Some(command));
        }
        let mut chosen: Option<&Vec<String>> = None;
        for (i, harness) in harnesses.iter().enumerate() {
            let mapped = self
                .config
                .harness_commands
                .get(harness.unwrap_or("claude-code"));
            if i > 0 && mapped != chosen {
                return Err(ApiError::bad_request(
                    "these harnesses have different configured commands, and one task runs \
                     one command; submit them as separate tasks, or put the harnesses on the \
                     server's PATH",
                ));
            }
            chosen = mapped;
        }
        Ok(chosen.cloned())
    }
}

pub fn router(app: Shared) -> Router {
    let v1 = Router::new()
        .route("/v1/repos", get(repos))
        .route("/v1/harnesses", get(harnesses))
        .route("/v1/operations/{id}", get(operation))
        .route("/v1/repos/{repo}/tasks", axum::routing::post(post_task))
        .route("/v1/repos/{repo}/branches", get(branches))
        .route(
            "/v1/repos/{repo}/branches/{branch}",
            get(branch).delete(delete_branch),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/send",
            axum::routing::post(post_send),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/fork",
            axum::routing::post(post_fork),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/merge",
            axum::routing::post(post_merge),
        )
        .route("/v1/repos/{repo}/branches/{branch}/diff", get(diff))
        .route("/v1/repos/{repo}/branches/{branch}/events", get(events))
        .route("/v1/repos/{repo}/events/stream", get(stream_events));
    let log = app.config.log_requests;
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .merge(v1)
        .fallback(|| async { ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route") })
        .method_not_allowed_fallback(|| async {
            ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "this route does not take that method",
            )
        })
        .layer(middleware::from_fn_with_state(app.clone(), authenticate))
        .layer(tower::limit::GlobalConcurrencyLimitLayer::new(
            MAX_CONCURRENT_REQUESTS,
        ))
        .layer(middleware::from_fn(move |req, next| {
            request_id(log, req, next)
        }))
        .with_state(app)
}

/// Tag every response with a request ID (the caller's, when usable) and
/// log one line per request. Headers, including `Authorization`, are
/// never logged.
async fn request_id(log: bool, request: Request, next: Next) -> Response {
    let started = Instant::now();
    let id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 64
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
        .map(str::to_owned)
        .unwrap_or_else(|| format!("req_{}", &branchyard_client::new_key()[..16]));
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", value);
    }
    if log {
        eprintln!(
            "branchyard-server: {id} {method} {path} {} {}ms",
            response.status().as_u16(),
            started.elapsed().as_millis()
        );
    }
    response
}

/// Everything but `/healthz` needs a token, unknown routes included, so
/// routes cannot be probed anonymously.
async fn authenticate(State(app): State<Shared>, mut request: Request, next: Next) -> Response {
    if request.uri().path() == "/healthz" {
        return next.run(request).await;
    }
    let header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    match app.tokens.verify(header) {
        Some(name) => {
            let caller = Caller(name.to_owned());
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        None => ApiError::unauthorized().into_response(),
    }
}

/// A JSON request body, bounded by `max_body_bytes`, with the request
/// re-serialized in canonical form for idempotency fingerprints.
pub struct JsonBody<T>(pub T, pub String);

impl<T: DeserializeOwned + Serialize> FromRequest<Shared> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, app: &Shared) -> Result<Self, ApiError> {
        let json = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
            });
        if !json {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "send the request body as Content-Type: application/json",
            ));
        }
        let limit = app.config.max_body_bytes;
        let bytes = axum::body::to_bytes(request.into_body(), limit)
            .await
            .map_err(|_| {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "body_too_large",
                    format!("the request body is larger than {limit} bytes"),
                )
                .detail(serde_json::json!({ "limit": limit }))
            })?;
        let value: T = serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?;
        let canonical =
            serde_json::to_string(&value).map_err(|e| ApiError::internal(e.to_string()))?;
        Ok(JsonBody(value, canonical))
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ApiError::internal(format!("worker failed: {e}")))
}

/// FNV-1a, 64-bit: a stable fingerprint, not a security boundary.
fn fingerprint(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn idempotency(
    headers: &HeaderMap,
    caller: &Caller,
    route: &str,
    canonical: &str,
) -> Result<Option<Idempotency>, ApiError> {
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
    Ok(Some(Idempotency {
        caller: caller.0.clone(),
        key: key.to_owned(),
        fingerprint: fingerprint(&format!("{route}\n{canonical}")),
    }))
}

/// `202 Accepted` for work in progress, `200` once finished; either way the
/// operation, with its location.
fn operation_response(op: Operation, replayed: bool) -> Response {
    let status = match op.state.is_terminal() {
        true => StatusCode::OK,
        false => StatusCode::ACCEPTED,
    };
    let location = format!("/v1/operations/{}", op.id);
    let mut response = (status, Json(op)).into_response();
    if let Ok(value) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    if replayed {
        response
            .headers_mut()
            .insert("idempotent-replayed", HeaderValue::from_static("true"));
    }
    response
}

fn cursor_param(query: Option<&str>) -> Result<Option<u64>, ApiError> {
    for pair in query.unwrap_or("").split('&') {
        if let Some(value) = pair.strip_prefix("cursor=") {
            return value
                .parse()
                .map(Some)
                .map_err(|_| ApiError::bad_request("cursor must be a whole number"));
        }
    }
    Ok(None)
}

async fn sync_feed(feed: &Arc<Feed>) -> Result<u64, ApiError> {
    let feed = feed.clone();
    blocking(move || feed.sync())
        .await?
        .map_err(|e| ApiError::internal(format!("could not read the event logs: {e}")))
}

/// Run `work` as an operation's job: afterwards, ingest its activity so
/// the operation's end cursor covers all of it.
fn job(
    feed: Arc<Feed>,
    work: impl FnOnce() -> Result<OperationResult, ErrorBody> + Send + 'static,
) -> Job {
    Box::new(move || {
        let result = work();
        let end_cursor = match feed.sync() {
            Ok(head) => Some(head),
            Err(e) => {
                eprintln!("branchyard-server: could not ingest activity: {e}");
                None
            }
        };
        Finished { result, end_cursor }
    })
}

fn observer(wake: &Arc<Notify>) -> Observer {
    let wake = wake.clone();
    Arc::new(move |_: &BranchEvent| wake.notify_one())
}

fn branch_infos(branches: Vec<branchyard::Branch>) -> OperationResult {
    OperationResult {
        branches: branches.iter().map(|b| b.info().clone()).collect(),
        merged: None,
    }
}

async fn repos(State(app): State<Shared>) -> Json<RepoList> {
    Json(RepoList {
        repos: app
            .repos
            .values()
            .map(|r| RepoEntry {
                name: r.name.clone(),
                root: r.yard.root().display().to_string(),
            })
            .collect(),
    })
}

async fn harnesses(State(app): State<Shared>) -> Result<Json<HarnessList>, ApiError> {
    let yard = app
        .repos
        .values()
        .next()
        .map(|r| r.yard.clone())
        .ok_or_else(|| ApiError::internal("no repositories"))?;
    let list = blocking(move || yard.harnesses()).await?;
    Ok(Json(HarnessList {
        harnesses: list.iter().map(HarnessEntry::from).collect(),
    }))
}

async fn operation(
    State(app): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<Operation>, ApiError> {
    app.registry.get(&id).map(Json).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_operation",
            format!("no operation {id}"),
        )
    })
}

async fn post_task(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(request, canonical): JsonBody<TaskRequest>,
) -> Result<Response, ApiError> {
    let repo = app.repo(&repo)?.clone();
    let route = format!("POST /v1/repos/{}/tasks", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(op) = idem
        .as_ref()
        .map(|i| app.registry.replay(i))
        .transpose()?
        .flatten()
    {
        return Ok(operation_response(op, true));
    }
    if request.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt is empty"));
    }
    if request.harness.is_some() && !request.harnesses.is_empty() {
        return Err(ApiError::bad_request(
            "give harness for one branch or harnesses for several, not both",
        ));
    }
    let budget = request.budget.to_budget().map_err(ApiError::bad_request)?;
    let targets: Vec<Option<&str>> = match request.harnesses.is_empty() {
        true => vec![request.harness.as_deref()],
        false => request.harnesses.iter().map(|h| Some(h.as_str())).collect(),
    };
    let command = app.command(request.command.clone(), &targets)?;
    let options = TaskOptions {
        harness: request.harness.clone(),
        name: request.name.clone(),
        base: request.base.clone(),
        budget,
        policy: request.policy.to_policy(),
        check: request.check.clone(),
        observer: Some(observer(&repo.wake)),
        isolated: request.isolated,
        command,
        // The server runs harnesses locally and offers them no delegation
        // tools yet.
        provider: None,
        delegation: None,
        delegation_cli: None,
        delegation_server: None,
    };
    let harnesses = request.harnesses.clone();
    let prompt = request.prompt.clone();
    let planned = {
        let (yard, options, prompt, harnesses) = (
            repo.yard.clone(),
            options.clone(),
            prompt.clone(),
            harnesses.clone(),
        );
        blocking(move || {
            let ids: Vec<&str> = harnesses.iter().map(String::as_str).collect();
            yard.task(prompt).options(options).planned_names(&ids)
        })
        .await?
        .map_err(|e| error::sdk(&e))?
    };
    let cursor = sync_feed(&repo.feed).await?;
    let yard = repo.yard.clone();
    let work = job(repo.feed.clone(), move || {
        let builder = yard.task(prompt).options(options);
        let result = match harnesses.is_empty() {
            true => builder.run().map(|b| vec![b]),
            false => {
                let ids: Vec<&str> = harnesses.iter().map(String::as_str).collect();
                builder.run_on(&ids)
            }
        };
        result.map(branch_infos).map_err(|e| *error::sdk(&e).body)
    });
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Task,
        locks: planned.clone(),
        branches: planned,
        cursor,
        idempotency: idem,
    };
    let (op, replayed) = app.registry.submit(new, work)?;
    Ok(operation_response(op, replayed))
}

/// The branch's record, or `unknown_branch`.
async fn existing(yard: &Yard, name: &str) -> Result<branchyard::Branch, ApiError> {
    let (yard, name) = (yard.clone(), name.to_owned());
    blocking(move || yard.branch(&name))
        .await?
        .map_err(|e| error::sdk(&e))
}

async fn post_send(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(request, canonical): JsonBody<SendRequest>,
) -> Result<Response, ApiError> {
    let repo = app.repo(&repo)?.clone();
    let route = format!("POST /v1/repos/{}/branches/{branch}/send", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(op) = idem
        .as_ref()
        .map(|i| app.registry.replay(i))
        .transpose()?
        .flatten()
    {
        return Ok(operation_response(op, true));
    }
    if request.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt is empty"));
    }
    let budget = request.budget.to_budget().map_err(ApiError::bad_request)?;
    // A send keeps the branch's recorded command unless the request
    // replaces it.
    let command = match request.command.clone() {
        Some(command) => app.command(Some(command), &[])?,
        None => None,
    };
    let target = existing(&repo.yard, &branch).await?;
    let options = TaskOptions {
        budget,
        policy: request.policy.to_policy(),
        check: request.check.clone(),
        observer: Some(observer(&repo.wake)),
        command,
        ..TaskOptions::default()
    };
    let cursor = sync_feed(&repo.feed).await?;
    let prompt = request.prompt.clone();
    let work = job(repo.feed.clone(), move || {
        target
            .send(&prompt, options)
            .map(|b| branch_infos(vec![b]))
            .map_err(|e| *error::sdk(&e).body)
    });
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Send,
        branches: vec![branch.clone()],
        cursor,
        locks: vec![branch],
        idempotency: idem,
    };
    let (op, replayed) = app.registry.submit(new, work)?;
    Ok(operation_response(op, replayed))
}

async fn post_fork(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(request, canonical): JsonBody<ForkRequest>,
) -> Result<Response, ApiError> {
    let repo = app.repo(&repo)?.clone();
    let route = format!("POST /v1/repos/{}/branches/{branch}/fork", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(op) = idem
        .as_ref()
        .map(|i| app.registry.replay(i))
        .transpose()?
        .flatten()
    {
        return Ok(operation_response(op, true));
    }
    if request.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt is empty"));
    }
    let budget = request.budget.to_budget().map_err(ApiError::bad_request)?;
    // Without a harness the fork keeps its parent's, and its command.
    let command = match (&request.command, &request.harness) {
        (Some(_), _) => app.command(request.command.clone(), &[])?,
        (None, Some(harness)) => app.command(None, &[Some(harness)])?,
        (None, None) => None,
    };
    let source = existing(&repo.yard, &branch).await?;
    let options = TaskOptions {
        harness: request.harness.clone(),
        name: request.name.clone(),
        budget,
        policy: request.policy.to_policy(),
        check: request.check.clone(),
        observer: Some(observer(&repo.wake)),
        isolated: request.isolated,
        command,
        ..TaskOptions::default()
    };
    let planned = {
        let (yard, options, prompt) = (repo.yard.clone(), options.clone(), request.prompt.clone());
        blocking(move || yard.task(prompt).options(options).planned_names(&[]))
            .await?
            .map_err(|e| error::sdk(&e))?
    };
    let cursor = sync_feed(&repo.feed).await?;
    let (prompt, fresh) = (request.prompt.clone(), request.fresh_session);
    let work = job(repo.feed.clone(), move || {
        source
            .fork(&prompt, fresh, options)
            .map(|b| branch_infos(vec![b]))
            .map_err(|e| *error::sdk(&e).body)
    });
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Fork,
        locks: planned.clone(),
        branches: planned,
        cursor,
        idempotency: idem,
    };
    let (op, replayed) = app.registry.submit(new, work)?;
    Ok(operation_response(op, replayed))
}

/// The branch checked out in the served repository.
fn current_branch(root: &std::path::Path) -> Result<String, ApiError> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(root)
        .output()
        .map_err(|e| ApiError::internal(format!("could not run git: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "git_error",
            format!("could not resolve the current branch: {}", stderr.trim()),
        ));
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if name == "HEAD" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "detached_head",
            "the served repository's HEAD is detached; pass a target",
        ));
    }
    Ok(name)
}

async fn post_merge(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(request, canonical): JsonBody<MergeRequest>,
) -> Result<Response, ApiError> {
    let repo = app.repo(&repo)?.clone();
    let route = format!("POST /v1/repos/{}/branches/{branch}/merge", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(op) = idem
        .as_ref()
        .map(|i| app.registry.replay(i))
        .transpose()?
        .flatten()
    {
        return Ok(operation_response(op, true));
    }
    existing(&repo.yard, &branch).await?;
    let target = match request.target.clone() {
        Some(target) => target,
        None => {
            let root = repo.yard.root().to_path_buf();
            blocking(move || current_branch(&root)).await??
        }
    };
    let cursor = sync_feed(&repo.feed).await?;
    let (yard, name) = (repo.yard.clone(), branch.clone());
    let work = job(repo.feed.clone(), move || {
        let merged = yard
            .merge(&name, &target)
            .map_err(|e| *error::sdk(&e).body)?;
        let branches = yard
            .branch(&name)
            .map(|b| vec![b.info().clone()])
            .unwrap_or_default();
        Ok(OperationResult {
            branches,
            merged: Some(MergedInfo::from(merged)),
        })
    });
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Merge,
        branches: vec![branch.clone()],
        cursor,
        locks: vec![branch],
        idempotency: idem,
    };
    let (op, replayed) = app.registry.submit(new, work)?;
    Ok(operation_response(op, replayed))
}

async fn branches(
    State(app): State<Shared>,
    Path(repo): Path<String>,
) -> Result<Json<BranchList>, ApiError> {
    let yard = app.repo(&repo)?.yard.clone();
    let branches = blocking(move || yard.branches())
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(BranchList { branches }))
}

async fn branch(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
) -> Result<Json<branchyard::BranchInfo>, ApiError> {
    let yard = &app.repo(&repo)?.yard;
    Ok(Json(existing(yard, &branch).await?.info().clone()))
}

async fn diff(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
) -> Result<Json<Diff>, ApiError> {
    let target = existing(&app.repo(&repo)?.yard, &branch).await?;
    let diff = blocking(move || target.diff())
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(Diff { diff }))
}

async fn events(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<BranchEvents>, ApiError> {
    let cursor = cursor_param(query.as_deref())?.unwrap_or(0);
    let target = existing(&app.repo(&repo)?.yard, &branch).await?;
    let all = blocking(move || target.events())
        .await?
        .map_err(|e| error::sdk(&e))?;
    let total = all.len() as u64;
    let events = all.into_iter().skip(cursor as usize).collect();
    Ok(Json(BranchEvents {
        events,
        cursor: total.max(cursor),
    }))
}

async fn delete_branch(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
) -> Result<Json<Removed>, ApiError> {
    let repo = app.repo(&repo)?.clone();
    let hold = app.registry.hold(&repo.name, &branch, "a removal")?;
    let name = branch.clone();
    blocking(move || {
        // Take in the branch's last activity before its log goes.
        let _ = repo.feed.sync();
        let removed = repo.yard.remove(&name);
        if removed.is_ok() {
            repo.feed.forget(&name);
        }
        drop(hold);
        removed
    })
    .await?
    .map_err(|e| error::sdk(&e))?;
    Ok(Json(Removed { removed: branch }))
}

struct Streaming {
    feed: Arc<Feed>,
    cursor: u64,
    buffer: VecDeque<FeedEntry>,
    head: watch::Receiver<u64>,
    shutdown: watch::Receiver<bool>,
}

async fn next_entry(mut s: Streaming) -> Option<(Result<SseEvent, Infallible>, Streaming)> {
    loop {
        if let Some(entry) = s.buffer.pop_front() {
            let data = serde_json::to_string(&entry).ok()?;
            let event = SseEvent::default()
                .event("activity")
                .id(entry.seq.to_string())
                .data(data);
            return Some((Ok(event), s));
        }
        if *s.shutdown.borrow() {
            return None;
        }
        let head = *s.head.borrow_and_update();
        if s.cursor < head {
            let (feed, cursor) = (s.feed.clone(), s.cursor);
            let entries =
                tokio::task::spawn_blocking(move || feed.read_after(cursor, STREAM_BATCH))
                    .await
                    .ok()?
                    .ok()?;
            // Ending the stream makes the client reconnect from its cursor.
            let last = entries.last()?.seq;
            s.cursor = last;
            s.buffer.extend(entries);
            continue;
        }
        tokio::select! {
            changed = s.head.changed() => {
                if changed.is_err() {
                    return None;
                }
            }
            _ = s.shutdown.changed() => {}
        }
    }
}

async fn stream_events(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let repo = app.repo(&repo)?;
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let head = repo.feed.head();
    let cursor = last_event_id
        .or(cursor_param(query.as_deref())?)
        .unwrap_or(head);
    if cursor > head {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "cursor_out_of_range",
            format!("cursor {cursor} is past the end of the feed ({head})"),
        )
        .detail(serde_json::json!({ "head": head })));
    }
    let open = SseEvent::default()
        .event("open")
        .id(cursor.to_string())
        .data(serde_json::json!({ "cursor": cursor, "head": head }).to_string());
    let state = Streaming {
        feed: repo.feed.clone(),
        cursor,
        buffer: VecDeque::new(),
        head: repo.feed.subscribe(),
        shutdown: app.shutdown.clone(),
    };
    let events = stream::once(async move { Ok(open) }).chain(stream::unfold(state, next_entry));
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_are_stable() {
        assert_eq!(fingerprint(""), "cbf29ce484222325");
        assert_ne!(fingerprint("a"), fingerprint("b"));
    }

    #[test]
    fn cursors_parse_or_are_refused() {
        assert_eq!(cursor_param(None).unwrap(), None);
        assert_eq!(cursor_param(Some("x=1&cursor=12")).unwrap(), Some(12));
        assert!(cursor_param(Some("cursor=-1")).is_err());
    }
}
