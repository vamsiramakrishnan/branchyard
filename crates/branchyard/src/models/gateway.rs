//! A turn's model gateway: an HTTP/1.1 reverse proxy on `std` threads,
//! one thread per connection, for as long as the turn runs.
//!
//! Paths:
//!
//! - `/anthropic/...` is Anthropic's API (`ANTHROPIC_BASE_URL`), such as
//!   `/anthropic/v1/messages`.
//! - `/openai/...` is OpenAI's (`OPENAI_BASE_URL` is `/openai/v1`), such
//!   as `/openai/v1/chat/completions` and `/openai/v1/responses`.
//! - `/backends/<name>/...` passes through to that backend as it is.
//!
//! The rest of the path and the query go to the backend after its base
//! URL's path. Every response is streamed to the harness as it arrives,
//! with `Transfer-Encoding: chunked` and `Connection: close`. A backend with
//! a compatibility layer ([`super::Compat`]) gets the request adapted, and
//! its answer read, by that layer (`gemini`).

use branchyard_support::best_effort;
use branchyard_support::{CondvarExt as _, LockExt as _};
use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use branchyard_wire as wire;
use serde_json::{json, Value};

use super::translate;
use super::upstream::{self, Failure, Target};
use super::usage::{Asked, Collector, Meter, Observed};
use super::{
    audit, gemini, Api, Audit, Backend, Compat, ControlPlane, ControlPlaneCall, Gateway,
    Interception, ModelActivity, ModelCall, Tokens, UsageRecord,
};
use crate::connectors::keys;
use crate::{Activity, RecordedEvent, Yard};
use branchyard_support::time::now_ms;

/// Largest request head and body the gateway takes.
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 32 * 1024 * 1024;
/// Largest error body kept from a backend that is failed over.
const MAX_ERROR_BODY: usize = 1024 * 1024;
/// How long a turn's end waits for calls still in flight.
const DRAIN: Duration = Duration::from_secs(5);

/// What a turn's gateway knows.
pub(crate) struct TurnState {
    pub yard: Yard,
    pub gateway: Arc<Gateway>,
    /// The yard's public keys, which the token must verify against.
    pub jwks: Value,
    /// The turn's token: the only one this gateway takes.
    pub token: String,
    pub branch: String,
    /// The branch as tokens and usage name it (`<repo>/<branch>` on a
    /// server).
    pub by_branch: String,
    pub turn: u32,
    pub subject: String,
    /// The harness's catalog id, as the branch records it: the
    /// `by_harness` a control-plane backend is told.
    pub harness: String,
    /// The turn's W3C trace context, the one its harness is given: a
    /// control-plane backend is sent it as `traceparent`.
    pub trace_parent: Option<String>,
    /// The branch's cost limit, what it had spent before this turn, and
    /// what its children held of the limit when the turn started (the
    /// figure a call falls back on when the store cannot be read: each
    /// call reads what they hold then).
    pub max_usd: Option<f64>,
    pub cost_before: f64,
    pub held: f64,
    /// What this turn's calls cost so far.
    pub metered: Mutex<f64>,
}

/// A running turn gateway; stopped when dropped.
pub struct TurnGateway {
    state: Arc<TurnState>,
    stop: Arc<AtomicBool>,
    address: SocketAddr,
    in_flight: Arc<(Mutex<usize>, Condvar)>,
    thread: Option<JoinHandle<()>>,
    /// Its entry in the registry, removed when the gateway stops.
    service: Option<crate::services::Registration>,
}

impl TurnGateway {
    pub(crate) fn start(listener: TcpListener, state: Arc<TurnState>) -> io::Result<TurnGateway> {
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let in_flight = Arc::new((Mutex::new(0usize), Condvar::new()));
        let thread = {
            let (state, stop, in_flight) = (state.clone(), stop.clone(), in_flight.clone());
            std::thread::Builder::new()
                .name("by-models".into())
                .spawn(move || accept(listener, state, stop, in_flight))?
        };
        Ok(TurnGateway {
            state,
            stop,
            address,
            in_flight,
            thread: Some(thread),
            service: None,
        })
    }

    /// Keep `service`, the gateway's registry entry, for as long as it runs.
    pub(crate) fn registered(mut self, service: Option<crate::services::Registration>) -> Self {
        self.service = service;
        self
    }

    /// What this turn's calls have cost so far.
    pub fn metered(&self) -> f64 {
        *self.state.metered.lock_recovering("metered")
    }

    /// Stop taking calls, wait a little for those in flight, and return
    /// what the turn's calls cost.
    pub fn finish(mut self) -> f64 {
        self.shut();
        self.metered()
    }

    fn shut(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        // Gone from the registry before it stops answering.
        self.service.take();
        // Wake the accept loop.
        let mut wake = self.address;
        if wake.ip().is_unspecified() {
            wake.set_ip(match wake {
                SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
            });
        }
        best_effort(
            "connect to wake the accept loop",
            TcpStream::connect_timeout(&wake, Duration::from_secs(1)),
        );
        if let Some(thread) = self.thread.take() {
            branchyard_support::join_reporting("model gateway accept", thread);
        }
        let (count, idle) = &*self.in_flight;
        let deadline = Instant::now() + DRAIN;
        let mut active = count.lock_recovering("count");
        while *active > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            active = idle.wait_timeout_recovering(active, left, "idle").0;
        }
    }
}

impl Drop for TurnGateway {
    fn drop(&mut self) {
        self.shut();
    }
}

fn accept(
    listener: TcpListener,
    state: Arc<TurnState>,
    stop: Arc<AtomicBool>,
    in_flight: Arc<(Mutex<usize>, Condvar)>,
) {
    static THREADS: AtomicUsize = AtomicUsize::new(0);
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let (state, in_flight) = (state.clone(), in_flight.clone());
        *in_flight.0.lock_recovering("in-flight count") += 1;
        let n = THREADS.fetch_add(1, Ordering::Relaxed);
        let spawned = std::thread::Builder::new()
            .name(format!("by-models-{n}"))
            .spawn(move || {
                handle(stream, &state);
                let (count, idle) = &*in_flight;
                *count.lock_recovering("count") -= 1;
                idle.notify_all();
            });
        if spawned.is_err() {
            return;
        }
    }
}

/// A request from the harness.
pub(crate) struct Request {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Read one request from the harness, through the wire codec: a head it
/// cannot frame (both `Content-Length` and `Transfer-Encoding`, a bad chunk
/// size, a body cut short) is an `Err`, which the caller answers with 400.
pub(crate) fn read_request(
    reader: &mut BufReader<TcpStream>,
    out: &mut TcpStream,
) -> Result<Option<Request>, String> {
    let bad = |e: wire::WireError| format!("the request is malformed: {e}");
    let Some(head) = wire::read_request_head(reader, MAX_HEAD).map_err(bad)? else {
        return Ok(None);
    };
    let framing = wire::request_framing(&head.headers).map_err(bad)?;
    let expects = wire::header(&head.headers, "expect")
        .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"));
    if expects && framing != wire::Framing::None {
        best_effort(
            "send 100 Continue",
            out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n"),
        );
    }
    let body = wire::read_body(&mut *reader, framing, MAX_BODY).map_err(bad)?;
    Ok(Some(Request {
        method: head.method,
        target: head.target,
        headers: head.headers,
        body,
    }))
}

/// Which API a path is, and the path to send on; a `/backends/<name>`
/// path names its backend.
fn route(target: &str) -> Option<(Api, String, Option<String>)> {
    let rest_of = |rest: &str| -> Option<String> {
        match rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') {
            true => Some(rest.to_owned()),
            false => None,
        }
    };
    if let Some(rest) = target.strip_prefix("/anthropic") {
        return Some((Api::Anthropic, rest_of(rest)?, None));
    }
    if let Some(rest) = target.strip_prefix("/openai") {
        return Some((Api::Openai, rest_of(rest)?, None));
    }
    let rest = target.strip_prefix("/backends/")?;
    let (name, path) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    (!name.is_empty()).then(|| (Api::Generic, path.to_owned(), Some(name.to_owned())))
}

/// The token the harness presented: `x-api-key`, or a bearer token.
pub(crate) fn presented(request: &Request) -> Option<&str> {
    request.header("x-api-key").or_else(|| {
        request
            .header("authorization")
            .and_then(|v| {
                v.strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
            })
            .map(str::trim)
    })
}

pub(crate) fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The models the presented token allows, or why it is refused: it must
/// be this turn's token, verify against the yard's keys, be unexpired, and
/// carry a model scope.
fn authorize(state: &TurnState, token: Option<&str>) -> Result<Vec<String>, String> {
    let token = token.ok_or("no token was presented")?;
    if !same(token, &state.token) {
        return Err("the token is not this turn's".into());
    }
    let claims = keys::verify(&state.jwks, token)?;
    let claims: keys::Claims =
        serde_json::from_value(claims).map_err(|e| format!("the token's claims: {e}"))?;
    if claims.exp <= now_ms() / 1000 {
        return Err("the token has expired".into());
    }
    if claims.by_purpose.is_some()
        || claims.by_branch != state.by_branch
        || claims.by_turn != state.turn.to_string()
    {
        return Err("the token is not this turn's".into());
    }
    claims
        .by_models
        .ok_or_else(|| "the token carries no model scope".to_owned())
}

/// An error in `api`'s own shape.
fn error_body(api: Api, status: u16, message: &str) -> String {
    let kind = match status {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "api_error",
    };
    match api {
        Api::Anthropic => json!({"type": "error", "error": {"type": kind, "message": message}}),
        Api::Openai | Api::Generic => {
            json!({"error": {"message": message, "type": kind, "code": kind}})
        }
    }
    .to_string()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Error",
    }
}

