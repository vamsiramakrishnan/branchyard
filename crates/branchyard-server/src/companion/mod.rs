//! The web companion (`docs/companion.md`): a single page served at
//! `/app/` when the operator turns it on (`--app`, `"app": true`), pairing
//! links that turn a one-time code into a scoped, expiring token, and Web
//! Push notifications.
//!
//! The page is static and embedded in the binary; it holds no data of its
//! own and reaches the server only through the API every other client
//! uses, with a bearer token, under that token's scopes. What is new on
//! the server is small:
//!
//! - `GET /app/...` serves the page's files with a strict Content Security
//!   Policy, without a token (they contain nothing but code).
//! - `POST /app/pair` redeems a pairing code: single use (the code's row is
//!   deleted in the transaction that records the token), short-lived,
//!   rate-limited per server, never logged (it travels in the link's
//!   fragment and the request body, which no log records). The token it
//!   returns is stored hashed like every credential, expires, and is
//!   revocable with `by serve token revoke`.
//! - `GET /v1/app/me` says who the caller is; `/v1/app/push...` manages the
//!   caller's own push subscriptions (`read` scope).

pub mod link;
pub mod push;
pub mod qr;
#[cfg(test)]
mod qr_fixtures;
pub mod store;

use branchyard_support::LockExt as _;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use branchyard_client::companion::{
    Me, PairRequest, Paired, PushInfo, PushResult, PushSubscribe, PushTest, PushUnsubscribe,
};
use futures_util::StreamExt;
use ring::rand::{SecureRandom, SystemRandom};

use crate::api::{blocking, App, Caller, JsonBody, Shared};
use crate::config::{sha256_hex, Principal};
use crate::error::ApiError;
use store::{CompanionStore, Subscription};

/// The policy every file of the page is served with: its own scripts,
/// styles, images and manifest only; requests only to this server; no
/// inline script or style, no plugins, no framing, no form posts, no
/// `<base>`.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
    img-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; \
    base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Redemption attempts per server per window; more are refused `429`.
pub const PAIR_ATTEMPTS: usize = 10;
pub const PAIR_WINDOW: Duration = Duration::from_secs(60);
/// How often a paired caller's event stream checks its token is still
/// good (it also ends at the token's expiry).
const RECHECK: Duration = Duration::from_secs(10);

/// The page's files: path under `/app/`, content type, bytes.
const ASSETS: &[(&str, &str, &str)] = &[
    (
        "",
        "text/html; charset=utf-8",
        include_str!("assets/index.html"),
    ),
    (
        "index.html",
        "text/html; charset=utf-8",
        include_str!("assets/index.html"),
    ),
    (
        "app.js",
        "text/javascript; charset=utf-8",
        include_str!("assets/app.js"),
    ),
    (
        "app.css",
        "text/css; charset=utf-8",
        include_str!("assets/app.css"),
    ),
    (
        "sw.js",
        "text/javascript; charset=utf-8",
        include_str!("assets/sw.js"),
    ),
    (
        "manifest.webmanifest",
        "application/manifest+json",
        include_str!("assets/manifest.webmanifest"),
    ),
    ("icon.svg", "image/svg+xml", include_str!("assets/icon.svg")),
];

/// The companion's state in a running server.
pub struct Companion {
    pub store: Arc<dyn CompanionStore>,
    /// `None` when push is off.
    pub vapid: Option<push::Vapid>,
    /// The HTTP client pushes are sent with; `None` when push is off.
    pub client: Option<reqwest::Client>,
    attempts: Mutex<VecDeque<Instant>>,
}

impl Companion {
    /// Open the store and, with push on, the VAPID key. Blocking.
    pub fn open(config: &crate::Config) -> Result<Companion, String> {
        let store = store::open(config)?;
        let mut vapid = match config.app.push {
            true => Some(push::Vapid::load_or_create(
                &config.app.vapid_key_path(&config.data_dir),
                config.app.subject(config),
            )?),
            false => None,
        };
        // A client that cannot be built (no readable trust roots, say)
        // turns push off; the page and pairing still work.
        let client = match vapid {
            Some(_) => match reqwest::Client::builder().build() {
                Ok(client) => Some(client),
                Err(error) => {
                    tracing::warn!(%error, "companion: push is off: no HTTP client");
                    vapid = None;
                    None
                }
            },
            None => None,
        };
        Ok(Companion {
            store,
            vapid,
            client,
            attempts: Mutex::new(VecDeque::new()),
        })
    }

