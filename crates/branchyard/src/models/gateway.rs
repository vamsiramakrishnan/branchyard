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
//! with `Transfer-Encoding: chunked` and `Connection: close`.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::upstream::{self, Failure, Target};
use super::usage::Meter;
use super::{period_starts, Api, Backend, Gateway, ModelActivity, ModelCall, UsageRecord};
use crate::connectors::keys;
use crate::state::now_ms;
use crate::{Activity, RecordedEvent, Yard};

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
    /// The branch's cost limit, and what it had spent (with what its
    /// children reserved) before this turn.
    pub max_usd: Option<f64>,
    pub spent_before: f64,
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
        *self.state.metered.lock().unwrap_or_else(|e| e.into_inner())
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
        let _ = TcpStream::connect_timeout(&wake, Duration::from_secs(1));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let (count, idle) = &*self.in_flight;
        let deadline = Instant::now() + DRAIN;
        let mut active = count.lock().unwrap_or_else(|e| e.into_inner());
        while *active > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            active = idle
                .wait_timeout(active, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
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
        *in_flight.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        let n = THREADS.fetch_add(1, Ordering::Relaxed);
        let spawned = std::thread::Builder::new()
            .name(format!("by-models-{n}"))
            .spawn(move || {
                handle(stream, &state);
                let (count, idle) = &*in_flight;
                *count.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
                idle.notify_all();
            });
        if spawned.is_err() {
            return;
        }
    }
}