/// Answer the harness without forwarding.
fn refuse(
    out: &mut TcpStream,
    api: Api,
    status: u16,
    decision: &str,
    message: &str,
    extra: &[(&str, String)],
) {
    let body = error_body(api, status, message);
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\nX-Branchyard-Gateway: {decision}\r\n",
        reason_phrase(status),
        body.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    best_effort("send the refusal head", out.write_all(head.as_bytes()));
    best_effort("send the refusal body", out.write_all(body.as_bytes()));
    best_effort("flush the refusal", out.flush());
}

/// Hop-by-hop headers, and those the gateway sets itself.
fn dropped(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "host"
            | "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "expect"
            | "authorization"
            | "x-api-key"
            | "api-key"
            | "accept-encoding"
    ) || name.starts_with("proxy-")
        || name.starts_with("x-branchyard-")
}

/// Of what the harness sent, the request headers a control-plane backend
/// is sent: the body's content headers, `Accept`, the user agent, the
/// APIs' own version, beta and idempotency headers. Nothing else of the
/// harness's reaches a control plane, which reads some headers as who is
/// calling (an IAP assertion, `X-Forwarded-For`, a cookie, its own
/// attribution headers): the gateway speaks for the call
/// (`docs/model-gateway.md`, "What is sent").
pub(crate) const CONTROL_PLANE_REQUEST_HEADERS: &[&str] = &[
    "content-type",
    "content-encoding",
    "accept",
    "user-agent",
    "anthropic-version",
    "anthropic-beta",
    "openai-beta",
    "idempotency-key",
];

/// Whether a control-plane backend is sent the harness's header `name`.
fn to_control_plane(name: &str) -> bool {
    CONTROL_PLANE_REQUEST_HEADERS
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
}

/// Of a control-plane backend's answer, the response headers the harness
/// gets: what its SDK reads from an answer (the body's content headers,
/// cache control, when and whether to retry, the provider's request id
/// and rate-limit headers). Nothing else is relayed: no cookie, location,
/// link, server or the control plane's own headers.
pub(crate) const CONTROL_PLANE_RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "content-encoding",
    "cache-control",
    "retry-after",
    "retry-after-ms",
    "x-should-retry",
    "request-id",
    "x-request-id",
];

/// Prefixes of the provider rate-limit headers a control-plane backend's
/// answer may relay.
pub(crate) const CONTROL_PLANE_RESPONSE_PREFIXES: &[&str] =
    &["anthropic-ratelimit-", "x-ratelimit-", "x-codex-"];

/// Whether the harness gets a control-plane backend's response header
/// `name`.
fn from_control_plane(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    CONTROL_PLANE_RESPONSE_HEADERS.contains(&name.as_str())
        || CONTROL_PLANE_RESPONSE_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// What the gateway tells a control-plane backend about a call: the id of
/// the call's usage record, the branch, turn and harness as tags, and the
/// turn's trace context. Made once per call; a plain backend is sent none
/// of it.
pub(crate) struct Attribution {
    /// `X-Client-Request-Id`: the [`UsageRecord::id`] the call is stored
    /// under.
    pub client_request_id: String,
    /// `X-AIPlex-Tags`, when any tag fits.
    pub tags: Option<String>,
    /// `traceparent`, when the turn has a well-formed one.
    pub traceparent: Option<String>,
}

/// The header a control-plane backend is sent the call's id in.
pub(crate) const CLIENT_REQUEST_ID: &str = "X-Client-Request-Id";
/// The header a control-plane backend is sent the tags in.
pub(crate) const TAGS: &str = "X-AIPlex-Tags";
/// The header a control-plane backend is sent the trace context in.
pub(crate) const TRACEPARENT: &str = "traceparent";
/// The response header carrying the control plane's id of the call.
pub(crate) const UPSTREAM_REQUEST_ID: &str = "X-Aiplex-Request-Id";
/// The response header carrying the control plane's code for a refusal
/// it made itself; its presence makes the answer final.
pub(crate) const UPSTREAM_ERROR_CODE: &str = "X-Aiplex-Error-Code";
/// The longest `X-AIPlex-Tags` value, in bytes.
const MAX_TAGS: usize = 512;
/// The longest tag value, in characters.
const MAX_TAG_VALUE: usize = 64;

impl Attribution {
    /// The call's attribution for `state`'s turn, with a fresh id.
    fn new(state: &TurnState) -> Result<Attribution, String> {
        let id = keys::random_id().map_err(|e| format!("could not make the call's id: {e}"))?;
        if !request_id_ok(&id) {
            return Err(format!("could not make the call's id: {id:?} is not one"));
        }
        Ok(Attribution {
            client_request_id: id,
            tags: tags(&state.by_branch, state.turn, &state.harness),
            traceparent: state.trace_parent.clone().filter(|t| traceparent_ok(t)),
        })
    }
}

/// Whether `id` is a request id both sides keep: 1 to 128 of
/// `A-Z a-z 0-9 - _ . :`.
pub(crate) fn request_id_ok(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// Whether `code` is a control plane's error code: 1 to 64 of `A-Z 0-9 _`.
pub(crate) fn error_code_ok(code: &str) -> bool {
    (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Whether `value` fits a tag: 1 to 64 of
/// `A-Z a-z 0-9 _ . : / @ + ( ) -` and space.
fn tag_value_ok(value: &str) -> bool {
    (1..=MAX_TAG_VALUE).contains(&value.len())
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'_' | b'.' | b':' | b'/' | b'@' | b'+' | b'(' | b')' | b'-' | b' '
                )
        })
}

/// `by_branch=<branch>,by_turn=<turn>,by_harness=<harness>`: a value that
/// does not fit is left out with its key, never cut short, and keys are
/// dropped from the end until the whole is at most 512 bytes. `None` when
/// no tag is left.
pub(crate) fn tags(branch: &str, turn: u32, harness: &str) -> Option<String> {
    let turn = turn.to_string();
    fit_tags(&[
        ("by_branch", branch),
        ("by_turn", turn.as_str()),
        ("by_harness", harness),
    ])
}

/// `key=value` pairs joined by commas, under [`tags`]' rules.
fn fit_tags(pairs: &[(&str, &str)]) -> Option<String> {
    let mut parts: Vec<String> = pairs
        .iter()
        .filter(|(_, value)| tag_value_ok(value))
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    while parts.join(",").len() > MAX_TAGS {
        parts.pop();
    }
    (!parts.is_empty()).then(|| parts.join(","))
}

/// Whether `value` is a W3C `traceparent` of version `00`: lowercase hex,
/// a trace id and a span id that are not all zeros, and flags.
pub(crate) fn traceparent_ok(value: &str) -> bool {
    let hex = |part: &str, len: usize| {
        part.len() == len
            && part
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    let parts: Vec<&str> = value.split('-').collect();
    match parts.as_slice() {
        [version, trace, span, flags] => {
            *version == "00"
                && hex(trace, 32)
                && hex(span, 16)
                && hex(flags, 2)
                && trace.bytes().any(|b| b != b'0')
                && span.bytes().any(|b| b != b'0')
        }
        _ => false,
    }
}

/// The headers `backend` is sent: what the harness sent, without its
/// credentials, hop-by-hop headers and `Accept-Encoding`, then
/// `Accept-Encoding: identity` and the backend's key. A control-plane
/// backend is sent only the harness's headers that
/// [`CONTROL_PLANE_REQUEST_HEADERS`] names, takes its key as a bearer token
/// whatever its API, and is sent `attribution`.
fn upstream_headers(
    request: &Request,
    backend: &Backend,
    key: &str,
    attribution: &Attribution,
) -> Vec<(String, String)> {
    let governed = backend.control_plane.is_some();
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|(n, _)| match governed {
            true => to_control_plane(n),
            false => !dropped(n),
        })
        .cloned()
        .collect();
    // Plain bytes, so usage can be read from them.
    headers.push(("Accept-Encoding".into(), "identity".into()));
    let bearer = ("Authorization".to_owned(), format!("Bearer {key}"));
    match (backend.api, backend.header.as_deref()) {
        _ if governed => headers.push(bearer),
        (Api::Anthropic, _) => headers.push(("x-api-key".into(), key.to_owned())),
        (_, None) => headers.push(bearer),
        (_, Some(header)) if header.eq_ignore_ascii_case("authorization") => headers.push(bearer),
        (_, Some(header)) => headers.push((header.to_owned(), key.to_owned())),
    }
    if governed {
        headers.push((
            CLIENT_REQUEST_ID.to_owned(),
            attribution.client_request_id.clone(),
        ));
        if let Some(tags) = &attribution.tags {
            headers.push((TAGS.to_owned(), tags.clone()));
        }
        if let Some(traceparent) = &attribution.traceparent {
            headers.push((TRACEPARENT.to_owned(), traceparent.clone()));
        }
    }
    headers
}

/// The last backend that failed a call: its answer, the body kept of it,
/// the backend, the model it was asked for when translated, its verdict,
/// and the key it was sent.
type Failed<'a> = (
    upstream::Response,
    Vec<u8>,
    &'a Backend,
    Option<String>,
    Verdict,
    String,
);

/// The backend that answered a call: its answer, the backend, the model it
/// was asked for when translated, whether Gemini's signatures are kept from
/// it, its verdict, and the key it was sent.
type Answered<'a> = (
    upstream::Response,
    &'a Backend,
    Option<String>,
    bool,
    Verdict,
    String,
);

/// What a control-plane backend's answer says of itself.
#[derive(Debug, Default, PartialEq)]
struct Verdict {
    /// Its id of the call, when well formed.
    request_id: Option<String>,
    /// Its code for a refusal it made, when well formed.
    error_code: Option<String>,
    /// Whether the answer carries an error-code header at all: then it is
    /// the control plane's decision, final, whatever the code says.
    decided: bool,
}

