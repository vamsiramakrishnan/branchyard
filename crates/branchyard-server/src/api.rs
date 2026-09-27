//! Routes and handlers. See `docs/server.md` for the reference.
//!
//! Handlers never run the engine on a request's task: reads go to the
//! blocking pool, and changes become operations in the registry.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
    GraphRequest, HarnessList, IntegrateRequest, MergeRequest, Operation, OperationKind,
    PolicySpec, ReincarnateRequest, Removed, RepoEntry, RepoList, SendRequest, SpawnRequest,
    SteerRequest, TaskRequest,
};
use futures_util::stream::{self, Stream, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::{watch, Notify};

use crate::auth::Credentials;
use crate::config::{Config, Principal, TenantPolicy};
use crate::error::{self, ApiError};
use crate::feed::Feed;
use crate::ops::{NewOperation, Registry};
use crate::store::{AdmissionQuota, ExistingBranches, Idempotency};
use crate::work::{self, Work};

/// Requests handled at once; more wait.
const MAX_CONCURRENT_REQUESTS: usize = 256;
/// Feed entries read per batch while streaming.
const STREAM_BATCH: usize = 256;

pub struct App {
    pub repos: BTreeMap<String, RepoState>,
    pub registry: Arc<Registry>,
    pub credentials: Credentials,
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

/// The authenticated caller: the principal its bearer token verified as.
/// Requests never carry a `tenant_id`; every caller's tenant comes only
/// from here. See `docs/server.md#identity-and-scopes`.
#[derive(Clone)]
pub struct Caller(pub Principal);

impl Caller {
    /// The subject name recorded for idempotency, audit and the operation
    /// registry's `busy` messages.
    pub fn name(&self) -> &str {
        &self.0.name
    }

    pub fn tenant(&self) -> &str {
        &self.0.tenant
    }

    /// What this principal's idempotency keys are scoped to: its subject
    /// name in the default tenant (as before tenants existed), else
    /// `tenant/subject`, so two tenants' principals of one name never
    /// share a key.
    pub fn idempotency_scope(&self) -> String {
        match self.tenant() == crate::config::DEFAULT_TENANT {
            true => self.name().to_owned(),
            false => format!("{}/{}", self.tenant(), self.name()),
        }
    }
}

fn scope_required(scope: &str) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "scope_required",
        format!("this request needs the {scope} scope"),
    )
    .detail(serde_json::json!({ "scope": scope }))
}

fn usd_quota_exceeded(limit: &str, tenant: &str, max: f64, spent: f64) -> ApiError {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "quota_exceeded",
        format!(
            "tenant {tenant} is at its {limit} quota (${spent:.4} of ${max:.4}); ask the \
             operator to raise it"
        ),
    )
    .detail(serde_json::json!({ "tenant": tenant, "limit": limit, "max": max, "spent": spent }))
}

