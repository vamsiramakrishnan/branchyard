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
use branchyard::{
    BranchEvent, Budget, Envelope, Observer, Policy, Provider, Provisioning, Seats, TaskOptions,
    Yard,
};
use branchyard_client::api::{
    BranchEvents, BranchList, CancelRequest, CancelResult, Diff, FeedEntry, ForkRequest,
    HarnessList, IntegrateRequest, MergeRequest, Operation, OperationKind, PolicySpec,
    ReincarnateRequest, Removed, RepoEntry, RepoList, SendRequest, SpawnRequest, SteerRequest,
    TaskRequest,
};
use futures_util::stream::{self, Stream, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::{watch, Notify};

use crate::auth::Tokens;
use crate::config::Config;
use crate::error::{self, ApiError};
use crate::feed::Feed;
use crate::ops::{NewOperation, Registry};
use crate::store::Idempotency;
use crate::work::{self, Work};

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
    /// Idempotency cache for the storage routes (artifacts, scratch
    /// areas): quick, synchronous calls, unlike the operation registry's
    /// durable one for long operations. See `storage_routes`.
    pub storage_idem: crate::storage_routes::StorageIdem,
}

#[derive(Clone)]
pub struct RepoState {
    pub name: String,
    pub yard: Yard,
    pub feed: Arc<Feed>,
    /// Wakes the feed's poller when the engine records activity.
    pub wake: Arc<Notify>,
}

pub(crate) type Shared = Arc<App>;

/// The authenticated caller: its token's configured name.
#[derive(Clone)]
pub struct Caller(pub String);

impl App {
    pub(crate) fn repo(&self, name: &str) -> Result<&RepoState, ApiError> {
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
    pub(crate) fn command(
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

impl App {
    /// The request's provider, if this server allows it. `local` always
    /// is; a Substrate key must be an absolute path on this server.
    pub(crate) fn provider(
        &self,
        requested: Option<Provider>,
    ) -> Result<Option<Provider>, ApiError> {
        let Some(provider) = requested else {
            return Ok(None);
        };
        let kind = match &provider {
            Provider::Local => return Ok(Some(provider)),
            Provider::Microsandbox(_) => "microsandbox",
            Provider::Substrate(_) => "substrate",
        };
        if !self.config.allow_providers.contains(kind) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "provider_not_allowed",
                format!(
                    "this server does not run harnesses with the {kind} provider; its operator \
                     can allow it with --allow-provider {kind}"
                ),
            )
            .detail(serde_json::json!({ "provider": kind })));
        }
        if let Provider::Substrate(options) = &provider {
            if !options.key.is_absolute() {
                return Err(ApiError::bad_request(format!(
                    "provider.key must be an absolute path on the server, not {}",
                    options.key.display()
                )));
            }
        }
        Ok(Some(provider))
    }