impl Verdict {
    /// What `response` from `backend` says; nothing for a plain backend.
    fn of(backend: &Backend, headers: &[(String, String)]) -> Verdict {
        if backend.control_plane.is_none() {
            return Verdict::default();
        }
        let first = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.trim())
        };
        Verdict {
            request_id: first(UPSTREAM_REQUEST_ID)
                .filter(|v| request_id_ok(v))
                .map(str::to_owned),
            error_code: first(UPSTREAM_ERROR_CODE)
                .filter(|v| error_code_ok(v))
                .map(str::to_owned),
            decided: first(UPSTREAM_ERROR_CODE).is_some(),
        }
    }

    /// What the call's event records of it.
    fn record(&self, plane: ControlPlane, attribution: &Attribution) -> Box<ControlPlaneCall> {
        Box::new(ControlPlaneCall {
            plane,
            call_id: attribution.client_request_id.clone(),
            request_id: self.request_id.clone(),
            error_code: self.error_code.clone(),
        })
    }

    /// The call's reason when the control plane refused it.
    fn reason(&self, backend: &Backend) -> String {
        let plane = backend.control_plane.unwrap_or(ControlPlane::Aiplex);
        match &self.error_code {
            Some(code) => format!("refused by {plane} ({code})"),
            None => format!("refused by {plane} (an error code that is not one)"),
        }
    }
}

/// Ask an OpenAI chat stream for its usage in its last chunk, which it
/// sends only when asked.
fn with_stream_usage(api: Api, path: &str, json: &mut Value) -> bool {
    let chat = path
        .split('?')
        .next()
        .unwrap_or("")
        .ends_with("/chat/completions");
    if api != Api::Openai || !chat || json.get("stream") != Some(&Value::Bool(true)) {
        return false;
    }
    let Some(object) = json.as_object_mut() else {
        return false;
    };
    let options = object.entry("stream_options").or_insert_with(|| json!({}));
    match options.as_object_mut() {
        Some(options) if !options.contains_key("include_usage") => {
            options.insert("include_usage".into(), Value::Bool(true));
            true
        }
        _ => false,
    }
}

/// The next request, or `None` once the harness is answered: nothing sent,
/// or a request the wire codec refuses, which gets a 400 and no more.
fn take_request(reader: &mut BufReader<TcpStream>, out: &mut TcpStream) -> Option<Request> {
    match read_request(reader, out) {
        Ok(request) => request,
        Err(why) => {
            refuse(out, Api::Generic, 400, "bad_request", &why, &[]);
            None
        }
    }
}

