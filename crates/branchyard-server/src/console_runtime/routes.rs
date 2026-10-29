//! HTTP adapters; all placement and permission decisions remain in the gateway.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, RawQuery};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use branchyard::workspace_runtime::RuntimeAction;
use branchyard_support::LockExt as _;
use serde::{Deserialize, Serialize};

use super::{OpenTerminal, RuntimeAccess, RuntimeGateway, RuntimeStatus, TerminalHandle};
use crate::api::{blocking, Caller};
use crate::config::sha256_hex;
use crate::error::{self, ApiError};

/// Mount workspace runtime routes behind the server's existing authentication.
///
/// No central application state is required. The generic state permits merging
/// into the embedding server's router. Active app content is deliberately absent;
/// it requires a separate preview origin and its own scoped authorization.
pub fn router<S>(gateway: Arc<RuntimeGateway>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/v1/workspaces/{workspace}/sessions/{session}/runtime",
            get(status),
        )
        .route(
            "/v1/workspaces/{workspace}/sessions/{session}/runtime/actions/{action}",
            post(control),
        )
        .route(
            "/v1/workspaces/{workspace}/sessions/{session}/terminals",
            post(open),
        )
        .route(
            "/v1/workspaces/{workspace}/sessions/{session}/terminals/{terminal}/output",
            get(output),
        )
        .route(
            "/v1/workspaces/{workspace}/sessions/{session}/terminals/{terminal}/input",
            post(input),
        )
        .route(
            "/v1/workspaces/{workspace}/sessions/{session}/terminals/{terminal}/resize",
            post(resize),
        )
        .route(
            "/v1/workspaces/{workspace}/sessions/{session}/terminals/{terminal}/close",
            post(close),
        )
        .layer(DefaultBodyLimit::max(24 * 1024))
        .layer(Extension(gateway))
}

fn caller(value: Option<Extension<Caller>>) -> Result<Caller, ApiError> {
    value
        .map(|value| value.0)
        .ok_or_else(ApiError::unauthorized)
}

fn json<T>(value: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    value.map(|value| value.0).map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "terminal JSON must fit within 24 KiB",
            )
        } else {
            ApiError::bad_request(error.body_text())
        }
    })
}