    /// The request's provisioning, with this server's source for each
    /// secret: a request names secrets and never chooses where they come
    /// from. MCP servers are commands this server runs, so they need
    /// client commands allowed.
    pub(crate) fn provision(
        &self,
        requested: Option<Provisioning>,
    ) -> Result<Option<Provisioning>, ApiError> {
        let Some(mut spec) = requested else {
            return Ok(None);
        };
        for secret in &mut spec.secrets {
            if secret.from.is_some() {
                return Err(ApiError::bad_request(format!(
                    "secret {} names a source; a request names only the secret, and this \
                     server's operator decides where it comes from",
                    secret.name
                )));
            }
            match self.config.secrets.get(&secret.name) {
                Some(source) => *secret = source.clone(),
                None => {
                    return Err(ApiError::new(
                        StatusCode::FORBIDDEN,
                        "secret_not_allowed",
                        format!(
                            "this server has no secret {name}; its operator can define one \
                             with --secret {name}[=VAR|=@FILE]",
                            name = secret.name
                        ),
                    )
                    .detail(serde_json::json!({ "secret": secret.name })))
                }
            }
        }
        if !spec.mcp_servers.is_empty() && !self.config.allow_client_commands {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "command_not_allowed",
                "MCP servers are commands this server runs; its operator can accept a \
                 request's own commands with allow_client_commands",
            ));
        }
        // A remote server receives its headers, which are the server's
        // secrets: a request choosing the URL chooses where they go.
        if !spec.remote_mcp_servers.is_empty() && !self.config.allow_client_commands {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "command_not_allowed",
                "HTTP and SSE MCP servers receive this server's secrets as headers at a URL the \
                 request chooses; its operator can accept them with allow_client_commands",
            ));
        }
        Ok(Some(spec))
    }

    /// A rig's seats, with each seat's provisioning held to the rules of
    /// [`App::provision`].
    pub(crate) fn seats(&self, requested: Option<Seats>) -> Result<Option<Seats>, ApiError> {
        let Some(mut seats) = requested else {
            return Ok(None);
        };
        for seat in seats.table.values_mut() {
            seat.provision = self.provision(seat.provision.take())?;
        }
        Ok(Some(seats))
    }

    /// Refuse delegation and unapproved tools unless this server allows
    /// them.
    pub(crate) fn opt_ins(
        &self,
        delegation: bool,
        allow_delegation: bool,
        unapproved_tools: bool,
    ) -> Result<(), ApiError> {
        if (delegation || allow_delegation) && !self.config.allow_delegation {
            return Err(delegation_not_allowed(
                "this server does not offer delegation to its harnesses; its operator can \
                 allow it with --allow-delegation",
            ));
        }
        if unapproved_tools && !self.config.allow_unapproved_tools {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "unapproved_tools_not_allowed",
                "this server does not run profiles whose tools bypass the policy; its operator \
                 can allow it with --allow-unapproved-tools",
            ));
        }
        Ok(())
    }

    /// The `by` a delegating harness is told about, for the delegation
    /// command rule.
    fn by_path(&self) -> std::path::PathBuf {
        self.config
            .by_path
            .clone()
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| "by".into())
    }

    /// The request's policy, with the delegation command rule last when
    /// asked for, as `by --allow-delegation` adds it.
    pub(crate) fn policy(&self, spec: &PolicySpec, allow_delegation: bool) -> Policy {
        let policy = spec.to_policy();
        match allow_delegation {
            true => policy.allow_delegation_commands(self.by_path()),
            false => policy,
        }
    }

    /// Options every request that runs a harness shares.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn options(
        &self,
        repo: &RepoState,
        budget: Budget,
        policy: Policy,
        check: Option<Vec<String>>,
        command: Option<Vec<String>>,
        delegation: Option<Envelope>,
        unapproved_tools: bool,
        provider: Option<Provider>,
    ) -> TaskOptions {
        TaskOptions {
            budget,
            policy,
            check,
            observer: Some(observer(&repo.wake)),
            command,
            provider,
            delegation,
            delegation_cli: self.config.by_path.clone(),
            unapproved_tools,
            ..TaskOptions::default()
        }
    }
}

pub(crate) fn delegation_not_allowed(message: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "delegation_not_allowed", message)
}

pub fn router(app: Shared) -> Router {
    let v1 = Router::new()
        .route("/v1/repos", get(repos))
        .route("/v1/harnesses", get(harnesses))
        .route("/v1/operations", get(operation_by_key))
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
            "/v1/repos/{repo}/branches/{branch}/reincarnate",
            axum::routing::post(post_reincarnate),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/merge",
            axum::routing::post(post_merge),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/cancel",
            axum::routing::post(post_cancel),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/steer",
            axum::routing::post(post_steer),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/spawn",
            axum::routing::post(post_spawn),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/integrate",
            axum::routing::post(post_integrate),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/inspection",
            get(inspection),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/event-page",
            get(event_page),
        )
        .route("/v1/repos/{repo}/branches/{branch}/children", get(children))
        .route("/v1/repos/{repo}/branches/{branch}/inbox", get(inbox))
        .route(
            "/v1/repos/{repo}/branches/{branch}/ask",
            axum::routing::post(post_ask),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/report",
            axum::routing::post(post_report),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/escalate",
            axum::routing::post(post_escalate),
        )
        .route(
            "/v1/repos/{repo}/branches/{branch}/answer",
            axum::routing::post(post_answer),
        )
        .route("/v1/repos/{repo}/branches/{branch}/diff", get(diff))
        .route("/v1/repos/{repo}/branches/{branch}/events", get(events))
        .route("/v1/repos/{repo}/events/stream", get(stream_events))
        // Artifacts and scratch areas: see `storage_routes`, kept separate
        // so this feature's routes are easy to merge alongside unrelated
        // work on this router (inbox messages, branch lifecycle).
        .merge(crate::storage_routes::router());
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