fn handle(stream: TcpStream, state: &TurnState) {
    let started = Instant::now();
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    best_effort(
        "restore the stream's read timeout",
        stream.set_read_timeout(Some(Duration::from_secs(300))),
    );
    let mut reader = BufReader::new(stream);
    let Some(request) = take_request(&mut reader, &mut out) else {
        return;
    };
    let Some((api, path, named)) = route(&request.target) else {
        refuse(
            &mut out,
            Api::Generic,
            404,
            "not_found",
            "Branchyard's model gateway serves /anthropic, /openai and /backends/<name>",
            &[],
        );
        return;
    };
    let mut call = ModelCall {
        model: String::new(),
        api,
        decision: String::new(),
        backend: None,
        reason: None,
        status: 0,
        translated: None,
        tokens: None,
        cost_usd: None,
        latency_ms: 0,
        streamed: false,
        failed_over: Vec::new(),
        interception: None,
        audit_file: None,
        control_plane: None,
    };
    let mut json: Option<Value> = match request.body.is_empty() {
        true => None,
        false => serde_json::from_slice(&request.body).ok(),
    };
    // A JSON body the gateway cannot read could name a model it cannot
    // check: refused, never forwarded.
    let says_json = request
        .header("content-type")
        .is_some_and(|t| t.to_ascii_lowercase().contains("json"));
    if says_json && !request.body.is_empty() && json.is_none() {
        refuse(
            &mut out,
            api,
            400,
            "bad_request",
            "Branchyard's model gateway: the request body is not JSON",
            &[],
        );
        return;
    }
    call.model = json
        .as_ref()
        .and_then(|j| j.get("model"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let done = |call: &mut ModelCall, decision: &str, status: u16, reason: Option<String>| {
        call.decision = decision.to_owned();
        call.status = status;
        call.reason = reason;
        call.latency_ms = started.elapsed().as_millis() as u64;
        record(state, call);
    };
    // The token, then its model scope.
    let allowed = match authorize(state, presented(&request)) {
        Ok(models) => models,
        Err(why) => {
            done(&mut call, "unauthorized", 401, Some(why.clone()));
            refuse(
                &mut out,
                api,
                401,
                "unauthorized",
                &format!("Branchyard's model gateway: {why}"),
                &[],
            );
            return;
        }
    };
    let access = branchyard_provision::models::ModelAccess { allow: allowed };
    if !call.model.is_empty() && !access.allows(&call.model) {
        let why = format!(
            "the turn may not call {} (it may call {access})",
            call.model
        );
        done(&mut call, "denied", 403, Some(why.clone()));
        refuse(
            &mut out,
            api,
            403,
            "denied",
            &format!("Branchyard's model gateway: {why}"),
            &[],
        );
        return;
    }
    // Budgets, before anything is spent.
    if let Err(why) = within_budgets(state) {
        done(&mut call, "budget", 403, Some(why.clone()));
        refuse(
            &mut out,
            api,
            403,
            "budget",
            &format!("Branchyard's model gateway: {why}"),
            &[],
        );
        return;
    }
    // Backends, and the route's rate limit.
    let gateway = &state.gateway;
    let model = (!call.model.is_empty()).then_some(call.model.as_str());
    // The pass-through path names one backend, never a control plane's:
    // it would carry whatever the harness sent, under no route.
    let (index, order): (Option<usize>, Vec<&Backend>) = match &named {
        Some(name) => (
            None,
            gateway
                .backend(name)
                .filter(|b| b.control_plane.is_none())
                .into_iter()
                .collect(),
        ),
        None => gateway.plan(api, model),
    };
    if order.is_empty() {
        let why = match &named {
            Some(name) if gateway.backend(name).is_some() => format!(
                "{name} is a control-plane backend, which the pass-through path never reaches"
            ),
            Some(name) => format!("no backend {name}"),
            None => format!(
                "no backend serves the {api} API for {}",
                model.unwrap_or("this request")
            ),
        };
        done(&mut call, "failed", 404, Some(why.clone()));
        refuse(
            &mut out,
            api,
            404,
            "failed",
            &format!("Branchyard's model gateway: {why}"),
            &[],
        );
        return;
    }
    if let Some(index) = index {
        if let Err(seconds) = gateway.admit(index, now_ms()) {
            let why = format!(
                "the route for {} allows {} requests a minute",
                gateway.routes[index].model,
                gateway.routes[index].requests_per_minute.unwrap_or(0)
            );
            done(&mut call, "rate_limited", 429, Some(why.clone()));
            refuse(
                &mut out,
                api,
                429,
                "rate_limited",
                &format!("Branchyard's model gateway: {why}"),
                &[("Retry-After", seconds.to_string())],
            );
            return;
        }
    }
    // The call's id, made before it is forwarded: a control-plane backend
    // is sent it, and the usage record is stored under it.
    let attribution = match Attribution::new(state) {
        Ok(attribution) => attribution,
        Err(why) => {
            done(&mut call, "failed", 500, Some(why.clone()));
            refuse(
                &mut out,
                api,
                500,
                "failed",
                &format!("Branchyard's model gateway: {why}"),
                &[],
            );
            return;
        }
    };
    // The request as the harness sent it, before the gateway changes it.
    let asked = Asked::new(&request.body, json.as_ref());
    let mut body = request.body.clone();
    if let Some(value) = json.as_mut() {
        if with_stream_usage(api, &path, value) {
            body = serde_json::to_vec(value).unwrap_or(body);
        }
    }
    // A route naming a backend model translates for a backend of another
    // API, asking it for that model.
    let backend_model = index
        .and_then(|i| gateway.routes.get(i))
        .and_then(|r| r.backend_model.clone());
    // The chat completion a Gemini backend is sent, made once for the call:
    // every Gemini backend of a call is sent the same one.
    let mut for_gemini: Option<Vec<u8>> = None;
    // Forward: the next backend on a refused connection, a 5xx or a 429,
    // unless a control plane decided: its refusal is the answer.
    let mut last: Option<Failed<'_>> = None;
    let mut answered: Option<Answered<'_>> = None;
    for backend in order {
        let key = match &backend.key {
            Some(source) => match source.read() {
                Ok(key) => key,
                Err(why) => {
                    call.failed_over
                        .push(format!("{}: no key ({why})", backend.name));
                    continue;
                }
            },
            None => {
                call.failed_over
                    .push(format!("{}: no key configured", backend.name));
                continue;
            }
        };
        let target = match Target::parse(&backend.url) {
            Ok(target) => target,
            Err(why) => {
                call.failed_over.push(format!("{}: {why}", backend.name));
                continue;
            }
        };
        // What this backend is sent: the request as it is, or translated
        // for the API it speaks.
        let (send_path, send_body, as_model) = match backend.api == api {
            true => (path.clone(), body.clone(), None),
            false => {
                let bridged = backend_model
                    .clone()
                    .filter(|_| api.translates_to(backend.api));
                let Some(as_model) = bridged else {
                    call.failed_over.push(format!(
                        "{}: speaks the {} API, which a {api} request is not translated to",
                        backend.name, backend.api
                    ));
                    continue;
                };
                if !translate::translatable(&request.method, &path) {
                    call.failed_over.push(format!(
                        "{}: speaks the {} API, and {} {path} is not translated",
                        backend.name, backend.api, request.method
                    ));
                    continue;
                }
                let translated = json
                    .as_ref()
                    .ok_or_else(|| "the request has no JSON body".to_owned())
                    .and_then(|j| translate::request_to_chat(j, &as_model));
                match translated {
                    Ok(chat) => (
                        translate::CHAT_PATH.to_owned(),
                        chat.to_string().into_bytes(),
                        Some(as_model),
                    ),
                    Err(why) => {
                        let why = format!(
                            "the request cannot be translated for {}: {why}",
                            backend.name
                        );
                        done(&mut call, "failed", 400, Some(why.clone()));
                        refuse(
                            &mut out,
                            api,
                            400,
                            "bad_request",
                            &format!("Branchyard's model gateway: {why}"),
                            &[],
                        );
                        return;
                    }
                }
            }
        };
        // A backend's compatibility layer: its path, and for a chat
        // completion, its body, and its answer read for what to keep.
        let chat = gemini::is_chat(&send_path);
        let (send_path, send_body) = match backend.compat {
            Compat::Gemini => {
                let send_body = match chat {
                    true => for_gemini
                        .get_or_insert_with(|| {
                            for_gemini_backend(state, backend, &call.model, send_body)
                        })
                        .clone(),
                    false => send_body,
                };
                (gemini::path(&send_path), send_body)
            }
            Compat::Versioned => (gemini::path(&send_path), send_body),
            Compat::Off => (send_path, send_body),
        };
        let recording = backend.compat == Compat::Gemini && chat;
        let mut headers = upstream_headers(&request, backend, &key, &attribution);
        if as_model.is_some() {
            headers.retain(|(n, _)| !n.to_ascii_lowercase().starts_with("anthropic-"));
        }
        // From the first control-plane backend tried, the call's event
        // names the id it was sent under, whatever comes back.
        if let (Some(plane), None) = (backend.control_plane, &call.control_plane) {
            call.control_plane = Some(Verdict::default().record(plane, &attribution));
        }
        let sent =
            gateway
                .transport
                .send(&target, &request.method, &send_path, &headers, &send_body);
        let verdict = match &sent {
            Ok(response) => Verdict::of(backend, &response.headers),
            Err(_) => Verdict::default(),
        };
        match sent {
            Err(Failure::Connect(why)) => {
                call.failed_over
                    .push(format!("{}: could not connect ({why})", backend.name));
            }
            Err(Failure::Exchange(why)) => {
                let why = format!("{}: {why}", backend.name);
                done(&mut call, "failed", 502, Some(why.clone()));
                refuse(
                    &mut out,
                    api,
                    502,
                    "failed",
                    &format!("Branchyard's model gateway: {why}"),
                    &[],
                );
                return;
            }
            // A control plane's redirect is neither followed (it would carry
            // the credential elsewhere) nor relayed: that backend failed.
            Ok(response) if backend.control_plane.is_some() && redirect(response.status) => {
                call.failed_over.push(format!(
                    "{}: the control plane answered with a redirect ({}), not followed",
                    backend.name, response.status
                ));
            }
            // A control plane's own refusal is its decision, whatever the
            // status: no other backend is tried.
            Ok(response) if verdict.decided => {
                answered = Some((response, backend, as_model, recording, verdict, key));
                break;
            }
            Ok(mut response) if response.status >= 500 || response.status == 429 => {
                call.failed_over.push(match &verdict.request_id {
                    Some(id) => format!(
                        "{}: {} ({} request {id})",
                        backend.name,
                        response.status,
                        ControlPlane::Aiplex
                    ),
                    None => format!("{}: {}", backend.name, response.status),
                });
                let mut kept = Vec::new();
                best_effort(
                    "read the backend's error body",
                    (&mut response.body)
                        .take(MAX_ERROR_BODY as u64)
                        .read_to_end(&mut kept),
                );
                last = Some((response, kept, backend, as_model, verdict, key));
            }
            Ok(response) => {
                answered = Some((response, backend, as_model, recording, verdict, key));
                break;
            }
        }
    }
    let Some((mut response, chosen, as_model, recording, verdict, key)) = answered else {
        match last {
            // Every backend failed: the harness gets the last one's
            // answer, as it would have from the provider, in its own API.
            Some((mut response, kept, backend, as_model, verdict, key)) => {
                call.failed_over.pop();
                call.backend = Some(backend.name.clone());
                call.translated = as_model.clone();
                let mut withheld = None;
                if let Some(plane) = backend.control_plane {
                    call.control_plane = Some(verdict.record(plane, &attribution));
                    hide_control_plane(&mut response.headers);
                    withheld = withhold(&kept, &key, kept.len() >= MAX_ERROR_BODY);
                }
                let status = response.status;
                if let Some(why) = withheld {
                    // Neither shown nor kept: the harness gets Branchyard's
                    // own error, with the status.
                    let reason = with_note(
                        (!call.failed_over.is_empty()).then(|| call.failed_over.join("; ")),
                        Some(why.to_owned()),
                    );
                    done(&mut call, "failed", status, reason);
                    refuse(
                        &mut out,
                        api,
                        status,
                        "failed",
                        &format!("Branchyard's model gateway: {why}"),
                        &retry_after(&response.headers),
                    );
                    return;
                }
                // What the backend sent, before it is put in the harness's
                // API.
                let sse = is_sse(&response);
                let mut collector = Collector::new(sse);
                collector.feed(&kept);
                call.interception = Some(Box::new(Interception::new(asked, collector.finish())));
                let note = match gateway.audit {
                    Audit::Contents => {
                        keep_contents(state, &mut call, &request.body, &kept, sse, false)
                    }
                    Audit::Digests => None,
                };
                let why = (!call.failed_over.is_empty()).then(|| call.failed_over.join("; "));
                done(&mut call, "failed", status, with_note(why, note));
                let kept = match as_model {
                    Some(_) => translate::error_to_anthropic(status, &kept)
                        .to_string()
                        .into_bytes(),
                    None => kept,
                };
                best_effort(
                    "send the failed response head",
                    write_head(&mut out, &response, false),
                );
                best_effort(
                    "send the failed response body",
                    wire::write_chunk(&mut out, &kept),
                );
                best_effort("end the failed response", out.write_all(wire::LAST_CHUNK));
                best_effort("flush the failed response", out.flush());
            }
            None => {
                let why = call.failed_over.join("; ");
                done(&mut call, "failed", 502, Some(why.clone()));
                refuse(
                    &mut out,
                    api,
                    502,
                    "failed",
                    &format!("Branchyard's model gateway: no backend answered: {why}"),
                    &[],
                );
            }
        }
        return;
    };
    let backend = chosen.name.clone();
    call.backend = Some(backend.clone());
    call.translated = as_model.clone();
    call.status = response.status;
    if let Some(plane) = chosen.control_plane {
        call.control_plane = Some(verdict.record(plane, &attribution));
        hide_control_plane(&mut response.headers);
    }
    let sse = is_sse(&response);
    call.streamed = sse;
    let no_body = request.method == "HEAD" || matches!(response.status, 204 | 304);
    // A control plane's answer that is not a success is read whole, up to
    // MAX_ERROR_BODY, and checked for the credential the gateway sent before
    // the harness sees a byte of it; withheld, it is neither shown nor kept.
    let mut prefetched: Option<std::io::Cursor<Vec<u8>>> = None;
    let mut withheld: Option<String> = None;
    if chosen.control_plane.is_some() && !response_ok(response.status) && !no_body {
        let mut whole = Vec::new();
        withheld = match (&mut response.body)
            .take(MAX_ERROR_BODY as u64 + 1)
            .read_to_end(&mut whole)
        {
            Ok(_) => withhold(&whole, &key, whole.len() > MAX_ERROR_BODY).map(str::to_owned),
            Err(e) => Some(format!(
                "the control plane's answer broke off before it could be checked: {e}"
            )),
        };
        prefetched = Some(std::io::Cursor::new(whole));
    }
    // Usage and tool calls are read in the backend's shape: the harness's
    // own, unless the request was translated.
    let backend_api = chosen.api;
    let keep = matches!(gateway.audit, Audit::Contents) && withheld.is_none();
    let mut reading = Reading::new(backend_api, sse, keep).recording(
        recording.then(|| gemini::Recorder::new(&gateway.signatures, &state.subject, sse)),
    );
    let mut buf = vec![0u8; 16 * 1024];
    let mut broken: Option<String> = None;
    let mut client_gone = false;
    let status = response.status;
    let source: &mut dyn Read = match prefetched.as_mut() {
        Some(bytes) => bytes,
        None => &mut response.body,
    };
    match (&as_model, sse && response_ok(status), &withheld) {
        // Branchyard's own error in the harness's API, with the status.
        (_, _, Some(why)) => {
            let label = match verdict.decided {
                true => "refused",
                false => "failed",
            };
            refuse(
                &mut out,
                api,
                status,
                label,
                &format!("Branchyard's model gateway: {why}"),
                &retry_after(&response.headers),
            );
        }
        (None, _, None) => {
            client_gone = write_head_parts(
                &mut out,
                status,
                &response.reason,
                &response.headers,
                no_body,
            )
            .is_err();
            if !no_body {
                loop {
                    match source.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            reading.feed(&buf[..n]);
                            if !client_gone && wire::write_chunk(&mut out, &buf[..n]).is_err() {
                                // The harness went away: stop, which ends the
                                // backend's generation too.
                                client_gone = true;
                                break;
                            }
                        }
                        Err(e) => {
                            broken = Some(format!("the backend's response broke off: {e}"));
                            break;
                        }
                    }
                }
            }
        }
        // A translated stream: the backend's events become the harness's
        // as they pass.
        (Some(_), true, None) => {
            client_gone =
                write_head_parts(&mut out, status, &response.reason, &response.headers, false)
                    .is_err();
            let mut translator = translate::StreamTranslator::new(&call.model);
            loop {
                match source.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        reading.feed(&buf[..n]);
                        let events = translator.feed(&buf[..n]);
                        if !client_gone
                            && !events.is_empty()
                            && wire::write_chunk(&mut out, &events).is_err()
                        {
                            client_gone = true;
                            break;
                        }
                    }
                    Err(e) => {
                        broken = Some(format!("the backend's response broke off: {e}"));
                        break;
                    }
                }
            }
            let tail = translator.finish();
            if !client_gone
                && broken.is_none()
                && !tail.is_empty()
                && wire::write_chunk(&mut out, &tail).is_err()
            {
                client_gone = true;
            }
        }
        // A translated answer, or error: whole, then in the harness's API.
        (Some(_), false, None) => {
            let mut whole = Vec::new();
            match source.take(MAX_BODY as u64 + 1).read_to_end(&mut whole) {
                Err(e) => broken = Some(format!("the backend's response broke off: {e}")),
                Ok(_) if whole.len() > MAX_BODY => {
                    broken = Some("the backend's response is too large to translate".into())
                }
                Ok(_) => {}
            }
            reading.feed(&whole);
            let translated = match (&broken, response_ok(status)) {
                (Some(_), _) => None,
                (None, false) => Some(translate::error_to_anthropic(status, &whole).to_string()),
                (None, true) => match serde_json::from_slice::<Value>(&whole)
                    .map_err(|e| e.to_string())
                    .and_then(|v| translate::response_to_message(&v, &call.model))
                {
                    Ok(message) => Some(message.to_string()),
                    Err(why) => {
                        broken = Some(format!(
                            "the backend's answer could not be translated: {why}"
                        ));
                        None
                    }
                },
            };
            match translated {
                Some(text) => {
                    let mut headers: Vec<(String, String)> = response
                        .headers
                        .iter()
                        .filter(|(n, _)| !n.eq_ignore_ascii_case("content-type"))
                        .cloned()
                        .collect();
                    headers.push(("Content-Type".into(), "application/json".into()));
                    client_gone =
                        write_head_parts(&mut out, status, &response.reason, &headers, false)
                            .is_err()
                            || wire::write_chunk(&mut out, text.as_bytes()).is_err();
                }
                None => {
                    call.status = 502;
                    refuse(
                        &mut out,
                        api,
                        502,
                        "failed",
                        &format!(
                            "Branchyard's model gateway: {}",
                            broken
                                .as_deref()
                                .unwrap_or("the backend's answer could not be read")
                        ),
                        &[],
                    );
                }
            }
        }
    }
    // The call is metered, stored and recorded before the harness sees its
    // response end, so the next call it makes is held to what this one
    // cost. A translated call is priced by the model the backend served.
    let read = reading.finish();
    let (tokens, answered_model) = (read.tokens, read.model);
    if call.model.is_empty() {
        call.model = answered_model.unwrap_or_default();
    }
    call.tokens = tokens;
    call.interception = Some(Box::new(Interception::new(asked, read.observed)));
    // With `audit = "contents"`, the request and the response, to a file of
    // the turn's; never to the event log.
    let note = match &read.kept {
        Some(kept) => keep_contents(state, &mut call, &request.body, kept, sse, read.truncated),
        None => None,
    };
    let priced = as_model.clone().unwrap_or_else(|| call.model.clone());
    call.cost_usd = tokens.and_then(|t| gateway.cost(backend_api, &priced, &t));
    let reason = match (client_gone, broken.clone()) {
        (_, Some(why)) => Some(why),
        (true, None) => Some("the harness closed the connection".to_owned()),
        _ if response_ok(call.status) && tokens.is_none() && !call.model.is_empty() => {
            Some("the response carried no usage".to_owned())
        }
        _ => None,
    };
    let reason = with_note(with_note(reason, note), withheld.clone());
    let status = call.status;
    let (decision, reason) = match (response_ok(status), verdict.decided) {
        (true, _) => ("allowed", reason),
        (false, true) => ("refused", with_note(Some(verdict.reason(chosen)), reason)),
        (false, false) => ("failed", reason),
    };
    let tokens = tokens.unwrap_or_default();
    let row = UsageRecord {
        id: attribution.client_request_id.clone(),
        at_ms: now_ms(),
        branch: state.by_branch.clone(),
        turn: state.turn,
        subject: state.subject.clone(),
        model: priced,
        api: backend_api,
        backend: backend.clone(),
        tokens,
        cost_usd: call.cost_usd,
        latency_ms: started.elapsed().as_millis() as u64,
        status,
        streamed: call.streamed,
    };
    if let Some(cost) = call.cost_usd {
        *state.metered.lock_recovering("metered") += cost;
    }
    best_effort(
        "record the model usage",
        state.yard.store().usage().put_usage(&row),
    );
    done(&mut call, decision, status, reason);
    alert(state);
    if !no_body && !client_gone && broken.is_none() && withheld.is_none() {
        best_effort("end the streamed response", out.write_all(wire::LAST_CHUNK));
    }
    best_effort("flush the streamed response", out.flush());
}