    /// Count a redemption attempt; false when over the limit.
    #[allow(clippy::expect_used)] // ratchet: branchyard-server
    fn attempt(&self, now: Instant) -> Result<(), Duration> {
        let mut attempts = self.attempts.lock_recovering("attempts");
        while attempts
            .front()
            .is_some_and(|t| now.duration_since(*t) >= PAIR_WINDOW)
        {
            attempts.pop_front();
        }
        if attempts.len() >= PAIR_ATTEMPTS {
            let oldest = *attempts.front().expect("full");
            return Err(PAIR_WINDOW.saturating_sub(now.duration_since(oldest)));
        }
        attempts.push_back(now);
        Ok(())
    }
}

/// How the caller authenticated, beside its [`Caller`]: the hash of its
/// token, and for a paired token when it expires.
#[derive(Clone, Debug)]
pub struct Verified {
    pub token_sha256: String,
    pub expires_at_ms: Option<u64>,
}

fn bearer(header: Option<&str>) -> Option<&str> {
    let presented = header?.strip_prefix("Bearer ")?.trim();
    (!presented.is_empty()).then_some(presented)
}

/// The [`Verified`] of a configured credential that matched `header`.
pub(crate) fn configured(header: Option<&str>) -> Verified {
    Verified {
        token_sha256: bearer(header)
            .map(|t| sha256_hex(t.as_bytes()))
            .unwrap_or_default(),
        expires_at_ms: None,
    }
}

/// Requests that need no token: the page's files, and redeeming a code.
pub(crate) fn is_public(app: &App, method: &Method, path: &str) -> bool {
    if app.companion.is_none() {
        return false;
    }
    // Exactly the page's own files: anything else under /app/ needs a
    // token like every other path, so nothing can be probed anonymously.
    let page = path == "/app"
        || path
            .strip_prefix("/app/")
            .is_some_and(|file| ASSETS.iter().any(|(p, _, _)| *p == file));
    match *method {
        Method::GET | Method::HEAD => page,
        Method::POST => path == "/app/pair",
        _ => false,
    }
}

/// The principal a paired token in `header` verifies as, if any.
pub(crate) async fn verify_paired(
    app: &Shared,
    header: Option<&str>,
) -> Option<(Principal, Verified)> {
    let companion = app.companion.clone()?;
    let hash = sha256_hex(bearer(header)?.as_bytes());
    let lookup = hash.clone();
    let token = tokio::task::spawn_blocking(move || companion.store.token(&lookup))
        .await
        .ok()?
        .ok()??;
    if !token.usable_at(branchyard_support::time::now_ms()) {
        return None;
    }
    Some((
        token.principal,
        Verified {
            token_sha256: hash,
            expires_at_ms: Some(token.expires_at_ms),
        },
    ))
}

/// For a paired caller, end an event stream when its token expires or is
/// revoked, rather than letting an open stream outlive the credential.
pub(crate) fn bound(app: &Shared, verified: &Verified, response: Response) -> Response {
    let Some(expires_at_ms) = verified.expires_at_ms else {
        return response;
    };
    let streaming = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let Some(companion) = app.companion.clone().filter(|_| streaming) else {
        return response;
    };
    let hash = verified.token_sha256.clone();
    let ended = async move {
        loop {
            let now = branchyard_support::time::now_ms();
            if now >= expires_at_ms {
                return;
            }
            let wait = Duration::from_millis(expires_at_ms - now).min(RECHECK);
            tokio::time::sleep(wait).await;
            let (store, hash) = (companion.store.clone(), hash.clone());
            let still = tokio::task::spawn_blocking(move || store.token(&hash))
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .is_some_and(|t| t.usable_at(branchyard_support::time::now_ms()));
            if !still {
                return;
            }
        }
    };
    let (parts, body) = response.into_parts();
    let body = axum::body::Body::from_stream(body.into_data_stream().take_until(Box::pin(ended)));
    Response::from_parts(parts, body)
}