pub(crate) async fn blocking<T: Send + 'static>(
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

/// A request that failed before it was registered, returned as the
/// operation its idempotency key already names, if one does. A retry that
/// passed the first replay check while its original attempt was still being
/// admitted can fail on a name that attempt has since taken; the key, not
/// the refusal, decides the answer.
async fn replay_or(
    app: &Shared,
    idem: Option<&Idempotency>,
    error: ApiError,
) -> Result<Response, ApiError> {
    match replayed(app, idem).await? {
        Some(response) => Ok(response),
        None => Err(error),
    }
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
        .map_err(|e| ApiError::internal(format!("could not read the event feed: {e}")))
}

pub(crate) fn observer(wake: &Arc<Notify>) -> Observer {
    let wake = wake.clone();
    Arc::new(move |_: &BranchEvent| wake.notify_one())
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
    Ok(Json(HarnessList { harnesses: list }))
}

fn unknown_operation(what: String) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "unknown_operation",
        format!("no operation {what}"),
    )
}

async fn operation(
    State(app): State<Shared>,
    Path(id): Path<String>,
) -> Result<Json<Operation>, ApiError> {
    let registry = app.registry.clone();
    let found = {
        let id = id.clone();
        blocking(move || registry.get(&id)).await??
    };
    found.map(Json).ok_or_else(|| unknown_operation(id))
}

/// `GET /v1/operations?idempotency_key=KEY`: the operation the caller's
/// request with that key created, on any server sharing the operation
/// store. What a client that lost the response to its `POST` reconciles
/// with, besides retrying the `POST` with the same key.
async fn operation_by_key(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    RawQuery(query): RawQuery,
) -> Result<Json<Operation>, ApiError> {
    let key = query
        .as_deref()
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix("idempotency_key="))
        .map(percent_decode)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| ApiError::bad_request("give idempotency_key"))?;
    let registry = app.registry.clone();
    let found = {
        let key = key.clone();
        blocking(move || registry.by_key(&caller.0, &key)).await??
    };
    found
        .map(Json)
        .ok_or_else(|| unknown_operation(format!("with idempotency key {key}")))
}

/// `%XX` escapes and `+` in a query value.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Admit `work` as an operation: durably enqueued before this returns.
async fn admit(app: &Shared, new: NewOperation, work: Work) -> Result<Response, ApiError> {
    let value = work.to_value()?;
    let registry = app.registry.clone();
    let (op, replayed) = blocking(move || registry.submit(new, value)).await??;
    Ok(operation_response(op, replayed))
}

/// The operation a request's idempotency key already names, if any.
async fn replayed(app: &Shared, idem: Option<&Idempotency>) -> Result<Option<Response>, ApiError> {
    let Some(idem) = idem.cloned() else {
        return Ok(None);
    };
    let registry = app.registry.clone();
    let found = blocking(move || registry.replay(&idem)).await??;
    Ok(found.map(|op| operation_response(op, true)))
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
    if let Some(response) = replayed(&app, idem.as_ref()).await? {
        return Ok(response);
    }
    let options = work::task_options(&app, &repo, &request)?;
    let planned = {
        let (yard, prompt, harnesses) = (
            repo.yard.clone(),
            request.prompt.clone(),
            request.harnesses.clone(),
        );
        match blocking(move || {
            let ids: Vec<&str> = harnesses.iter().map(String::as_str).collect();
            yard.task(prompt).options(options).planned_names(&ids)
        })
        .await?
        {
            Ok(planned) => planned,
            Err(e) => return replay_or(&app, idem.as_ref(), error::sdk(&e)).await,
        }
    };
    let cursor = sync_feed(&repo.feed).await?;
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Task,
        locks: planned.clone(),
        branches: planned,
        cursor,
        idempotency: idem,
    };
    admit(&app, new, Work::Task { request }).await
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
    if let Some(response) = replayed(&app, idem.as_ref()).await? {
        return Ok(response);
    }
    work::send_options(&app, &repo, &request)?;
    let target = existing(&repo.yard, &branch).await?;
    {
        let (app, branch, request) = (app.clone(), branch.clone(), request.clone());
        blocking(move || work::send_allowed(&app, &target, &branch, &request)).await??;
    }
    let cursor = sync_feed(&repo.feed).await?;
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Send,
        branches: vec![branch.clone()],
        cursor,
        locks: vec![branch.clone()],
        idempotency: idem,
    };
    admit(&app, new, Work::Send { branch, request }).await
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
    if let Some(response) = replayed(&app, idem.as_ref()).await? {
        return Ok(response);
    }
    let options = work::fork_options(&app, &repo, &request)?;
    existing(&repo.yard, &branch).await?;
    let planned = {
        let (yard, prompt) = (repo.yard.clone(), request.prompt.clone());
        match blocking(move || yard.task(prompt).options(options).planned_names(&[])).await? {
            Ok(planned) => planned,
            Err(e) => return replay_or(&app, idem.as_ref(), error::sdk(&e)).await,
        }
    };
    let cursor = sync_feed(&repo.feed).await?;
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Fork,
        locks: planned.clone(),
        branches: planned,
        cursor,
        idempotency: idem,
    };
    admit(&app, new, Work::Fork { branch, request }).await
}