/// The chat completion `body` as a Gemini backend takes it (`gemini`): the
/// fields its endpoint rejects stripped, and replayed tool calls given their
/// thought signatures, as the turn's person's calls recorded them. A body
/// that is not JSON goes as it is.
fn for_gemini_backend(state: &TurnState, backend: &Backend, model: &str, body: Vec<u8>) -> Vec<u8> {
    let Ok(mut request) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let prepared = gemini::prepare(&mut request, &state.gateway.signatures, &state.subject);
    gemini::log(&backend.name, model, &prepared);
    match prepared.changed() {
        true => serde_json::to_vec(&request).unwrap_or(body),
        false => body,
    }
}

/// Keep of a control-plane backend's answer only the headers a harness's
/// SDK reads ([`CONTROL_PLANE_RESPONSE_HEADERS`]): nothing that would point
/// the harness at the control plane (a `WWW-Authenticate` challenge naming
/// its URL, a `Location`, a `Link`, a cookie) reaches it. Status, body and
/// `Retry-After` reach the harness as they are.
fn hide_control_plane(headers: &mut Vec<(String, String)>) {
    headers.retain(|(name, _)| from_control_plane(name));
}

/// The `Retry-After` of an answer, for an error the gateway writes itself.
fn retry_after(headers: &[(String, String)]) -> Vec<(&'static str, String)> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("retry-after"))
        .map(|(_, v)| ("Retry-After", v.clone()))
        .into_iter()
        .collect()
}

/// Whether `status` is a redirect, which the gateway neither follows nor
/// relays from a control-plane backend.
fn redirect(status: u16) -> bool {
    (300..400).contains(&status) && status != 304
}

/// Why a control-plane backend's answer that is not a success is withheld
/// from the harness, if it is: it holds `key`, the credential the gateway
/// sent (a control plane that echoes its request), or it is longer than
/// [`MAX_ERROR_BODY`] (`cut`), so it could not be checked whole.
fn withhold(body: &[u8], key: &str, cut: bool) -> Option<&'static str> {
    let key = key.as_bytes();
    if !key.is_empty() && body.windows(key.len()).any(|w| w == key) {
        return Some(
            "the control plane's answer was withheld: it held the credential the gateway sent",
        );
    }
    cut.then_some(
        "the control plane's answer was withheld: it was too large to check for the credential",
    )
}

fn response_ok(status: u16) -> bool {
    (200..400).contains(&status)
}

fn is_sse(response: &upstream::Response) -> bool {
    response
        .header("content-type")
        .is_some_and(|t| t.to_ascii_lowercase().starts_with("text/event-stream"))
}

/// Reads a backend's response as its bytes pass: its usage, what it did
/// (the tools it called, why it stopped, how long it was), when the
/// call's contents are kept, the bytes themselves, and for a Gemini
/// backend, the thought signatures to keep.
struct Reading<'a> {
    meter: Meter,
    collector: Collector,
    kept: Option<Vec<u8>>,
    truncated: bool,
    recorder: Option<gemini::Recorder<'a>>,
}

/// What a response came to.
struct Finished {
    tokens: Option<Tokens>,
    /// The model the provider says answered.
    model: Option<String>,
    observed: Observed,
    /// The response body, when contents are kept.
    kept: Option<Vec<u8>>,
    /// The kept body is cut at [`audit::MAX_RESPONSE`].
    truncated: bool,
}

impl<'a> Reading<'a> {
    /// Reads a response of `api`'s shape; keeps its bytes when `keep`.
    fn new(api: Api, sse: bool, keep: bool) -> Reading<'a> {
        Reading {
            meter: Meter::new(api, sse),
            collector: Collector::new(sse),
            kept: keep.then(Vec::new),
            truncated: false,
            recorder: None,
        }
    }

    /// Also keep the thought signatures the response carries.
    fn recording(mut self, recorder: Option<gemini::Recorder<'a>>) -> Reading<'a> {
        self.recorder = recorder;
        self
    }

    /// Bytes of the response body, in order.
    fn feed(&mut self, bytes: &[u8]) {
        self.meter.feed(bytes);
        self.collector.feed(bytes);
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.feed(bytes);
        }
        if let Some(kept) = self.kept.as_mut() {
            // Cut at a chunk, and keep nothing after the cut.
            if self.truncated || kept.len() + bytes.len() > audit::MAX_RESPONSE {
                self.truncated = true;
            } else {
                kept.extend_from_slice(bytes);
            }
        }
    }

    fn finish(self) -> Finished {
        if let Some(recorder) = self.recorder {
            recorder.finish();
        }
        let (tokens, model) = self.meter.finish();
        Finished {
            tokens,
            model,
            observed: self.collector.finish(),
            kept: self.kept,
            truncated: self.truncated,
        }
    }
}