/// The companion's routes, merged into the API's router. Each answers as
/// an unknown route when the companion is off.
pub fn router() -> Router<Shared> {
    Router::new()
        .route("/app", get(redirect))
        .route("/app/", get(asset))
        .route("/app/pair", post(pair))
        .route("/app/{file}", get(asset))
        .route("/v1/app/me", get(me))
        .route("/v1/app/push", get(push_info))
        .route(
            "/v1/app/push/subscriptions",
            post(subscribe).delete(unsubscribe),
        )
        .route("/v1/app/push/test", post(push_test))
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route")
}

fn enabled(app: &App) -> Result<Arc<Companion>, ApiError> {
    app.companion.clone().ok_or_else(not_found)
}

fn security_headers(headers: &mut HeaderMap) {
    for (name, value) in [
        ("content-security-policy", CSP),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

async fn redirect(State(app): State<Shared>) -> Response {
    if app.companion.is_none() {
        return not_found().into_response();
    }
    let mut response = StatusCode::PERMANENT_REDIRECT.into_response();
    response
        .headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/app/"));
    response
}

async fn asset(State(app): State<Shared>, request: Request) -> Response {
    if app.companion.is_none() {
        return not_found().into_response();
    }
    let path = request.uri().path().trim_start_matches("/app/");
    let Some((_, content_type, text)) = ASSETS.iter().find(|(p, _, _)| *p == path) else {
        return not_found().into_response();
    };
    let etag = format!("\"{}\"", &sha256_hex(text.as_bytes())[..32]);
    let fresh = request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    let mut response = match fresh {
        true => StatusCode::NOT_MODIFIED.into_response(),
        false => ([(header::CONTENT_TYPE, *content_type)], *text).into_response(),
    };
    let headers = response.headers_mut();
    security_headers(headers);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    response
}

/// A device label: printable, at most 80 characters.
fn device_label(text: Option<String>) -> Option<String> {
    let label: String = text?.chars().filter(|c| !c.is_control()).take(80).collect();
    let label = label.trim().to_owned();
    (!label.is_empty()).then_some(label)
}

pub(crate) fn me_of(principal: &Principal, verified: Option<&Verified>) -> Me {
    let expires_at_ms = verified.and_then(|v| v.expires_at_ms);
    Me {
        name: principal.name.clone(),
        tenant: principal.tenant.clone(),
        scopes: principal.scopes.iter().cloned().collect(),
        repos: principal
            .repos
            .as_ref()
            .map(|r| r.iter().cloned().collect()),
        kind: match expires_at_ms {
            Some(_) => "paired".into(),
            None => "configured".into(),
        },
        expires_at_ms,
    }
}

/// A fresh random secret: `bytes` from the system's generator, as hex.
pub(crate) fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| "no randomness available".to_owned())?;
    Ok(hex::encode(buf))
}