async fn post_reincarnate(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(request, canonical): JsonBody<ReincarnateRequest>,
) -> Result<Response, ApiError> {
    let repo = app.repo(&repo)?.clone();
    let route = format!("POST /v1/repos/{}/branches/{branch}/reincarnate", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(response) = replayed(&app, idem.as_ref()).await? {
        return Ok(response);
    }
    let options = work::reincarnate_options(&app, &repo, &request)?;
    let source = existing(&repo.yard, &branch).await?;
    let planned = {
        let (yard, prompt) = (repo.yard.clone(), source.info().prompt.clone());
        match blocking(move || yard.task(prompt).options(options).planned_names(&[])).await? {
            Ok(planned) => planned,
            Err(e) => return replay_or(&app, idem.as_ref(), error::sdk(&e)).await,
        }
    };
    let cursor = sync_feed(&repo.feed).await?;
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Reincarnate,
        locks: planned.clone(),
        branches: planned,
        cursor,
        idempotency: idem,
    };
    admit(&app, new, Work::Reincarnate { branch, request }).await
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
    if let Some(response) = replayed(&app, idem.as_ref()).await? {
        return Ok(response);
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
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Merge,
        branches: vec![branch.clone()],
        cursor,
        locks: vec![branch.clone()],
        idempotency: idem,
    };
    admit(&app, new, Work::Merge { branch, target }).await
}

/// Ask the branch's running turn and every running turn below it to stop.
/// Not an operation and not subject to branch locks: it is quick,
/// idempotent, and meant for exactly the branches an operation holds. The
/// request is durable in the repository's store, and the engine running
/// the turn, here or in another process, observes it.
async fn post_cancel(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(CancelRequest {}, _): JsonBody<CancelRequest>,
) -> Result<Json<CancelResult>, ApiError> {
    let yard = app.repo(&repo)?.yard.clone();
    let by = format!("{} through the server", caller.0);
    let cancelled = blocking(move || yard.cancel_as(&branch, &by))
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(CancelResult { cancelled }))
}

/// How long a steer request waits for the engine running the turn to
/// deliver the input.
const STEER_WAIT: Duration = Duration::from_secs(10);

/// Add input to the branch's running turn without interrupting it, like
/// `by send --steer`. Like a cancel, not an operation and not subject to
/// branch locks: the running turn's operation holds them, and the input is
/// for exactly that turn. It is queued durably, bound to the turn, and the
/// engine running it, here or in another process, delivers it; the answer
/// waits briefly for that. Refused with 409 `not_running` when no turn
/// runs, and 422 `unsupported` when the harness cannot take input mid-turn.
async fn post_steer(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(SteerRequest { text }, _): JsonBody<SteerRequest>,
) -> Result<Json<branchyard::Steer>, ApiError> {
    let yard = app.repo(&repo)?.yard.clone();
    let by = format!("{} through the server", caller.0);
    let steer = blocking(move || {
        let steer = yard.steer_as(&branch, &text, &by)?;
        yard.wait_steer(&branch, steer.id, STEER_WAIT)
    })
    .await?
    .map_err(|e| error::sdk(&e))?;
    Ok(Json(steer))
}

