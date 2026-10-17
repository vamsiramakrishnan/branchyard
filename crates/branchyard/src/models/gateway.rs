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
    audit, gemini, Api, Audit, Backend, Compat, Gateway, Interception, ModelActivity, ModelCall,
    Tokens, UsageRecord,
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

fn upstream_headers(request: &Request, backend: &Backend, key: &str) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|(n, _)| !dropped(n))
        .cloned()
        .collect();
    // Plain bytes, so usage can be read from them.
    headers.push(("Accept-Encoding".into(), "identity".into()));
    match (backend.api, backend.header.as_deref()) {
        (Api::Anthropic, _) => headers.push(("x-api-key".into(), key.to_owned())),
        (_, None) => headers.push(("Authorization".into(), format!("Bearer {key}"))),
        (_, Some(header)) if header.eq_ignore_ascii_case("authorization") => {
            headers.push(("Authorization".into(), format!("Bearer {key}")))
        }
        (_, Some(header)) => headers.push((header.to_owned(), key.to_owned())),
    }
    headers
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
    let (index, order): (Option<usize>, Vec<&Backend>) = match &named {
        Some(name) => (None, gateway.backend(name).into_iter().collect()),
        None => gateway.plan(api, model),
    };
    if order.is_empty() {
        let why = match &named {
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
    // Forward: the next backend on a refused connection, a 5xx or a 429.
    let mut last: Option<(upstream::Response, Vec<u8>, String, Option<String>)> = None;
    let mut answered: Option<(upstream::Response, String, Option<String>, bool)> = None;
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
        let mut headers = upstream_headers(&request, backend, &key);
        if as_model.is_some() {
            headers.retain(|(n, _)| !n.to_ascii_lowercase().starts_with("anthropic-"));
        }
        match gateway
            .transport
            .send(&target, &request.method, &send_path, &headers, &send_body)
        {
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
            Ok(mut response) if response.status >= 500 || response.status == 429 => {
                call.failed_over
                    .push(format!("{}: {}", backend.name, response.status));
                let mut kept = Vec::new();
                best_effort(
                    "read the backend's error body",
                    (&mut response.body)
                        .take(MAX_ERROR_BODY as u64)
                        .read_to_end(&mut kept),
                );
                last = Some((response, kept, backend.name.clone(), as_model));
            }
            Ok(response) => {
                answered = Some((response, backend.name.clone(), as_model, recording));
                break;
            }
        }
    }
    let Some((response, backend, as_model, recording)) = answered else {
        match last {
            // Every backend failed: the harness gets the last one's
            // answer, as it would have from the provider, in its own API.
            Some((response, kept, backend, as_model)) => {
                call.failed_over.pop();
                call.backend = Some(backend);
                call.translated = as_model.clone();
                let status = response.status;
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
    call.backend = Some(backend.clone());
    call.translated = as_model.clone();
    call.status = response.status;
    let sse = is_sse(&response);
    call.streamed = sse;
    // Usage and tool calls are read in the backend's shape: the harness's
    // own, unless the request was translated.
    let backend_api = gateway.backend(&backend).map_or(api, |b| b.api);
    let mut reading = Reading::new(backend_api, sse, matches!(gateway.audit, Audit::Contents))
        .recording(
            recording.then(|| gemini::Recorder::new(&gateway.signatures, &state.subject, sse)),
        );
    let no_body = request.method == "HEAD" || matches!(response.status, 204 | 304);
    let mut response = response;
    let mut buf = vec![0u8; 16 * 1024];
    let mut broken: Option<String> = None;
    let mut client_gone = false;
    match (&as_model, sse && response_ok(response.status)) {
        (None, _) => {
            client_gone = write_head(&mut out, &response, no_body).is_err();
            if !no_body {
                loop {
                    match response.body.read(&mut buf) {
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
        (Some(_), true) => {
            client_gone = write_head(&mut out, &response, false).is_err();
            let mut translator = translate::StreamTranslator::new(&call.model);
            loop {
                match response.body.read(&mut buf) {
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
        (Some(_), false) => {
            let mut whole = Vec::new();
            match (&mut response.body)
                .take(MAX_BODY as u64 + 1)
                .read_to_end(&mut whole)
            {
                Err(e) => broken = Some(format!("the backend's response broke off: {e}")),
                Ok(_) if whole.len() > MAX_BODY => {
                    broken = Some("the backend's response is too large to translate".into())
                }
                Ok(_) => {}
            }
            reading.feed(&whole);
            let translated = match (&broken, response_ok(response.status)) {
                (Some(_), _) => None,
                (None, false) => {
                    Some(translate::error_to_anthropic(response.status, &whole).to_string())
                }
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
                    client_gone = write_head_parts(
                        &mut out,
                        response.status,
                        &response.reason,
                        &headers,
                        false,
                    )
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
    let reason = with_note(reason, note);
    let status = call.status;
    let decision = if response_ok(status) {
        "allowed"
    } else {
        "failed"
    };
    let tokens = tokens.unwrap_or_default();
    let row = UsageRecord {
        id: keys::random_id().unwrap_or_default(),
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
    if !no_body && !client_gone && broken.is_none() {
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

    #[test]
    fn a_connection_that_sends_nothing_is_not_answered() {
        let (request, answer) = exchange(b"");
        assert!(request.is_none());
        assert!(answer.is_empty());
    }
}