/// `POST /app/pair`.
async fn pair(
    State(app): State<Shared>,
    JsonBody(request, _): JsonBody<PairRequest>,
) -> Result<Response, ApiError> {
    let companion = enabled(&app)?;
    if let Err(wait) = companion.attempt(Instant::now()) {
        let seconds = wait.as_secs().max(1);
        let mut response = ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            format!("too many pairing attempts; try again in {seconds} seconds"),
        )
        .detail(serde_json::json!({ "retry_after_seconds": seconds }))
        .into_response();
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return Ok(response);
    }
    let refused = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_pairing_code",
            "this pairing link is unknown, already used or expired; ask for a new one \
             (by serve token new --link)",
        )
    };
    let code = request.code.trim().to_ascii_lowercase();
    if code.is_empty() || code.len() > 128 {
        return Err(refused());
    }
    let token = random_hex(32).map_err(ApiError::internal)?;
    let (code_hash, token_hash) = (sha256_hex(code.as_bytes()), sha256_hex(token.as_bytes()));
    let device = device_label(request.device);
    let store = companion.store.clone();
    let redeemed = blocking(move || {
        store.redeem(
            &code_hash,
            &token_hash,
            device.as_deref(),
            branchyard_support::time::now_ms(),
        )
    })
    .await?
    .map_err(|e| ApiError::internal(e.to_string()))?;
    let Some(paired) = redeemed else {
        return Err(refused());
    };
    tracing::info!(
        name = %paired.principal.name,
        tenant = %paired.principal.tenant,
        "companion: a pairing link was redeemed"
    );
    let verified = Verified {
        token_sha256: paired.token_sha256.clone(),
        expires_at_ms: Some(paired.expires_at_ms),
    };
    let me = me_of(&paired.principal, Some(&verified));
    let mut response = Json(Paired { token, me }).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// `GET /v1/app/me`.
async fn me(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    verified: Option<Extension<Verified>>,
) -> Result<Json<Me>, ApiError> {
    enabled(&app)?;
    Ok(Json(me_of(&caller.0, verified.as_ref().map(|v| &v.0))))
}

fn read_scope(caller: &Caller) -> Result<(), ApiError> {
    match caller.0.allows("read") {
        true => Ok(()),
        false => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "scope_required",
            "this request needs the read scope",
        )
        .detail(serde_json::json!({ "scope": "read" }))),
    }
}

async fn own_subscriptions(
    companion: &Arc<Companion>,
    hash: &str,
) -> Result<Vec<Subscription>, ApiError> {
    let (store, hash) = (companion.store.clone(), hash.to_owned());
    blocking(move || store.subscriptions())
        .await?
        .map(|all| all.into_iter().filter(|s| s.token_sha256 == hash).collect())
        .map_err(|e| ApiError::internal(e.to_string()))
}

/// `GET /v1/app/push`.
async fn push_info(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Extension(verified): Extension<Verified>,
) -> Result<Json<PushInfo>, ApiError> {
    let companion = enabled(&app)?;
    read_scope(&caller)?;
    let mine = own_subscriptions(&companion, &verified.token_sha256).await?;
    Ok(Json(PushInfo {
        enabled: companion.vapid.is_some(),
        public_key: companion.vapid.as_ref().map(push::Vapid::public_key),
        services: app.config.app.push_services.clone(),
        subscriptions: mine.into_iter().map(|s| s.endpoint).collect(),
        kinds: push::KINDS.iter().map(|k| k.to_string()).collect(),
    }))
}

/// `POST /v1/app/push/subscriptions`.
async fn subscribe(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Extension(verified): Extension<Verified>,
    JsonBody(request, _): JsonBody<PushSubscribe>,
) -> Result<Json<PushResult>, ApiError> {
    let companion = enabled(&app)?;
    read_scope(&caller)?;
    if companion.vapid.is_none() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "push_not_enabled",
            "this server does not send push notifications (app.push is off)",
        ));
    }
    push::endpoint_allowed(&request.endpoint, &app.config.app.push_services)
        .map_err(ApiError::bad_request)?;
    if request.endpoint.len() > 2048 {
        return Err(ApiError::bad_request("the endpoint is too long"));
    }
    let key = push::decode_b64(&request.keys.p256dh)
        .map_err(|e| ApiError::bad_request(format!("keys.p256dh: {e}")))?;
    if key.len() != 65 || key[0] != 4 {
        return Err(ApiError::bad_request(
            "keys.p256dh is not an uncompressed P-256 point",
        ));
    }
    let auth = push::decode_b64(&request.keys.auth)
        .map_err(|e| ApiError::bad_request(format!("keys.auth: {e}")))?;
    if auth.len() != 16 {
        return Err(ApiError::bad_request("keys.auth is not 16 bytes"));
    }
    for kind in &request.kinds {
        push::check_kind(kind).map_err(ApiError::bad_request)?;
    }
    let subscription = Subscription {
        endpoint: request.endpoint,
        token_sha256: verified.token_sha256.clone(),
        p256dh: request.keys.p256dh,
        auth: request.keys.auth,
        kinds: request.kinds,
        created_at_ms: branchyard_support::time::now_ms(),
    };
    let store = companion.store.clone();
    blocking(move || store.subscribe(&subscription))
        .await?
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let mine = own_subscriptions(&companion, &verified.token_sha256).await?;
    Ok(Json(PushResult {
        subscriptions: mine.len(),
        ..PushResult::default()
    }))
}