fn private(value: impl Serialize) -> Response {
    let mut response = Json(value).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn key(headers: &HeaderMap) -> Result<String, ApiError> {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|key| {
            !key.is_empty() && key.len() <= 255 && key.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .map(str::to_owned)
        .ok_or_else(|| {
            ApiError::bad_request("Idempotency-Key must be 1 to 255 visible ASCII characters")
        })
}

async fn status(
    Extension(gateway): Extension<Arc<RuntimeGateway>>,
    authenticated: Option<Extension<Caller>>,
    Path((workspace, session)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let caller = caller(authenticated)?;
    let status = blocking(move || {
        let target = gateway.authorize(&caller, &workspace, &session, RuntimeAccess::Inspect)?;
        let runtime = target
            .yard
            .workspace_runtime_state(&target.branch)
            .map_err(|error| error::sdk(&error))?;
        let terminal_reason = if let Err(error) =
            gateway.authorize(&caller, &workspace, &session, RuntimeAccess::Terminal)
        {
            Some(error.body.message)
        } else if let Some(reason) = &runtime.reason {
            Some(reason.clone())
        } else if runtime.runtime.remote && runtime.runtime.allocation.is_none() {
            Some("this workspace has no retained remote allocation".into())
        } else if !matches!(
            runtime.state.as_str(),
            "local" | "running" | "paused" | "stopped"
        ) {
            Some(format!("the execution provider reports {}", runtime.state))
        } else if target
            .yard
            .branch(&target.branch)
            .map_err(|error| error::sdk(&error))?
            .info()
            .status
            == branchyard::BranchStatus::Running
        {
            Some("wait for the native turn to finish before opening a manual writer".into())
        } else if gateway
            .slots
            .lock_recovering("workspace terminals")
            .values()
            .any(|slot| {
                slot.meta.workspace == workspace
                    && slot.meta.session == session
                    && slot
                        .entry
                        .as_ref()
                        .is_none_or(|entry| !entry.terminal.is_closed())
            })
        {
            Some("an existing terminal already owns this workspace's manual writer lease".into())
        } else {
            None
        };
        Ok::<_, ApiError>(RuntimeStatus {
            runtime,
            terminal_available: terminal_reason.is_none(),
            terminal_reason,
            preserves_sources: true,
            preserves_deliverables: true,
            terminal_persistent: false,
        })
    })
    .await??;
    Ok(private(status))
}

async fn control(
    Extension(gateway): Extension<Arc<RuntimeGateway>>,
    authenticated: Option<Extension<Caller>>,
    Path((workspace, session, action)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let caller = caller(authenticated)?;
    let action = match action.as_str() {
        "pause" => RuntimeAction::Pause,
        "suspend" => RuntimeAction::Suspend,
        "resume" => RuntimeAction::Resume,
        "destroy" => RuntimeAction::Destroy,
        _ => {
            return Err(ApiError::bad_request(
                "runtime action must be pause, suspend, resume, or destroy",
            ))
        }
    };
    let key = key(&headers)?;
    let operation = blocking(move || {
        gateway.authorize(
            &caller,
            &workspace,
            &session,
            RuntimeAccess::Control(action),
        )?;
        let submit = gateway.lifecycle.as_ref().ok_or_else(|| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "runtime_control_unavailable",
                "the server has no durable runtime-control queue configured",
            )
        })?;
        submit(&caller, &workspace, &session, action, &key)
    })
    .await??;
    let mut response = private(operation);
    *response.status_mut() = StatusCode::ACCEPTED;
    Ok(response)
}

async fn open(
    Extension(gateway): Extension<Arc<RuntimeGateway>>,
    authenticated: Option<Extension<Caller>>,
    Path((workspace, session)): Path<(String, String)>,
    headers: HeaderMap,
    request: Result<Json<OpenTerminal>, JsonRejection>,
) -> Result<Response, ApiError> {
    let caller = caller(authenticated)?;
    let key = key(&headers)?;
    let request = json(request)?;
    let handle: TerminalHandle =
        blocking(move || gateway.open(caller, workspace, session, request, key)).await??;
    Ok(private(handle))
}

#[derive(Default)]
struct OutputQuery {
    after: u64,
    limit: Option<usize>,
}

impl OutputQuery {
    fn parse(raw: Option<&str>) -> Result<Self, ApiError> {
        if raw.is_some_and(|raw| raw.len() > 256) {
            return Err(ApiError::bad_request("terminal output query is too long"));
        }
        let mut url = reqwest::Url::parse("http://query.invalid/")
            .map_err(|error| ApiError::internal(error.to_string()))?;
        url.set_query(raw);
        let mut after = None;
        let mut limit = None;
        for (name, value) in url.query_pairs() {
            match name.as_ref() {
                "after" if after.is_none() => {
                    after = Some(value.parse().map_err(|_| {
                        ApiError::bad_request("after must be an unsigned byte cursor")
                    })?);
                }
                "limit" if limit.is_none() => {
                    limit = Some(value.parse().map_err(|_| {
                        ApiError::bad_request("limit must be an unsigned byte count")
                    })?);
                }
                _ => {
                    return Err(ApiError::bad_request(
                        "terminal output accepts after and limit once each",
                    ))
                }
            }
        }
        Ok(Self {
            after: after.unwrap_or(0),
            limit,
        })
    }
}

#[derive(Serialize)]
struct OutputPage {
    cursor: u64,
    data: String,
    encoding: &'static str,
    truncated: bool,
    closed: bool,
    error: Option<String>,
}

async fn output(
    Extension(gateway): Extension<Arc<RuntimeGateway>>,
    authenticated: Option<Extension<Caller>>,
    Path((workspace, session, terminal)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let caller = caller(authenticated)?;
    let query = OutputQuery::parse(query.as_deref())?;
    let page = blocking(move || {
        let entry = gateway.entry(&caller, &workspace, &session, &terminal)?;
        let page = entry
            .terminal
            .read_after(query.after, query.limit.unwrap_or(65536))
            .map_err(|error| error::sdk(&error))?;
        Ok::<_, ApiError>(OutputPage {
            cursor: page.cursor,
            data: STANDARD.encode(page.data),
            encoding: "base64",
            truncated: page.truncated,
            closed: page.closed,
            error: page.error,
        })
    })
    .await??;
    Ok(private(page))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    sequence: u64,
    data: String,
}

#[derive(Serialize)]
struct InputAck {
    sequence: u64,
    next_sequence: u64,
}

async fn input(
    Extension(gateway): Extension<Arc<RuntimeGateway>>,
    authenticated: Option<Extension<Caller>>,
    Path((workspace, session, terminal)): Path<(String, String, String)>,
    request: Result<Json<Input>, JsonRejection>,
) -> Result<Response, ApiError> {
    let caller = caller(authenticated)?;
    let request = json(request)?;
    let data = STANDARD
        .decode(request.data)
        .map_err(|_| ApiError::bad_request("terminal data must be standard base64"))?;
    if data.len() > 16384 {
        return Err(ApiError::bad_request(
            "terminal input must be at most 16 KiB",
        ));
    }
    let ack = blocking(move || {
        let entry = gateway.entry(&caller, &workspace, &session, &terminal)?;
        let mut input = entry.input.lock_recovering("terminal input sequence");
        let next = input.last.as_ref().map_or(1, |(sequence, _)| sequence.saturating_add(1));
        let digest = sha256_hex(&data);
        if input.uncertain {
            return Err(ApiError::new(StatusCode::CONFLICT, "terminal_input_uncertain",
                "previous terminal input has an unknown outcome; inspect the workspace and open a new terminal"));
        }
        if input.last.as_ref().is_some_and(|(sequence, hash)| *sequence == request.sequence && hash == &digest) {
            return Ok(InputAck { sequence: request.sequence, next_sequence: next });
        }
        if request.sequence != next || request.sequence == u64::MAX {
            return Err(ApiError::new(StatusCode::CONFLICT, "terminal_input_sequence", "terminal input is out of order or its sequence was reused")
                .detail(serde_json::json!({"next_sequence":next})));
        }
        if let Err(error) = entry.terminal.write(&data) {
            input.uncertain = true;
            entry.terminal.close();
            return Err(error::sdk(&error));
        }
        input.last = Some((request.sequence, digest));
        Ok(InputAck { sequence: request.sequence, next_sequence: request.sequence + 1 })
    }).await??;
    Ok(private(ack))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resize {
    rows: u16,
    columns: u16,
}

async fn resize(
    Extension(gateway): Extension<Arc<RuntimeGateway>>,
    authenticated: Option<Extension<Caller>>,
    Path((workspace, session, terminal)): Path<(String, String, String)>,
    request: Result<Json<Resize>, JsonRejection>,
) -> Result<Response, ApiError> {
    let caller = caller(authenticated)?;
    let request = json(request)?;
    blocking(move || {
        gateway
            .entry(&caller, &workspace, &session, &terminal)?
            .terminal
            .resize(request.rows, request.columns)
            .map_err(|error| error::sdk(&error))
    })
    .await??;
    Ok(private(serde_json::json!({"ok":true})))
}

async fn close(
    Extension(gateway): Extension<Arc<RuntimeGateway>>,
    authenticated: Option<Extension<Caller>>,
    Path((workspace, session, terminal)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let caller = caller(authenticated)?;
    blocking(move || {
        gateway
            .entry(&caller, &workspace, &session, &terminal)?
            .terminal
            .close();
        Ok::<_, ApiError>(())
    })
    .await??;
    Ok(private(serde_json::json!({"ok":true})))
}