fn repo_not_allowed(repo: &str) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "repo_not_allowed",
        format!("this principal may not act on repository {repo}"),
    )
    .detail(serde_json::json!({ "repo": repo }))
}

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

    /// `repo` with `caller` authorized to use it with `scope`: the caller
    /// holds the scope, and its tenant and any repository allowlist of its
    /// own both admit `repo`. See `docs/server.md#quotas` for the tenant
    /// model this enforces (repositories belong to tenants).
    pub(crate) fn authorized_repo(
        &self,
        caller: &Caller,
        name: &str,
        scope: &str,
    ) -> Result<&RepoState, ApiError> {
        let repo = self.repo(name)?;
        if !caller.0.allows(scope) {
            return Err(scope_required(scope));
        }
        let policy = self.config.tenant_policy(caller.tenant());
        if !caller.0.repo_allowed(&policy, name) {
            return Err(repo_not_allowed(name));
        }
        Ok(repo)
    }

    /// This caller's tenant policy.
    pub(crate) fn tenant_policy(&self, caller: &Caller) -> TenantPolicy {
        self.config.tenant_policy(caller.tenant())
    }

    /// Repositories `caller`'s tenant (and, if narrower, the caller itself)
    /// may see.
    pub(crate) fn visible_repos<'a>(
        &'a self,
        caller: &'a Caller,
    ) -> impl Iterator<Item = &'a RepoState> {
        let policy = self.config.tenant_policy(caller.tenant());
        self.repos
            .values()
            .filter(move |r| caller.0.repo_allowed(&policy, &r.name))
    }

    /// Every repository `tenant` owns, regardless of any one principal's
    /// own narrower allowlist: quotas are per tenant, over everything it
    /// can reach.
    pub(crate) fn tenant_repos<'a>(
        &'a self,
        tenant: &'a str,
    ) -> impl Iterator<Item = &'a RepoState> + 'a {
        let policy = self.config.tenant_policy(tenant);
        self.repos
            .values()
            .filter(move |r| policy.allows_repo(&r.name))
    }

    /// The tenant's ceilings its admission checks in the operation
    /// store's transaction: `max_running` against the tenant's queued and
    /// running operations there, and `max_branches` against the branches
    /// its repositories have (read inside that transaction) together with
    /// those its unfinished operations will create. Both hold across every
    /// server sharing the store and across restarts. See
    /// `docs/server.md#quotas`.
    pub(crate) fn admission_quota(&self, caller: &Caller, policy: &TenantPolicy) -> AdmissionQuota {
        let existing_branches = policy.max_branches.map(|_| {
            let yards: Vec<(String, Yard)> = self
                .tenant_repos(caller.tenant())
                .map(|r| (r.name.clone(), r.yard.clone()))
                .collect();
            let read: ExistingBranches = Box::new(move || {
                let mut all = BTreeSet::new();
                for (repo, yard) in &yards {
                    let infos = yard.branches().map_err(std::io::Error::other)?;
                    all.extend(infos.into_iter().map(|b| (repo.clone(), b.name)));
                }
                Ok(all)
            });
            read
        });
        AdmissionQuota {
            max_running: policy.max_running,
            max_branches: policy.max_branches,
            existing_branches,
        }
    }

    /// `max_branches` for a graph proposal's spawns. A proposal is not an
    /// operation, so this is not taken in an admission's transaction: it
    /// counts the tenant's branches and those its unfinished operations
    /// will create, as admission does, but two proposals racing past the
    /// same near-limit tenant can both be applied. See
    /// `docs/server.md#quotas`.
    async fn check_graph_branches(
        self: &Arc<Self>,
        caller: &Caller,
        policy: &TenantPolicy,
        new_children: usize,
    ) -> Result<(), ApiError> {
        let Some(max) = policy.max_branches else {
            return Ok(());
        };
        let quota = self.admission_quota(caller, policy);
        let registry = self.registry.clone();
        let tenant = caller.tenant().to_owned();
        let reserved = blocking(move || {
            let mut branches: BTreeSet<(String, String)> = registry
                .unfinished(&tenant)?
                .into_iter()
                .flat_map(|o| {
                    let repo = o.operation.repo.clone();
                    o.creates.into_iter().map(move |b| (repo.clone(), b))
                })
                .collect();
            if let Some(existing) = &quota.existing_branches {
                branches.extend(existing().map_err(|e| {
                    ApiError::internal(format!("could not read the branches: {e}"))
                })?);
            }
            Ok::<_, ApiError>(branches.len())
        })
        .await??;
        if reserved + new_children > max {
            return Err(crate::ops::quota_exceeded(
                "max_branches",
                caller.tenant(),
                max,
                reserved,
            ));
        }
        Ok(())
    }

    /// `max_cost_usd` and `max_artifact_bytes`, checked live against
    /// durable state before a branch-creating operation (task, fork,
    /// reincarnate, spawn) is admitted. Best-effort, unlike `max_running`
    /// and `max_branches`: not taken in the admission's transaction, so
    /// two requests racing past the same near-limit tenant can both be
    /// admitted, and spend accrues while turns run; being read from
    /// durable branch and artifact state, it needs no recovery of its own
    /// after a restart. See `docs/server.md#quotas`.
    async fn check_admission_quotas(
        self: &Arc<Self>,
        caller: &Caller,
        policy: &TenantPolicy,
    ) -> Result<(), ApiError> {
        if policy.max_cost_usd.is_none() && policy.max_artifact_bytes.is_none() {
            return Ok(());
        }
        let yards: Vec<Yard> = self
            .tenant_repos(caller.tenant())
            .map(|r| r.yard.clone())
            .collect();
        let (cost, artifact_bytes) = blocking(move || {
            let mut cost = 0.0f64;
            let mut artifact_bytes = 0u64;
            // Deduplicated by digest: `Yard::artifacts` is reader-scoped
            // (what a branch may read, including ancestors' and shared
            // ones), so the same artifact appears once per branch that can
            // reach it; a tenant's total counts its bytes once.
            let mut seen_digests = std::collections::HashSet::new();
            for yard in &yards {
                let infos = yard.branches()?;
                cost += infos.iter().filter_map(|b| b.cost_usd).sum::<f64>();
                for info in &infos {
                    for artifact in yard.artifacts(&info.name)? {
                        if seen_digests.insert(artifact.digest.clone()) {
                            artifact_bytes += artifact.size;
                        }
                    }
                }
            }
            Ok::<_, branchyard::Error>((cost, artifact_bytes))
        })
        .await?
        .map_err(|e| error::sdk(&e))?;
        if let Some(max) = policy.max_cost_usd {
            if cost >= max {
                return Err(usd_quota_exceeded(
                    "max_cost_usd",
                    caller.tenant(),
                    max,
                    cost,
                ));
            }
        }
        if let Some(max) = policy.max_artifact_bytes {
            if artifact_bytes >= max {
                return Err(ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "quota_exceeded",
                    format!(
                        "tenant {} is at its max_artifact_bytes quota ({artifact_bytes} of \
                         {max}); ask the operator to raise it",
                        caller.tenant()
                    ),
                )
                .detail(serde_json::json!({
                    "tenant": caller.tenant(), "limit": "max_artifact_bytes", "max": max,
                    "reserved": artifact_bytes
                })));
            }
        }
        Ok(())
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
        .route(
            "/v1/repos/{repo}/branches/{branch}/graph",
            get(graph).post(post_graph),
        )
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
    match app.credentials.verify(header) {
        Some(principal) => {
            let caller = Caller(principal.clone());
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
    caller: &Caller,
    idem: Option<&Idempotency>,
    error: ApiError,
) -> Result<Response, ApiError> {
    match replayed(app, caller, idem).await? {
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
        caller: caller.idempotency_scope(),
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

/// Every repository the caller's tenant (and its own allowlist, if
/// narrower) may see. A single-token caller in the unconfigured default
/// tenant sees every served repository, unchanged from before tenants
/// existed.
async fn repos(State(app): State<Shared>, Extension(caller): Extension<Caller>) -> Json<RepoList> {
    Json(RepoList {
        repos: app
            .visible_repos(&caller)
            .map(|r| RepoEntry {
                name: r.name.clone(),
                root: r.yard.root().display().to_string(),
            })
            .collect(),
    })
}

async fn harnesses(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<HarnessList>, ApiError> {
    if !caller.0.allows("read") {
        return Err(scope_required("read"));
    }
    let yard = app
        .visible_repos(&caller)
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

/// An operation only its own tenant may read: another tenant's, like an
/// unknown ID, is `404`, so a principal cannot tell the two apart.
async fn operation(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Operation>, ApiError> {
    let registry = app.registry.clone();
    let found = {
        let id = id.clone();
        let tenant = caller.tenant().to_owned();
        blocking(move || registry.get_for_tenant(&id, &tenant)).await??
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
        blocking(move || registry.by_key(&caller.idempotency_scope(), &key, caller.tenant()))
            .await??
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
async fn replayed(
    app: &Shared,
    caller: &Caller,
    idem: Option<&Idempotency>,
) -> Result<Option<Response>, ApiError> {
    let Some(idem) = idem.cloned() else {
        return Ok(None);
    };
    let registry = app.registry.clone();
    let tenant = caller.tenant().to_owned();
    let found = blocking(move || registry.replay(&idem, &tenant)).await??;
    Ok(found.map(|op| operation_response(op, true)))
}

async fn post_task(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    JsonBody(request, canonical): JsonBody<TaskRequest>,
) -> Result<Response, ApiError> {
    let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
    let route = format!("POST /v1/repos/{}/tasks", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(response) = replayed(&app, &caller, idem.as_ref()).await? {
        return Ok(response);
    }
    let policy = app.tenant_policy(&caller);
    app.check_admission_quotas(&caller, &policy).await?;
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
            Err(e) => return replay_or(&app, &caller, idem.as_ref(), error::sdk(&e)).await,
        }
    };
    let cursor = sync_feed(&repo.feed).await?;
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Task,
        locks: planned.clone(),
        creates: planned.clone(),
        branches: planned,
        cursor,
        idempotency: idem,
        principal: caller.0.clone(),
        quota: app.admission_quota(&caller, &policy),
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
    let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
    let policy = app.tenant_policy(&caller);
    let route = format!("POST /v1/repos/{}/branches/{branch}/send", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(response) = replayed(&app, &caller, idem.as_ref()).await? {
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
        creates: Vec::new(),
        branches: vec![branch.clone()],
        cursor,
        locks: vec![branch.clone()],
        idempotency: idem,
        principal: caller.0.clone(),
        quota: app.admission_quota(&caller, &policy),
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
    let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
    let policy = app.tenant_policy(&caller);
    let route = format!("POST /v1/repos/{}/branches/{branch}/fork", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(response) = replayed(&app, &caller, idem.as_ref()).await? {
        return Ok(response);
    }
    app.check_admission_quotas(&caller, &policy).await?;
    let options = work::fork_options(&app, &repo, &request)?;
    existing(&repo.yard, &branch).await?;
    let planned = {
        let (yard, prompt) = (repo.yard.clone(), request.prompt.clone());
        match blocking(move || yard.task(prompt).options(options).planned_names(&[])).await? {
            Ok(planned) => planned,
            Err(e) => return replay_or(&app, &caller, idem.as_ref(), error::sdk(&e)).await,
        }
    };
    let cursor = sync_feed(&repo.feed).await?;
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Fork,
        locks: planned.clone(),
        creates: planned.clone(),
        branches: planned,
        cursor,
        idempotency: idem,
        principal: caller.0.clone(),
        quota: app.admission_quota(&caller, &policy),
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
    let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
    let policy = app.tenant_policy(&caller);
    let route = format!("POST /v1/repos/{}/branches/{branch}/reincarnate", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(response) = replayed(&app, &caller, idem.as_ref()).await? {
        return Ok(response);
    }
    app.check_admission_quotas(&caller, &policy).await?;
    let options = work::reincarnate_options(&app, &repo, &request)?;
    let source = existing(&repo.yard, &branch).await?;
    let planned = {
        let (yard, prompt) = (repo.yard.clone(), source.info().prompt.clone());
        match blocking(move || yard.task(prompt).options(options).planned_names(&[])).await? {
            Ok(planned) => planned,
            Err(e) => return replay_or(&app, &caller, idem.as_ref(), error::sdk(&e)).await,
        }
    };
    let cursor = sync_feed(&repo.feed).await?;
    let new = NewOperation {
        repo: repo.name.clone(),
        kind: OperationKind::Reincarnate,
        locks: planned.clone(),
        creates: planned.clone(),
        branches: planned,
        cursor,
        idempotency: idem,
        principal: caller.0.clone(),
        quota: app.admission_quota(&caller, &policy),
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
    let repo = app.authorized_repo(&caller, &repo, "merge")?.clone();
    let route = format!("POST /v1/repos/{}/branches/{branch}/merge", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(response) = replayed(&app, &caller, idem.as_ref()).await? {
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
        principal: caller.0.clone(),
        creates: Vec::new(),
        quota: AdmissionQuota::default(),
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
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let by = format!("{} through the server", caller.name());
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
    let yard = app.authorized_repo(&caller, &repo, "run")?.yard.clone();
    let by = format!("{} through the server", caller.name());
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
    let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
    let policy = app.tenant_policy(&caller);
    let route = format!("POST /v1/repos/{}/branches/{parent}/spawn", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(response) = replayed(&app, &caller, idem.as_ref()).await? {
        return Ok(response);
    }
    work::spawn_parts(&app, &repo, &request, None)?;
    app.check_admission_quotas(&caller, &policy).await?;
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
                Err(e) => return replay_or(&app, &caller, idem.as_ref(), error::sdk(&e)).await,
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
                Err(e) => return replay_or(&app, &caller, idem.as_ref(), error::sdk(&e)).await,
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
        creates: planned.clone(),
        branches: planned,
        cursor,
        idempotency: idem,
        principal: caller.0.clone(),
        quota: app.admission_quota(&caller, &policy),
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
    let repo = app.authorized_repo(&caller, &repo, "merge")?.clone();
    let route = format!("POST /v1/repos/{}/branches/{branch}/integrate", repo.name);
    let idem = idempotency(&headers, &caller, &route, &canonical)?;
    if let Some(response) = replayed(&app, &caller, idem.as_ref()).await? {
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
        principal: caller.0.clone(),
        creates: Vec::new(),
        quota: AdmissionQuota::default(),
    };
    admit(&app, new, Work::Integrate { branch, parent }).await
}

/// Act as `branch` with the server's authority, as `by inspect`, `by events`
/// and `by children` do outside a harness. `scope` is the caller's
/// required scope for this action; `repo` must be within its tenant (and,
/// if narrower, its own allowlist).
async fn as_person<T: Send + 'static>(
    app: &App,
    caller: &Caller,
    repo: &str,
    branch: String,
    scope: &str,
    work: impl FnOnce(branchyard::Delegate, &str) -> Result<T, branchyard::Error> + Send + 'static,
) -> Result<T, ApiError> {
    let yard = app.authorized_repo(caller, repo, scope)?.yard.clone();
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
    Extension(caller): Extension<Caller>,
) -> Result<Json<branchyard::Inspection>, ApiError> {
    as_person(&app, &caller, &repo, branch, "read", |d, b| d.inspect(b))
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
    Extension(caller): Extension<Caller>,
    RawQuery(query): RawQuery,
) -> Result<Json<branchyard::EventPage>, ApiError> {
    let cursor = query_number(query.as_deref(), "cursor")?;
    let limit = query_number(query.as_deref(), "limit")?.unwrap_or(50);
    as_person(&app, &caller, &repo, branch, "read", move |d, b| {
        d.events(b, cursor, limit)
    })
    .await
    .map(Json)
}

async fn children(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<branchyard::Children>, ApiError> {
    as_person(&app, &caller, &repo, branch, "read", |d, _| d.children())
        .await
        .map(Json)
}

async fn graph(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<branchyard::Graph>, ApiError> {
    as_person(&app, &caller, &repo, branch, "read", |d, b| d.graph(b))
        .await
        .map(Json)
}

/// Apply a graph proposal to `parent`'s children with the server's
/// authority as a person, bounded by the parent's envelope: what
/// `by graph apply --parent` does. Not an operation: the proposal commits
/// in one store transaction before this answers, and the children it
/// starts run on the server's threads, where their siblings' turns start
/// the rest as they settle. A stale `expected_revision` is
/// `409 stale_revision` and changes nothing, which also makes a retried
/// request safe.
async fn post_graph(
    State(app): State<Shared>,
    Path((repo, parent)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<GraphRequest>,
) -> Result<Json<branchyard::GraphApplied>, ApiError> {
    let spawns = request
        .edits
        .iter()
        .any(|edit| matches!(edit, branchyard::GraphEdit::Spawn(_)));
    if spawns && !app.config.allow_delegation {
        return Err(delegation_not_allowed(
            "this server does not offer delegation, so it does not spawn children; its \
             operator can allow it with --allow-delegation",
        ));
    }
    app.opt_ins(false, false, request.unapproved_tools)?;
    let repo = app.authorized_repo(&caller, &repo, "run")?.clone();
    let new_children = request
        .edits
        .iter()
        .filter(|edit| matches!(edit, branchyard::GraphEdit::Spawn(_)))
        .count();
    if new_children > 0 {
        let policy = app.tenant_policy(&caller);
        app.check_admission_quotas(&caller, &policy).await?;
        app.check_graph_branches(&caller, &policy, new_children)
            .await?;
    }
    let options = app.options(
        &repo,
        Budget::default(),
        app.policy(&request.policy, false),
        None,
        None,
        None,
        request.unapproved_tools,
        None,
    );
    let yard = repo.yard.clone();
    blocking(move || {
        yard.branch(&parent)?
            .delegate(options)?
            .apply_graph(request.edits, request.expected_revision)
    })
    .await?
    .map(Json)
    .map_err(|e| error::sdk(&e))
}

async fn inbox(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<branchyard::Inbox>, ApiError> {
    as_person(&app, &caller, &repo, branch, "read", |d, _| d.inbox())
        .await
        .map(Json)
}

/// Blocking a server worker for a long wait is bounded: a caller that wants
/// longer polls `inbox` instead.
const MAX_ASK_WAIT: Duration = Duration::from_secs(120);

async fn post_ask(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<branchyard_client::api::AskRequest>,
) -> Result<Json<branchyard::Asked>, ApiError> {
    if request.text.trim().is_empty() {
        return Err(ApiError::bad_request("text is empty"));
    }
    let wait = request
        .wait_seconds
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(|s| Duration::from_secs_f64(s).min(MAX_ASK_WAIT));
    as_person(&app, &caller, &repo, branch, "run", move |d, _| {
        d.ask(&request.text, wait)
    })
    .await
    .map(Json)
}

async fn post_report(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<branchyard_client::api::TextRequest>,
) -> Result<Json<branchyard::Message>, ApiError> {
    if request.text.trim().is_empty() {
        return Err(ApiError::bad_request("text is empty"));
    }
    as_person(&app, &caller, &repo, branch, "run", move |d, _| {
        d.report(&request.text)
    })
    .await
    .map(Json)
}

async fn post_escalate(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<branchyard_client::api::TextRequest>,
) -> Result<Json<branchyard::Message>, ApiError> {
    if request.text.trim().is_empty() {
        return Err(ApiError::bad_request("text is empty"));
    }
    as_person(&app, &caller, &repo, branch, "run", move |d, _| {
        d.escalate(&request.text)
    })
    .await
    .map(Json)
}

async fn post_answer(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<branchyard_client::api::AnswerRequest>,
) -> Result<Json<branchyard::Message>, ApiError> {
    if request.text.trim().is_empty() {
        return Err(ApiError::bad_request("text is empty"));
    }
    as_person(&app, &caller, &repo, branch, "run", move |d, _| {
        d.answer(request.message_id, &request.text)
    })
    .await
    .map(Json)
}

async fn branches(
    State(app): State<Shared>,
    Path(repo): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<BranchList>, ApiError> {
    let yard = app.authorized_repo(&caller, &repo, "read")?.yard.clone();
    let branches = blocking(move || yard.branches())
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(BranchList { branches }))
}

async fn branch(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<branchyard::BranchInfo>, ApiError> {
    let yard = &app.authorized_repo(&caller, &repo, "read")?.yard;
    Ok(Json(existing(yard, &branch).await?.info().clone()))
}

async fn diff(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Diff>, ApiError> {
    let target = existing(&app.authorized_repo(&caller, &repo, "read")?.yard, &branch).await?;
    let diff = blocking(move || target.diff())
        .await?
        .map_err(|e| error::sdk(&e))?;
    Ok(Json(Diff { diff }))
}

async fn events(
    State(app): State<Shared>,
    Path((repo, branch)): Path<(String, String)>,
    Extension(caller): Extension<Caller>,
    RawQuery(query): RawQuery,
) -> Result<Json<BranchEvents>, ApiError> {
    let cursor = cursor_param(query.as_deref())?.unwrap_or(0);
    let target = existing(&app.authorized_repo(&caller, &repo, "read")?.yard, &branch).await?;
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
    Extension(caller): Extension<Caller>,
) -> Result<Json<Removed>, ApiError> {
    let repo = app.authorized_repo(&caller, &repo, "admin")?.clone();
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
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let repo = app.authorized_repo(&caller, &repo, "read")?;
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