/// Create a child of `parent` with the server's authority as a person,
/// bounded by the parent's envelope, and run its first turn: what
/// `by spawn --parent` does. The operation waits for the parent's subtree,
/// as the local command does, and its result holds the child's inspection.
async fn post_spawn(
    State(app): State<Shared>,
    Path((repo, parent)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(request, canonical): JsonBody<SpawnRequest>,
) -> Result<Response, ApiError> {
    let repo = app.repo(&repo)?.clone();
    let route = format!("POST /v1/repos/{}/branches/{parent}/spawn", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(response) = replayed(&app, idem.as_ref()).await? {
        return Ok(response);
    }
    work::spawn_parts(&app, &repo, &request, None)?;
    existing(&repo.yard, &parent).await?;
    // A seat's child is named `<parent>-<seat>` by default, as the engine
    // names it; the name is fixed here so the operation locks and reports
    // the branch it creates.
    let name = match (&request.name, &request.seat) {
        (Some(name), _) => Some(name.clone()),
        (None, Some(seat)) => {
            let (yard, stem) = (repo.yard.clone(), format!("{parent}-{seat}"));
            let names = match blocking(move || yard.task(stem).planned_names(&[])).await? {
                Ok(planned) => planned,
                Err(e) => return replay_or(&app, idem.as_ref(), error::sdk(&e)).await,
            };
            names.into_iter().next()
        }
        (None, None) => None,
    };
    let planned = match &name {
        Some(name) => vec![name.clone()],
        None => {
            let (yard, prompt) = (repo.yard.clone(), request.prompt.clone());
            match blocking(move || yard.task(prompt).planned_names(&[])).await? {
                Ok(planned) => planned,
                Err(e) => return replay_or(&app, idem.as_ref(), error::sdk(&e)).await,
            }
        }
    };
    let cursor = sync_feed(&repo.feed).await?;
    // The parent too: the spawn reads its work and changes its children, so
    // a send or removal of it must not run meanwhile.
    let mut locks = planned.clone();
    locks.push(parent.clone());
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Spawn,
        locks,
        branches: planned,
        cursor,
        idempotency: idem,
    };
    admit(
        &app,
        new,
        Work::Spawn {
            parent,
            name,
            request,
        },
    )
    .await
}

/// Merge a delegated child into the parent that delegated it, with the
/// server's authority as a person: what `by integrate` does outside a
/// harness.
async fn post_integrate(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(IntegrateRequest {}, canonical): JsonBody<IntegrateRequest>,
) -> Result<Response, ApiError> {
    let repo = app.repo(&repo)?.clone();
    let route = format!("POST /v1/repos/{}/branches/{branch}/integrate", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(response) = replayed(&app, idem.as_ref()).await? {
        return Ok(response);
    }
    let parent = {
        let (yard, name) = (repo.yard.clone(), branch.clone());
        blocking(move || work::delegator(&yard, &name))
            .await?
            .map_err(|e| error::sdk(&e))?
    };
    let cursor = sync_feed(&repo.feed).await?;
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Integrate,
        branches: vec![branch.clone()],
        cursor,
        locks: vec![branch.clone(), parent.clone()],
        idempotency: idem,
    };
    admit(&app, new, Work::Integrate { branch, parent }).await
}

/// Act as `branch` with the server's authority, as `by inspect`, `by events`
/// and `by children` do outside a harness.
async fn as_person<T: Send + 'static>(
    app: &App,
    repo: &str,
    branch: String,
    work: impl FnOnce(branchyard::Delegate, &str) -> Result<T, branchyard::Error> + Send + 'static,
) -> Result<T, ApiError> {
    let yard = app.repo(repo)?.yard.clone();
    blocking(move || {
        let delegate = yard.branch(&branch)?.delegate(TaskOptions::default())?;
        work(delegate, &branch)
    })
    .await?
    .map_err(|e| error::sdk(&e))
}

async fn inspection(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
) -> Result<Json<branchyard::Inspection>, ApiError> {
    as_person(&app, &repo, branch, |d, b| d.inspect(b))
        .await
        .map(Json)
}

