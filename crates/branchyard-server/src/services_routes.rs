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
        branchyard_support::time::now_ms(),
    ));
    all.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.id.cmp(&b.id)));
    Ok(all)
}

async fn well_known(State(app): State<Shared>) -> Result<Json<WellKnown>, ApiError> {
    let shared = app.clone();
    let services = blocking(move || fleet(&shared)).await?.map_err(io)?;
    let now = branchyard_support::time::now_ms();
    Ok(Json(WellKnown {
        service: "branchyard".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        api: "/v1".into(),
        jwks_uri: (app.config.connectors.is_some() || crate::models::signs_for_aiplex(&app.config))
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
            .map(branchyard_client::http::decode)
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
    let now = branchyard_support::time::now_ms();
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
        let left = store.deregister(&id, &owner, branchyard_support::time::now_ms())?;
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
    services::reap(
        app.registry.services(),
        branchyard_support::time::now_ms(),
        &reaper,
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use axum::Extension;
    use branchyard::services::{conformance, Service};
    use tower::ServiceExt as _;

    use super::*;
    use crate::api::{App, Caller};
    use crate::config::{Config, Principal};

    fn app(directory: &tempfile::TempDir) -> Shared {
        let config = Config::new(PathBuf::from(directory.path()).join("data"));
        Arc::new(App {
            preview: Arc::new(
                crate::preview::PreviewService::new(directory.path().join("previews")).unwrap(),
            ),
            runtime: Default::default(),
            pull_requests: Default::default(),
            repos: Default::default(),
            registry: crate::ops::Registry::open(
                Box::new(crate::store::MemoryStore::default()),
                crate::ops::Options::new(Vec::new()),
            )
            .unwrap(),
            credentials: crate::auth::Credentials::new(config.all_credentials()),
            shutdown: tokio::sync::watch::channel(false).1,
            storage_idem: Default::default(),
            triggers: Arc::new(crate::triggers::dispatch::Hub::new(
                Arc::new(crate::triggers::store::SqliteTriggers::memory()),
                config.triggers.clone(),
                "http://127.0.0.1".into(),
            )),
            seen: Default::default(),
            companion: None,
            sync: None,
            config,
        })
    }

    /// `GET /v1/services?kind=...` decodes its query value: `%XX` escapes
    /// become bytes (either hex case), a `%` that is not followed by two hex
    /// digits stays literal, and bytes that are not UTF-8 become U+FFFD. The
    /// stored kinds are ASCII, so each case is observed by which of them the
    /// decoded value matches. A kind cannot hold `+`, so that `+` stays
    /// literal is pinned in `branchyard-client`'s `decoding_handles_the_edges`.
    #[tokio::test]
    async fn the_kind_query_decodes_percent_escapes() {
        let directory = tempfile::tempdir().unwrap();
        let app = app(&directory);
        let now = branchyard_support::time::now_ms();
        for kind in ["a", "ab", "a.b", "a:b", "model_gateway"] {
            let service = Service::new(kind, conformance::owner(&format!("owner-{kind}")));
            app.registry.services().register(&service, now).unwrap();
        }
        let cases: [(&str, &[&str]); 8] = [
            ("model%5Fgateway", &["model_gateway"]),
            ("a%2Eb", &["a.b"]),
            ("a%3ab", &["a:b"]),
            // A lone `%`, or one not followed by two hex digits, is literal:
            // it neither matches `a` or `ab` nor drops the bytes after it.
            ("a%b", &[]),
            ("a%2", &[]),
            ("a%G1", &[]),
            // Invalid UTF-8 becomes U+FFFD, so `ab` does not match.
            ("ab%FF", &[]),
            ("%", &[]),
        ];
        for (query, expected) in cases {
            let caller = Caller::new(Principal::default_for("reader"));
            let response = router()
                .layer(Extension(caller))
                .with_state(app.clone())
                .oneshot(
                    Request::builder()
                        .uri(format!("/v1/services?kind={query}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "kind={query}");
            let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let mut kinds: Vec<&str> = body["services"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["kind"].as_str().unwrap())
                .collect();
            kinds.sort_unstable();
            assert_eq!(kinds, expected, "kind={query}");
        }
    }
}