/// A request from the harness.
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn read_request(
    reader: &mut BufReader<TcpStream>,
    out: &mut TcpStream,
) -> Result<Option<Request>, String> {
    let mut raw = Vec::new();
    loop {
        let before = raw.len();
        let n = reader
            .read_until(b'\n', &mut raw)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return match raw.is_empty() {
                true => Ok(None),
                false => Err("the request ended in its head".into()),
            };
        }
        if raw.len() > MAX_HEAD {
            return Err("the request head is too large".into());
        }
        let line = &raw[before..];
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 96];
    let mut parsed = httparse::Request::new(&mut headers);
    match parsed.parse(&raw) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err("the request head is malformed".into()),
    }
    let method = parsed.method.unwrap_or("").to_owned();
    let target = parsed.path.unwrap_or("").to_owned();
    let headers: Vec<(String, String)> = parsed
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_owned(),
                String::from_utf8_lossy(h.value).trim().to_owned(),
            )
        })
        .collect();
    let find = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    if find("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
        let _ = out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let chunked = find("transfer-encoding").is_some_and(|v| {
        v.to_ascii_lowercase()
            .split(',')
            .any(|t| t.trim() == "chunked")
    });
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).map_err(|e| e.to_string())?;
            let size =
                usize::from_str_radix(line.trim().split(';').next().unwrap_or("").trim(), 16)
                    .map_err(|_| "the request has a bad chunk size".to_owned())?;
            if size == 0 {
                loop {
                    line.clear();
                    if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0
                        || line.trim().is_empty()
                    {
                        break;
                    }
                }
                break;
            }
            if body.len() + size > MAX_BODY {
                return Err("the request body is too large".into());
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader
                .read_exact(&mut body[start..])
                .map_err(|e| e.to_string())?;
            line.clear();
            reader.read_line(&mut line).map_err(|e| e.to_string())?;
        }
    } else if let Some(length) = find("content-length") {
        let length: usize = length
            .parse()
            .map_err(|_| "the request has a bad Content-Length".to_owned())?;
        if length > MAX_BODY {
            return Err("the request body is too large".into());
        }
        body.resize(length, 0);
        reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    }
    Ok(Some(Request {
        method,
        target,
        headers,
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
fn presented(request: &Request) -> Option<&str> {
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

fn same(a: &str, b: &str) -> bool {
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
    let _ = out.write_all(head.as_bytes());
    let _ = out.write_all(body.as_bytes());
    let _ = out.flush();
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

fn handle(stream: TcpStream, state: &TurnState) {
    let started = Instant::now();
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let mut reader = BufReader::new(stream);
    let request = match read_request(&mut reader, &mut out) {
        Ok(Some(request)) => request,
        Ok(None) => return,
        Err(why) => {
            refuse(&mut out, Api::Generic, 400, "bad_request", &why, &[]);
            return;
        }
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
        tokens: None,
        cost_usd: None,
        latency_ms: 0,
        streamed: false,
        failed_over: Vec::new(),
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
    let mut body = request.body.clone();
    if let Some(value) = json.as_mut() {
        if with_stream_usage(api, &path, value) {
            body = serde_json::to_vec(value).unwrap_or(body);
        }
    }
    // Forward: the next backend on a refused connection, a 5xx or a 429.
    let mut last: Option<(upstream::Response, Vec<u8>, String)> = None;
    let mut answered: Option<(upstream::Response, String)> = None;
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
        let headers = upstream_headers(&request, backend, &key);
        match upstream::send(&target, &request.method, &path, &headers, &body) {
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
                let _ = (&mut response.body)
                    .take(MAX_ERROR_BODY as u64)
                    .read_to_end(&mut kept);
                last = Some((response, kept, backend.name.clone()));
            }
            Ok(response) => {
                answered = Some((response, backend.name.clone()));
                break;
            }
        }
    }
    let Some((response, backend)) = answered else {
        match last {
            // Every backend failed: the harness gets the last one's
            // answer, as it would have from the provider.
            Some((response, kept, backend)) => {
                call.failed_over.pop();
                call.backend = Some(backend);
                let status = response.status;
                let why = (!call.failed_over.is_empty()).then(|| call.failed_over.join("; "));
                done(&mut call, "failed", status, why);
                let _ = write_head(&mut out, &response, false);
                let _ = write_chunk(&mut out, &kept);
                let _ = out.write_all(b"0\r\n\r\n");
                let _ = out.flush();
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
    call.status = response.status;
    let sse = response
        .header("content-type")
        .is_some_and(|t| t.to_ascii_lowercase().starts_with("text/event-stream"));
    call.streamed = sse;
    let mut meter = Meter::new(api, sse);
    let no_body = request.method == "HEAD" || matches!(response.status, 204 | 304);
    let mut client_gone = write_head(&mut out, &response, no_body).is_err();
    let mut response = response;
    let mut buf = vec![0u8; 16 * 1024];
    let mut broken: Option<String> = None;
    if !no_body {
        loop {
            match response.body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    meter.feed(&buf[..n]);
                    if !client_gone && write_chunk(&mut out, &buf[..n]).is_err() {
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
    // The call is metered, stored and recorded before the harness sees its
    // response end, so the next call it makes is held to what this one
    // cost.
    let (tokens, answered_model) = meter.finish();
    if call.model.is_empty() {
        call.model = answered_model.unwrap_or_default();
    }
    call.tokens = tokens;
    call.cost_usd = tokens.and_then(|t| gateway.cost(api, &call.model, &t));
    let reason = match (client_gone, broken.clone()) {
        (_, Some(why)) => Some(why),
        (true, None) => Some("the harness closed the connection".to_owned()),
        _ if response_ok(call.status) && tokens.is_none() && !call.model.is_empty() => {
            Some("the response carried no usage".to_owned())
        }
        _ => None,
    };
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
        model: call.model.clone(),
        api: gateway.backend(&backend).map_or(api, |b| b.api),
        backend: backend.clone(),
        tokens,
        cost_usd: call.cost_usd,
        latency_ms: started.elapsed().as_millis() as u64,
        status,
        streamed: call.streamed,
    };
    if let Some(cost) = call.cost_usd {
        *state.metered.lock().unwrap_or_else(|e| e.into_inner()) += cost;
    }
    let _ = state.yard.store().usage().put_usage(&row);
    done(&mut call, decision, status, reason);
    alert(state);
    if !no_body && !client_gone && broken.is_none() {
        let _ = out.write_all(b"0\r\n\r\n");
    }
    let _ = out.flush();
}

fn response_ok(status: u16) -> bool {
    (200..400).contains(&status)
}

fn write_head(out: &mut TcpStream, response: &upstream::Response, no_body: bool) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, response.reason);
    for (name, value) in &response.headers {
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

fn write_chunk(out: &mut TcpStream, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    out.write_all(format!("{:x}\r\n", bytes.len()).as_bytes())?;
    out.write_all(bytes)?;
    out.write_all(b"\r\n")?;
    out.flush()
}

/// Refuse a call the branch's budget or the yard's period budgets leave
/// no room for.
fn within_budgets(state: &TurnState) -> Result<(), String> {
    let metered = *state.metered.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(max) = state.max_usd {
        let spent = state.spent_before + metered;
        if spent >= max {
            return Err(format!(
                "the branch's budget is spent (max_usd: ${spent:.4} of ${max:.2})"
            ));
        }
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
    let (day_start, month_start) = period_starts(now_ms());
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
    let (day_start, month_start) = period_starts(now_ms());
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
    let _ = state.yard.store().append(&state.branch, &event, None);
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
}
