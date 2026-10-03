//! The fleet's service registry over HTTP (`docs/registry.md`), merged
//! into the main router like [`crate::map_routes`]:
//!
//! - `GET /.well-known/branchyard`, public: what this server is, its API,
//!   its JWKS when it has connectors, and the kinds and capabilities of
//!   the live services (not their endpoints or owners);
//! - `GET /v1/services[?kind=K]` (`read`): every record, workers among
//!   them, with endpoints, owners, health and leases;
//! - `POST /v1/services` (`admin`): register a service, or renew one this
//!   principal registered; such a record never carries anything for the
//!   server to reclaim, so a caller can never make the server stop a
//!   process;
//! - `DELETE /v1/services/{id}` (`admin`): deregister one this principal
//!   registered;
//! - `POST /v1/services/gc` (`admin`): expire and reclaim now, as the
//!   server does on its recovery interval.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use branchyard::services::{self, Service, ServiceOwner, DEFAULT_TTL, MAX_TTL, MIN_TTL};
use branchyard_client::api::{RegisterServiceRequest, ServiceList, ServiceSummary, WellKnown};

use crate::api::{blocking, Caller, JsonBody, Shared};
use crate::error::ApiError;

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/services", get(list).post(register))
        .route("/v1/services/gc", post(gc))
        .route("/v1/services/{id}", axum::routing::delete(deregister))
}

/// The public route, outside `/v1`, needing no token.
pub(crate) fn public() -> Router<Shared> {
    Router::new().route("/.well-known/branchyard", get(well_known))
}

fn io(error: std::io::Error) -> ApiError {
    match error.kind() {
        std::io::ErrorKind::AlreadyExists => {
            ApiError::new(StatusCode::CONFLICT, "service_held", error.to_string())
        }
        std::io::ErrorKind::InvalidInput => ApiError::bad_request(error.to_string()),
        _ => ApiError::internal(error.to_string()),
    }
}

fn need(caller: &Caller, scope: &str) -> Result<(), ApiError> {
    match caller.0.allows(scope) {
        true => Ok(()),
        false => Err(crate::api::scope_required(scope)),
    }
}

/// What a principal's records are owned as: one owner per principal, so
/// any process holding its token renews what it registered.
fn owner_id(caller: &Caller) -> String {
    format!("principal:{}", caller.0.name)
}

/// Every record of the fleet, workers included, with `now`'s view of
/// which are live.
pub(crate) fn fleet(app: &Shared) -> std::io::Result<Vec<Service>> {
    let registry = &app.registry;
    let mut all = registry.services().all()?;
    let within = crate::ops::live_window();
    all.extend(crate::store::worker_services(
        &registry.live_workers()?,
        within,
        crate::ops::now_ms(),
    ));
    all.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.id.cmp(&b.id)));
    Ok(all)
}

async fn well_known(State(app): State<Shared>) -> Result<Json<WellKnown>, ApiError> {
    let shared = app.clone();
    let services = blocking(move || fleet(&shared)).await?.map_err(io)?;
    let now = crate::ops::now_ms();
    Ok(Json(WellKnown {
        service: "branchyard".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        api: "/v1".into(),
        jwks_uri: app
            .config
            .connectors
            .is_some()
            .then(|| "/.well-known/jwks.json".to_owned()),
        services_uri: "/v1/services".into(),
        repos: app.repos.keys().cloned().collect(),
        scopes: crate::config::SCOPES
            .iter()
            .map(|s| s.to_string())
            .collect(),
        services: services
            .into_iter()
            .filter(|s| s.live_at(now))
            .map(|s| ServiceSummary {
                id: s.id,
                kind: s.kind,
                capabilities: s.capabilities,
                health: s.health,
            })
            .collect(),
    }))
}

async fn list(
    State(app): State<Shared>,
    uri: axum::http::Uri,
    Extension(caller): Extension<Caller>,
) -> Result<Json<ServiceList>, ApiError> {
    need(&caller, "read")?;
    let kind = uri.query().and_then(|q| {
        q.split('&')
            .find_map(|pair| pair.strip_prefix("kind="))
            .map(decode)
    });
    let services = blocking(move || fleet(&app)).await?.map_err(io)?;
    Ok(Json(ServiceList {
        services: services
            .into_iter()
            .filter(|s| kind.as_deref().is_none_or(|k| k == s.kind))
            .collect(),
    }))
}

async fn register(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<RegisterServiceRequest>,
) -> Result<Json<Service>, ApiError> {
    need(&caller, "admin")?;
    if request.kind == services::KIND_WORKER {
        return Err(ApiError::bad_request(
            "workers register by claiming work (by worker); they are not registered here",
        ));
    }
    let ttl = request
        .ttl_seconds
        .map_or(DEFAULT_TTL, Duration::from_secs)
        .clamp(MIN_TTL, MAX_TTL);
    let mut owner = ServiceOwner::remote(owner_id(&caller), caller.0.name.clone());
    owner.principal = Some(caller.0.name.clone());
    let mut service = Service::new(&request.kind, owner);
    if let Some(id) = request.id {
        service.id = id;
    }
    service.capabilities = request.capabilities;
    service.endpoints = request.endpoints;
    service.health = request.health;
    service.weight = request.weight.unwrap_or(1);
    // Never anything to reclaim: the server stops only what it started.
    service.reclaim = None;
    let now = crate::ops::now_ms();
    service.lease_until_ms = now + ttl.as_millis() as u64;
    let stored = blocking(move || app.registry.services().register(&service, now))
        .await?
        .map_err(io)?;
    Ok(Json(stored))
}

async fn deregister(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Service>, ApiError> {
    need(&caller, "admin")?;
    let owner = owner_id(&caller);
    let (left, found) = blocking(move || {
        let store = app.registry.services();
        let left = store.deregister(&id, &owner, crate::ops::now_ms())?;
        Ok::<_, std::io::Error>((left, store.get(&id)?))
    })
    .await?
    .map_err(io)?;
    match (left, found) {
        (true, Some(service)) => Ok(Json(service)),
        (_, Some(_)) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "service_held",
            "the service is not live, or another principal registered it",
        )),
        (_, None) => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such service",
        )),
    }
}

async fn gc(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<ServiceList>, ApiError> {
    need(&caller, "admin")?;
    let reaped = blocking(move || reap(&app)).await?.map_err(io)?;
    Ok(Json(ServiceList {
        services: reaped.into_iter().map(|r| r.service).collect(),
    }))
}

/// Expire and reclaim the fleet's records: a process this host started
/// is stopped (pid and start time checked); a record with nothing to
/// reclaim, such as one registered over the API, is only marked.
pub(crate) fn reap(app: &Shared) -> std::io::Result<Vec<services::Reaped>> {
    let reaper = |_: &Service, reclaim: &services::Reclaim| services::reclaim_process(reclaim);
    services::reap(app.registry.services(), crate::ops::now_ms(), &reaper)
}

/// A query parameter with its `%XX` escapes decoded.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).copied(),
            bytes.get(i + 2).copied(),
        ) {
            (b'%', Some(h), Some(l)) if hex(h).is_some() && hex(l).is_some() => {
                out.push((hex(h).unwrap_or(0) * 16 + hex(l).unwrap_or(0)) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