/// A whole number from the query, if given.
fn query_number(query: Option<&str>, name: &str) -> Result<Option<usize>, ApiError> {
    for pair in query.unwrap_or("").split('&') {
        if let Some(value) = pair.strip_prefix(name).and_then(|v| v.strip_prefix('=')) {
            return value
                .parse()
                .map(Some)
                .map_err(|_| ApiError::bad_request(format!("{name} must be a whole number")));
        }
    }
    Ok(None)
}

async fn event_page(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<branchyard::EventPage>, ApiError> {
    let cursor = query_number(query.as_deref(), "cursor")?;
    let limit = query_number(query.as_deref(), "limit")?.unwrap_or(50);
    as_person(&app, &repo, branch, move |d, b| d.events(b, cursor, limit))
        .await
        .map(Json)
}

async fn children(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
) -> Result<Json<branchyard::Children>, ApiError> {
    as_person(&app, &repo, branch, |d, _| d.children())
        .await
        .map(Json)
}

async fn inbox(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
) -> Result<Json<branchyard::Inbox>, ApiError> {
    as_person(&app, &repo, branch, |d, _| d.inbox())
        .await
        .map(Json)
}

/// Blocking a server worker for a long wait is bounded: a caller that wants
/// longer polls `inbox` instead.
const MAX_ASK_WAIT: Duration = Duration::from_secs(120);

async fn post_ask(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    JsonBody(request, _): JsonBody<branchyard_client::api::AskRequest>,
) -> Result<Json<branchyard::Asked>, ApiError> {
    if request.text.trim().is_empty() {
        return Err(ApiError::bad_request("text is empty"));
    }
    let wait = request
        .wait_seconds
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(|s| Duration::from_secs_f64(s).min(MAX_ASK_WAIT));
    as_person(&app, &repo, branch, move |d, _| d.ask(&request.text, wait))
        .await
        .map(Json)
}

async fn post_report(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    JsonBody(request, _): JsonBody<branchyard_client::api::TextRequest>,
) -> Result<Json<branchyard::Message>, ApiError> {
    if request.text.trim().is_empty() {
        return Err(ApiError::bad_request("text is empty"));
    }
    as_person(&app, &repo, branch, move |d, _| d.report(&request.text))
        .await
        .map(Json)
}

async fn post_escalate(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    JsonBody(request, _): JsonBody<branchyard_client::api::TextRequest>,
) -> Result<Json<branchyard::Message>, ApiError> {
    if request.text.trim().is_empty() {
        return Err(ApiError::bad_request("text is empty"));
    }
    as_person(&app, &repo, branch, move |d, _| d.escalate(&request.text))
        .await
        .map(Json)
}

async fn post_answer(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    JsonBody(request, _): JsonBody<branchyard_client::api::AnswerRequest>,
) -> Result<Json<branchyard::Message>, ApiError> {
    if request.text.trim().is_empty() {
        return Err(ApiError::bad_request("text is empty"));
    }
    as_person(&app, &repo, branch, move |d, _| {
        d.answer(request.message_id, &request.text)
    })
    .await
    .map(Json)
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
    let page = blocking(move || {
        let mut events = Vec::new();
        let mut next = cursor;
        loop {
            let page = target.events_since(next, STREAM_BATCH)?;
            if page.events.is_empty() {
                return Ok::<_, branchyard::Error>(BranchEvents {
                    events,
                    cursor: next,
                });
            }
            next = page.next_cursor;
            events.extend(page.events);
        }
    })
    .await?
    .map_err(|e| error::sdk(&e))?;
    Ok(Json(page))
}

async fn delete_branch(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
) -> Result<Json<Removed>, ApiError> {
    let repo = app.repo(&repo)?.clone();
    let registry = app.registry.clone();
    let name = branch.clone();
    blocking(move || {
        // Held in the operation store, so no operation on any server
        // sharing it changes the branch meanwhile.
        let hold = registry.hold(&repo.name, &name, "a removal")?;
        let removed = repo.yard.remove(&name).map_err(|e| error::sdk(&e));
        drop(hold);
        removed
    })
    .await??;
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
    let head = sync_feed(&repo.feed).await?;
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