/// Append the call's request and response to the turn's audit file
/// (`[models] audit = "contents"`) and name the file on the call. `Some`
/// says why it could not be written, for the call's reason.
fn keep_contents(
    state: &TurnState,
    call: &mut ModelCall,
    request: &[u8],
    response: &[u8],
    sse: bool,
    truncated: bool,
) -> Option<String> {
    let path = state.yard.store().model_audit(&state.branch, state.turn);
    let line = audit::Entry {
        at_ms: now_ms(),
        model: &call.model,
        backend: call.backend.as_deref().unwrap_or(""),
        request,
        response,
        sse,
        truncated,
    }
    .line();
    match audit::append(&path, &line) {
        Ok(()) => {
            let shown = path.strip_prefix(state.yard.root()).unwrap_or(&path);
            call.audit_file = Some(shown.to_string_lossy().into_owned());
            None
        }
        Err(e) => Some(format!(
            "the call's contents were not written to {}: {e}",
            path.display()
        )),
    }
}

/// `reason`, with `note` added to it.
fn with_note(reason: Option<String>, note: Option<String>) -> Option<String> {
    match (reason, note) {
        (Some(reason), Some(note)) => Some(format!("{reason}; {note}")),
        (reason, note) => reason.or(note),
    }
}

fn write_head(out: &mut TcpStream, response: &upstream::Response, no_body: bool) -> io::Result<()> {
    write_head_parts(
        out,
        response.status,
        &response.reason,
        &response.headers,
        no_body,
    )
}

/// The response head toward the harness: the backend's status and
/// headers, hop-by-hop ones dropped, chunked unless there is no body.
fn write_head_parts(
    out: &mut TcpStream,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    no_body: bool,
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "connection"
                | "keep-alive"
                | "transfer-encoding"
                | "content-length"
                | "te"
                | "trailer"
                | "upgrade"
        ) || lower.starts_with("proxy-")
        {
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !no_body {
        head.push_str("Transfer-Encoding: chunked\r\n");
    }
    head.push_str("Connection: close\r\n\r\n");
    out.write_all(head.as_bytes())?;
    out.flush()
}

/// Refuse a call the branch's budget, the cost limits above it or the
/// yard's period budgets leave no room for. A call that would start with
/// the spend already at the limit is refused; one already running is not
/// cut short and is counted when it ends, so a branch ends over by at most
/// the calls in flight when it reached the limit.
fn within_budgets(state: &TurnState) -> Result<(), String> {
    let metered = *state.metered.lock_recovering("metered");
    let store = state.yard.store();
    if let Some(max) = state.max_usd {
        // What the branch's children hold changes while the turn runs
        // (it may spawn them in this very turn), so it is read for the
        // call.
        let held = store.read(&state.branch).map_or(state.held, |record| {
            crate::delegation::reserved(&store, &record)
        });
        let spent = state.cost_before + held + metered;
        if spent >= max {
            return Err(format!(
                "the branch's budget is spent (max_usd: ${spent:.4} of ${max:.2})"
            ));
        }
    }
    // The turns of a tree spend a limit above them together: once the
    // tree has spent it, none of them may start a call.
    let own = state.cost_before + metered;
    if let Some(cap) = crate::delegation::capped_above(&store, &state.branch, Some(own)) {
        return Err(format!(
            "the cost limit above this branch is spent ({})",
            cap.spent_text()
        ));
    }
    let budget = &state.gateway.budget;
    if budget.daily_usd.is_none()
        && budget.monthly_usd.is_none()
        && budget.daily_tokens.is_none()
        && budget.monthly_tokens.is_none()
    {
        return Ok(());
    }
    let (day, month) = periods(state)?;
    for (limit, used, what) in [
        (budget.daily_usd, day.cost_usd, "daily_usd"),
        (budget.monthly_usd, month.cost_usd, "monthly_usd"),
    ] {
        if let Some(limit) = limit {
            if used >= limit {
                return Err(format!(
                    "the yard's {what} budget is spent (${used:.4} of ${limit:.2})"
                ));
            }
        }
    }
    for (limit, used, what) in [
        (budget.daily_tokens, day.tokens(), "daily_tokens"),
        (budget.monthly_tokens, month.tokens(), "monthly_tokens"),
    ] {
        if let Some(limit) = limit {
            if used >= limit {
                return Err(format!(
                    "the yard's {what} budget is spent ({used} of {limit} tokens)"
                ));
            }
        }
    }
    Ok(())
}

/// The yard's spending today and this month (UTC).
fn periods(state: &TurnState) -> Result<(super::UsageTotals, super::UsageTotals), String> {
    let (day_start, month_start) = branchyard_support::time::period_starts(now_ms());
    let rows = state
        .yard
        .store()
        .usage()
        .usage_since(month_start.min(day_start))
        .map_err(|e| format!("could not read the yard's model usage: {e}"))?;
    let mut day = super::UsageTotals::default();
    let mut month = super::UsageTotals::default();
    for row in &rows {
        if row.at_ms >= month_start {
            month.add(row);
        }
        if row.at_ms >= day_start {
            day.add(row);
        }
    }
    Ok((day, month))
}

/// Record an alert for each period whose spending passed its alert
/// threshold, once per period in this process.
fn alert(state: &TurnState) {
    let budget = &state.gateway.budget;
    let at = budget.alert_at.unwrap_or(0.8);
    let any = budget.daily_usd.is_some()
        || budget.monthly_usd.is_some()
        || budget.daily_tokens.is_some()
        || budget.monthly_tokens.is_some();
    if !any {
        return;
    }
    let Ok((day, month)) = periods(state) else {
        return;
    };
    let (day_start, month_start) = branchyard_support::time::period_starts(now_ms());
    let checks = [
        ("day", "usd", day.cost_usd, budget.daily_usd, day_start),
        (
            "month",
            "usd",
            month.cost_usd,
            budget.monthly_usd,
            month_start,
        ),
        (
            "day",
            "tokens",
            day.tokens() as f64,
            budget.daily_tokens.map(|t| t as f64),
            day_start,
        ),
        (
            "month",
            "tokens",
            month.tokens() as f64,
            budget.monthly_tokens.map(|t| t as f64),
            month_start,
        ),
    ];
    for (period, unit, used, limit, start) in checks {
        let Some(limit) = limit else { continue };
        if used >= at * limit
            && state
                .gateway
                .first_alert(&format!("{period}:{unit}"), start)
        {
            let activity = ModelActivity::Alert {
                period: period.to_owned(),
                unit: unit.to_owned(),
                used,
                limit,
            };
            append(state, activity);
        }
    }
}

fn record(state: &TurnState, call: &ModelCall) {
    append(state, ModelActivity::Call(call.clone()));
}