/// `DELETE /v1/app/push/subscriptions`: only the caller's own.
async fn unsubscribe(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Extension(verified): Extension<Verified>,
    JsonBody(request, _): JsonBody<PushUnsubscribe>,
) -> Result<Json<PushResult>, ApiError> {
    let companion = enabled(&app)?;
    read_scope(&caller)?;
    let (store, hash) = (companion.store.clone(), verified.token_sha256.clone());
    blocking(move || store.unsubscribe(&request.endpoint, Some(&[hash])))
        .await?
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let mine = own_subscriptions(&companion, &verified.token_sha256).await?;
    Ok(Json(PushResult {
        subscriptions: mine.len(),
        ..PushResult::default()
    }))
}

/// `POST /v1/app/push/test`: a notification to the caller's own
/// subscriptions, about the first repository it may read.
async fn push_test(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Extension(verified): Extension<Verified>,
    JsonBody(_, _): JsonBody<PushTest>,
) -> Result<Json<PushResult>, ApiError> {
    let companion = enabled(&app)?;
    read_scope(&caller)?;
    let Some(repo) = app.visible_repos(&caller).next().map(|r| r.name.clone()) else {
        return Err(ApiError::bad_request(
            "this principal can read no repository",
        ));
    };
    let notice = push::Notice {
        kind: "finished",
        repo: repo.clone(),
        branch: String::new(),
        title: "Branchyard".into(),
        body: format!("Notifications from {repo} reach this device."),
        url: "#/".into(),
        tag: "test".into(),
    };
    let only = [verified.token_sha256.clone()];
    let (delivered, failures) = push::fan_out(&app, &notice, Some(&only)).await;
    let mine = own_subscriptions(&companion, &verified.token_sha256).await?;
    Ok(Json(PushResult {
        subscriptions: mine.len(),
        delivered,
        failures,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_limit_counts_attempts_in_its_window() {
        let companion = Companion {
            store: Arc::new(store::SqliteCompanion::memory()),
            vapid: None,
            client: None,
            attempts: Mutex::new(VecDeque::new()),
        };
        let start = Instant::now();
        for _ in 0..PAIR_ATTEMPTS {
            companion.attempt(start).unwrap();
        }
        let wait = companion.attempt(start).unwrap_err();
        assert!(wait <= PAIR_WINDOW && wait > Duration::ZERO);
        companion.attempt(start + PAIR_WINDOW).unwrap();
    }

    #[test]
    fn every_asset_is_self_contained() {
        for (path, _, text) in ASSETS {
            // Nothing is loaded from elsewhere: no absolute URL but the SVG
            // namespace, no protocol-relative one, no CSS import.
            let without_ns = text.replace("http://www.w3.org/2000/svg", "");
            for external in ["http://", "https://", "\"//", "'//", "url(", "@import"] {
                assert!(!without_ns.contains(external), "{path} mentions {external}");
            }
            assert!(!text.contains("<script>"), "{path} has an inline script");
            assert!(!text.contains(" style=\""), "{path} has an inline style");
            assert!(
                !text.contains("innerHTML"),
                "{path} writes HTML from strings"
            );
            assert!(!text.contains("eval("), "{path} evaluates strings");
        }
        assert!(CSP.contains("script-src 'self'") && !CSP.contains("unsafe"));
        assert_eq!(
            device_label(Some(" Pixel\u{7}  ".into())).as_deref(),
            Some("Pixel")
        );
        assert_eq!(device_label(Some("\n".into())), None);
    }
}