fn append(state: &TurnState, activity: ModelActivity) {
    let event = RecordedEvent {
        at_ms: now_ms(),
        activity: Activity::Model(Box::new(activity)),
    };
    best_effort(
        "append the event to the branch's log",
        state.yard.store().append(&state.branch, &event, None),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_name_their_api() {
        assert_eq!(
            route("/anthropic/v1/messages?beta=true"),
            Some((Api::Anthropic, "/v1/messages?beta=true".into(), None))
        );
        assert_eq!(
            route("/openai/v1/responses"),
            Some((Api::Openai, "/v1/responses".into(), None))
        );
        assert_eq!(
            route("/backends/local/v1/generate"),
            Some((Api::Generic, "/v1/generate".into(), Some("local".into())))
        );
        assert_eq!(route("/anthropicx/v1"), None);
        assert_eq!(route("/backends/"), None);
        assert_eq!(route("/v1/messages"), None);
    }

    #[test]
    fn only_an_openai_chat_stream_is_asked_for_its_usage() {
        let mut chat = json!({"model": "gpt-5", "stream": true});
        assert!(with_stream_usage(
            Api::Openai,
            "/v1/chat/completions",
            &mut chat
        ));
        assert_eq!(chat["stream_options"]["include_usage"], true);
        assert!(!with_stream_usage(
            Api::Openai,
            "/v1/chat/completions",
            &mut chat
        ));
        let mut asked = json!({"stream": true, "stream_options": {"include_usage": false}});
        assert!(!with_stream_usage(
            Api::Openai,
            "/v1/chat/completions",
            &mut asked
        ));
        let mut plain = json!({"stream": false});
        assert!(!with_stream_usage(
            Api::Openai,
            "/v1/chat/completions",
            &mut plain
        ));
        let mut responses = json!({"stream": true});
        assert!(!with_stream_usage(
            Api::Openai,
            "/v1/responses",
            &mut responses
        ));
        let mut anthropic = json!({"stream": true});
        assert!(!with_stream_usage(
            Api::Anthropic,
            "/v1/chat/completions",
            &mut anthropic
        ));
    }

    #[test]
    fn errors_take_the_providers_shape() {
        let a: Value = serde_json::from_str(&error_body(Api::Anthropic, 403, "no")).unwrap();
        assert_eq!(a["type"], "error");
        assert_eq!(a["error"]["type"], "permission_error");
        let o: Value = serde_json::from_str(&error_body(Api::Openai, 429, "slow")).unwrap();
        assert_eq!(o["error"]["type"], "rate_limit_error");
        assert_eq!(o["error"]["message"], "slow");
    }

    /// A harness's connection to `take_request`: it sends `bytes` and
    /// closes its write side; what the gateway read, and what it answered.
    fn exchange(bytes: &[u8]) -> (Option<Request>, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        client.write_all(bytes).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut out = server.try_clone().unwrap();
        let mut reader = BufReader::new(server);
        let request = take_request(&mut reader, &mut out);
        drop((reader, out));
        let mut answer = String::new();
        // The gateway may reset the connection after answering a malformed
        // request; what it sent before the reset is the answer under test.
        branchyard_support::best_effort(
            "read the gateway answer",
            client.read_to_string(&mut answer),
        );
        (request, answer)
    }

    #[test]
    fn every_valid_request_in_the_wire_corpus_is_read() {
        for (name, bytes, body) in wire::corpus::requests_valid() {
            let (request, answer) = exchange(&bytes);
            let request = request.unwrap_or_else(|| panic!("{name}: answered {answer:?}"));
            assert_eq!(request.body, body, "{name}");
            assert!(answer.is_empty(), "{name}: {answer:?}");
        }
    }

    #[test]
    fn every_malformed_request_in_the_wire_corpus_gets_a_400() {
        for (name, bytes) in wire::corpus::requests_malformed() {
            let (request, answer) = exchange(&bytes);
            assert!(request.is_none(), "{name}: was read");
            assert!(
                answer.starts_with("HTTP/1.1 400 "),
                "{name}: answered {answer:?}"
            );
        }
    }

    #[test]
    fn content_length_with_transfer_encoding_is_refused_not_guessed() {
        let (request, answer) = exchange(
            b"POST /openai/v1/responses HTTP/1.1\r\nContent-Length: 4\r\n\
              Transfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n",
        );
        assert!(request.is_none());
        assert!(answer.starts_with("HTTP/1.1 400 "), "{answer:?}");
        assert!(answer.contains("Transfer-Encoding"), "{answer:?}");
    }

    #[test]
    fn a_response_is_kept_for_the_audit_only_when_asked_and_cut_once_at_a_chunk() {
        let body = br#"{"content":[{"type":"tool_use","name":"Read"}],"stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":2}}"#;
        let mut plain = Reading::new(Api::Anthropic, false, false);
        plain.feed(body);
        let done = plain.finish();
        assert_eq!(done.kept, None);
        assert_eq!(done.observed.tool_uses, ["Read"]);
        assert_eq!(done.tokens.map(|t| t.output), Some(2));
        let mut keeping = Reading::new(Api::Anthropic, false, true);
        keeping.feed(&body[..10]);
        keeping.feed(&body[10..]);
        let done = keeping.finish();
        assert_eq!(done.kept.as_deref(), Some(&body[..]));
        assert!(!done.truncated);
        // A chunk that would pass the limit is dropped, and so is every
        // chunk after it, however small: the kept bytes are a prefix.
        let mut long = Reading::new(Api::Anthropic, true, true);
        let first = vec![b':'; audit::MAX_RESPONSE / 2 + 1];
        long.feed(&first);
        long.feed(&first);
        long.feed(b": small\n");
        let done = long.finish();
        assert!(done.truncated);
        assert_eq!(done.kept.map(|k| k.len()), Some(first.len()));
        assert_eq!(done.observed.bytes, 2 * first.len() as u64 + 8);
    }

    /// What a harness might send: its credentials, hop-by-hop headers, its
    /// provider's own, and the attribution headers a control plane reads,
    /// forged, in other letter cases and repeated.
    fn harness_request() -> Request {
        let headers = [
            ("Host", "127.0.0.1:4100"),
            ("Content-Type", "application/json"),
            ("Content-Length", "2"),
            ("x-api-key", "the-turn-token"),
            ("Authorization", "Bearer the-turn-token"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "tools-1"),
            ("X-Client-Request-Id", "forged-1"),
            ("x-client-request-id", "forged-2"),
            ("X-AIPLEX-TAGS", "by_branch=forged"),
            ("x-aiplex-tags", "by_turn=9"),
            ("X-AIPlex-Other", "forged"),
            (
                "traceparent",
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            ),
            (
                "TraceParent",
                "00-11111111111111111111111111111111-2222222222222222-01",
            ),
            ("tracestate", "vendor=forged"),
            ("X-Request-Id", "forged-request"),
            ("Accept-Encoding", "gzip"),
            ("Connection", "keep-alive"),
            ("User-Agent", "harness/1"),
            ("X-Branchyard-Gateway", "forged"),
            ("Proxy-Authorization", "Basic forged"),
            ("api-key", "forged"),
            ("X-Stainless-Lang", "python"),
        ];
        Request {
            method: "POST".into(),
            target: "/anthropic/v1/messages".into(),
            headers: headers
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
            body: b"{}".to_vec(),
        }
    }

    fn plain(api: Api, header: Option<&str>) -> Backend {
        Backend {
            name: "b".into(),
            api,
            url: "http://127.0.0.1:1".into(),
            key: None,
            header: header.map(str::to_owned),
            compat: Compat::Off,
            control_plane: None,
        }
    }

    fn governed(api: Api) -> Backend {
        Backend {
            control_plane: Some(super::super::ControlPlane::Aiplex),
            ..plain(api, None)
        }
    }

    /// What the gateway would tell a control plane about a call.
    fn attribution() -> Attribution {
        Attribution {
            client_request_id: "0123456789abcdefABCDEF".into(),
            tags: tags("app/fix-login", 3, "claude-code"),
            traceparent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into()),
        }
    }

    fn pairs(headers: &[(String, String)]) -> Vec<(&str, &str)> {
        headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect()
    }

    #[test]
    fn a_plain_backend_is_sent_exactly_the_headers_it_always_was() {
        // Pinned before control planes existed: everything the harness sent
        // but its credentials, hop-by-hop headers, `Accept-Encoding` and
        // Branchyard's own, in its order, then the backend's key.
        let passed = [
            ("Content-Type", "application/json"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "tools-1"),
            ("X-Client-Request-Id", "forged-1"),
            ("x-client-request-id", "forged-2"),
            ("X-AIPLEX-TAGS", "by_branch=forged"),
            ("x-aiplex-tags", "by_turn=9"),
            ("X-AIPlex-Other", "forged"),
            (
                "traceparent",
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            ),
            (
                "TraceParent",
                "00-11111111111111111111111111111111-2222222222222222-01",
            ),
            ("tracestate", "vendor=forged"),
            ("X-Request-Id", "forged-request"),
            ("User-Agent", "harness/1"),
            ("X-Stainless-Lang", "python"),
            ("Accept-Encoding", "identity"),
        ];
        let request = harness_request();
        let with = |last: (&'static str, &'static str)| {
            let mut all = passed.to_vec();
            all.push(last);
            all
        };
        let sent = upstream_headers(
            &request,
            &plain(Api::Anthropic, None),
            "sk-key",
            &attribution(),
        );
        assert_eq!(pairs(&sent), with(("x-api-key", "sk-key")));
        let sent = upstream_headers(
            &request,
            &plain(Api::Openai, None),
            "sk-key",
            &attribution(),
        );
        assert_eq!(pairs(&sent), with(("Authorization", "Bearer sk-key")));
        let sent = upstream_headers(
            &request,
            &plain(Api::Generic, None),
            "sk-key",
            &attribution(),
        );
        assert_eq!(pairs(&sent), with(("Authorization", "Bearer sk-key")));
        let sent = upstream_headers(
            &request,
            &plain(Api::Generic, Some("AUTHORIZATION")),
            "sk-key",
            &attribution(),
        );
        assert_eq!(pairs(&sent), with(("Authorization", "Bearer sk-key")));
        let sent = upstream_headers(
            &request,
            &plain(Api::Generic, Some("x-key")),
            "sk-key",
            &attribution(),
        );
        assert_eq!(pairs(&sent), with(("x-key", "sk-key")));
    }

    #[test]
    fn a_control_plane_gets_a_bearer_and_the_gateways_attribution_never_the_harnesses() {
        // Whatever the harness sent as attribution, in any letter case and
        // however often, is dropped; the gateway's own follows the key.
        let passed = [
            ("Content-Type", "application/json"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "tools-1"),
            ("User-Agent", "harness/1"),
            ("Accept-Encoding", "identity"),
            ("Authorization", "Bearer aiplex-token"),
            ("X-Client-Request-Id", "0123456789abcdefABCDEF"),
            (
                "X-AIPlex-Tags",
                "by_branch=app/fix-login,by_turn=3,by_harness=claude-code",
            ),
            (
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            ),
        ];
        let request = harness_request();
        for api in [Api::Anthropic, Api::Openai] {
            let sent = upstream_headers(&request, &governed(api), "aiplex-token", &attribution());
            assert_eq!(pairs(&sent), passed, "{api}");
            // One credential, a bearer token, on both APIs.
            assert!(!sent
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("x-api-key")));
        }
        // No tags fit and no trace context: neither header is sent, and the
        // call's id still is.
        let bare = Attribution {
            client_request_id: "id-1".into(),
            tags: None,
            traceparent: None,
        };
        let sent = upstream_headers(&request, &governed(Api::Openai), "k", &bare);
        let names: Vec<&str> = sent.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"X-Client-Request-Id"), "{names:?}");
        for gone in ["X-AIPlex-Tags", "traceparent", "tracestate", "X-Request-Id"] {
            assert!(
                !names.iter().any(|n| n.eq_ignore_ascii_case(gone)),
                "{gone}: {names:?}"
            );
        }
    }

    #[test]
    fn a_control_plane_is_sent_only_the_harness_headers_its_api_needs() {
        let mut request = harness_request();
        for (name, value) in [
            ("X-Goog-Iap-Jwt-Assertion", "forged"),
            ("x-goog-api-key", "forged"),
            ("Cookie", "a=b"),
            ("Forwarded", "for=x"),
            ("X-Forwarded-For", "203.0.113.9"),
            ("X-REAL-IP", "203.0.113.9"),
            ("X-Tenant", "t"),
            ("x-user", "u"),
            ("OpenAI-Beta", "assistants=v2"),
            ("Idempotency-Key", "idem-1"),
            ("Accept", "application/json"),
            ("Content-Encoding", "identity"),
        ] {
            request.headers.push((name.to_owned(), value.to_owned()));
        }
        let sent = upstream_headers(&request, &governed(Api::Openai), "k", &attribution());
        let names: Vec<String> = sent.iter().map(|(n, _)| n.to_ascii_lowercase()).collect();
        assert_eq!(
            names,
            [
                "content-type",
                "anthropic-version",
                "anthropic-beta",
                "user-agent",
                "openai-beta",
                "idempotency-key",
                "accept",
                "content-encoding",
                "accept-encoding",
                "authorization",
                "x-client-request-id",
                "x-aiplex-tags",
                "traceparent",
            ]
        );
        // Values as the harness sent them.
        let value = |name: &str| {
            sent.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(value("anthropic-beta"), Some("tools-1"));
        assert_eq!(value("openai-beta"), Some("assistants=v2"));
        for name in CONTROL_PLANE_REQUEST_HEADERS {
            assert!(to_control_plane(&name.to_ascii_uppercase()), "{name}");
        }
        assert!(!to_control_plane("x-forwarded-for"));
    }

    #[test]
    fn only_what_an_sdk_reads_of_a_control_planes_answer_reaches_the_harness() {
        for name in [
            "Content-Type",
            "content-encoding",
            "Cache-Control",
            "Retry-After",
            "retry-after-ms",
            "x-should-retry",
            "request-id",
            "X-Request-Id",
            "anthropic-ratelimit-tokens-remaining",
            "x-ratelimit-limit-requests",
            "x-codex-primary-used-percent",
        ] {
            assert!(from_control_plane(name), "{name}");
        }
        for name in [
            "Set-Cookie",
            "Link",
            "Content-Location",
            "Location",
            "Via",
            "Server",
            "WWW-Authenticate",
            "X-Aiplex-Request-Id",
            "X-Aiplex-Error-Code",
            "Alt-Svc",
            "Strict-Transport-Security",
        ] {
            assert!(!from_control_plane(name), "{name}");
        }
        for status in [300, 301, 302, 303, 307, 308] {
            assert!(redirect(status), "{status}");
        }
        for status in [200, 204, 304, 400, 429, 503] {
            assert!(!redirect(status), "{status}");
        }
    }

    #[test]
    fn an_answer_holding_the_credential_is_withheld_wherever_it_is() {
        let key = "aiplex-token-123";
        assert_eq!(withhold(b"{\"error\": \"no\"}", key, false), None);
        for body in [
            &b"Bearer aiplex-token-123"[..],
            b"{\"message\": \"you sent aiplex-token-123\"}",
            b"aiplex-token-123",
        ] {
            let why = withhold(body, key, false).unwrap();
            assert!(why.contains("held the credential"), "{why}");
        }
        // A prefix of it is not it; a body too large to check is withheld.
        assert_eq!(withhold(b"aiplex-token-12", key, false), None);
        let why = withhold(b"fine", key, true).unwrap();
        assert!(why.contains("too large"), "{why}");
        // What the codec makes of a byte that is not UTF-8 is not an id.
        let odd = vec![("X-Aiplex-Request-Id".to_owned(), "req\u{fffd}1".to_owned())];
        assert_eq!(Verdict::of(&governed(Api::Openai), &odd).request_id, None);
    }

    #[test]
    fn tags_name_the_branch_turn_and_harness_and_leave_out_what_does_not_fit() {
        // A turn past 2^31 is a number like any other.
        assert_eq!(
            tags("b", u32::MAX, "h").as_deref(),
            Some("by_branch=b,by_turn=4294967295,by_harness=h")
        );
        assert_eq!(
            tags("app/fix-login", 3, "claude-code").as_deref(),
            Some("by_branch=app/fix-login,by_turn=3,by_harness=claude-code")
        );
        // Every character the contract allows, and 64 of them.
        let allowed = "Az09_.:/@+() -";
        assert_eq!(
            tags(allowed, 1, "h").as_deref(),
            Some("by_branch=Az09_.:/@+() -,by_turn=1,by_harness=h")
        );
        let longest = "b".repeat(64);
        assert_eq!(
            tags(&longest, 1, "h"),
            Some(format!("by_branch={longest},by_turn=1,by_harness=h"))
        );
        // 65 characters, or one outside the alphabet: left out with its
        // key, never cut short.
        let long = "b".repeat(65);
        assert_eq!(
            tags(&long, 1, "h").as_deref(),
            Some("by_turn=1,by_harness=h")
        );
        for bad in [
            "fix,login",
            "a=b",
            "été",
            "tab\tbranch",
            "x\"y",
            "a;b",
            "a#b",
            "",
        ] {
            assert_eq!(
                tags(bad, 2, "codex").as_deref(),
                Some("by_turn=2,by_harness=codex"),
                "{bad:?}"
            );
        }
        assert_eq!(tags("b", 7, "").as_deref(), Some("by_branch=b,by_turn=7"));
        // Nothing fits: no header.
        assert_eq!(fit_tags(&[("by_branch", ""), ("by_harness", "a,b")]), None);
        // Over 512 bytes, keys go from the end until it fits.
        let value = "v".repeat(64);
        let many: Vec<(String, &str)> = (0..9)
            .map(|i| (format!("key{i}"), value.as_str()))
            .collect();
        let pairs: Vec<(&str, &str)> = many.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let fitted = fit_tags(&pairs).unwrap();
        assert!(fitted.len() <= 512, "{}", fitted.len());
        assert_eq!(fitted.split(',').count(), 7);
        assert!(
            fitted.starts_with("key0=") && fitted.contains("key6=") && !fitted.contains("key7=")
        );
    }

    #[test]
    fn the_calls_id_and_what_a_control_plane_answers_are_checked_against_the_contract() {
        // Every id the gateway makes fits the contract's alphabet and length.
        for _ in 0..200 {
            let id = keys::random_id().unwrap();
            assert!(request_id_ok(&id), "{id}");
        }
        assert!(request_id_ok("req-17"));
        assert!(request_id_ok("A-z_0.9:x"));
        assert!(request_id_ok(&"a".repeat(128)));
        for bad in ["", " ", "a b", "a/b", "é", "a\"b"] {
            assert!(!request_id_ok(bad), "{bad:?}");
        }
        assert!(!request_id_ok(&"a".repeat(129)));
        assert!(error_code_ok("QUOTA_BUDGET_EXHAUSTED"));
        assert!(error_code_ok("NO_BACKEND_AVAILABLE"));
        assert!(error_code_ok(&"A".repeat(64)));
        for bad in ["", "quota", "Quota_X", "A-B", "A B", "É"] {
            assert!(!error_code_ok(bad), "{bad:?}");
        }
        assert!(!error_code_ok(&"A".repeat(65)));
        assert!(traceparent_ok(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        ));
        for bad in [
            "",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
        ] {
            assert!(!traceparent_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_control_planes_answer_is_read_for_its_id_and_its_code() {
        let headers = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect()
        };
        let refused = headers(&[
            ("x-aiplex-request-id", "req-17"),
            ("X-AIPLEX-ERROR-CODE", "QUOTA_BUDGET_EXHAUSTED"),
        ]);
        let verdict = Verdict::of(&governed(Api::Anthropic), &refused);
        assert_eq!(
            verdict,
            Verdict {
                request_id: Some("req-17".into()),
                error_code: Some("QUOTA_BUDGET_EXHAUSTED".into()),
                decided: true,
            }
        );
        assert_eq!(
            verdict.reason(&governed(Api::Anthropic)),
            "refused by aiplex (QUOTA_BUDGET_EXHAUSTED)"
        );
        // A plain backend's answer is not read for them, and decides nothing.
        assert_eq!(
            Verdict::of(&plain(Api::Anthropic, None), &refused),
            Verdict::default()
        );
        // A value that does not fit is not kept, but the header still makes
        // the answer the control plane's decision.
        let odd = headers(&[
            ("X-Aiplex-Request-Id", "has space"),
            ("X-Aiplex-Error-Code", "quota exhausted"),
        ]);
        let verdict = Verdict::of(&governed(Api::Openai), &odd);
        assert_eq!(
            verdict,
            Verdict {
                request_id: None,
                error_code: None,
                decided: true,
            }
        );
        assert_eq!(
            verdict.reason(&governed(Api::Openai)),
            "refused by aiplex (an error code that is not one)"
        );
        // An answer without the code is not a decision.
        let relayed = headers(&[("X-Aiplex-Request-Id", "req-18"), ("Retry-After", "3")]);
        let verdict = Verdict::of(&governed(Api::Openai), &relayed);
        assert!(!verdict.decided);
        assert_eq!(verdict.request_id.as_deref(), Some("req-18"));
        // Only what an SDK reads is kept: not what would point the harness
        // at the control plane, nor the control plane's own headers.
        let mut answer = headers(&[
            ("Retry-After", "30"),
            (
                "WWW-Authenticate",
                "Bearer resource_metadata=\"https://aiplex.test/x\"",
            ),
            ("X-Aiplex-Request-Id", "req-19"),
        ]);
        hide_control_plane(&mut answer);
        assert_eq!(pairs(&answer), [("Retry-After", "30")]);
    }

    #[test]
    fn a_connection_that_sends_nothing_is_not_answered() {
        let (request, answer) = exchange(b"");
        assert!(request.is_none());
        assert!(answer.is_empty());
    }
}
