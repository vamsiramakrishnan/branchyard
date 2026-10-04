//! A turn's effect-ledger proxy: an HTTP/1.1 reverse proxy in front of the
//! connector gateway, on `std` threads like the model gateway, for as long
//! as the turn runs. The harness is given it as `ANVIL_GATEWAY_URL`.
//!
//! Everything but a call passes through as it is (initialize, `tools/list`,
//! session deletes). A call, MCP's `tools/call` or Anvil's REST route
//! `POST /call/<tool>` alike:
//!
//! 1. is classified from what `tools/list` declares (the proxy lists the
//!    gateway's tools once, with the turn's token): a read passes through;
//!    so does a call on a connector the grant only lets read, which the
//!    gateway refuses itself;
//! 2. is decided by the approval policy: `block` answers a tool error;
//!    `ask` stores an ask and waits, within the turn's budget; `stage`
//!    performs the draft form (`_meta.stage`) or holds the call in the
//!    outbox and answers that it is staged;
//! 3. when it goes ahead, is written to the ledger `begun`, committed,
//!    **before** it is forwarded, with the entry's id as `Idempotency-Key`
//!    (and `_meta.idempotency_key` over MCP), and the lookup the tool's
//!    contract declares resolved from the request and that key; a ledger
//!    that cannot be written refuses the call;
//! 4. is finished from the answer: `confirmed` with the gateway's effect
//!    report (`_meta.effect`, or the `X-Anvil-Effect` header over REST:
//!    class, undo, deadline, compensation, lookup), or irreversible with no undo
//!    when the gateway described nothing; `failed` on a refusal or a tool
//!    error; `unknown` when the answer was lost after the call was sent.

use branchyard_support::best_effort;
use branchyard_support::{CondvarExt as _, LockExt as _};
use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};

use super::ask::{self, AskSpec};
use super::mcp::{self, EffectMeta, ToolDecl};
use super::{
    is_deletion_name, request_digest, ulid, Approval, ApprovalPolicy, ApprovalRecord, AskAbout,
    EffectActivity, EffectClass, EffectEntry, EffectMove, EffectState, Layers, Resolved, Staged,
    Subject,
};
use crate::connectors::{GrantEntry, GrantMode};
use crate::models::gateway::{presented, read_request, same, Request};
use crate::models::upstream::{self, Failure, Target};
use crate::Yard;
use branchyard_support::time::now_ms;

/// How long a turn's end waits for calls still in flight.
const DRAIN: Duration = Duration::from_secs(5);

/// What a turn's proxy knows.
pub(crate) struct ProxyState {
    pub yard: Yard,
    /// The gateway's `/mcp` URL, as this host reaches it.
    pub upstream: String,
    /// The turn's token: the only one this proxy takes.
    pub token: String,
    pub branch: String,
    pub task: String,
    pub turn: u32,
    pub subject: String,
    pub grant: Vec<GrantEntry>,
    pub admin: Option<ApprovalPolicy>,
    pub seat: Option<ApprovalPolicy>,
    pub person: Option<ApprovalPolicy>,
    pub preset: Option<ApprovalPolicy>,
    /// The turn's deadline: how long an ask may wait.
    pub deadline_ms: Option<u64>,
    /// What `tools/list` declared; listed once.
    tools: Mutex<Option<Listed>>,
    stop: Arc<AtomicBool>,
}

impl ProxyState {
    fn layers(&self) -> Layers<'_> {
        Layers {
            admin: self.admin.as_ref(),
            seat: self.seat.as_ref(),
            person: self.person.as_ref(),
            preset: self.preset.as_ref(),
        }
    }

    fn listed<T>(&self, read: impl FnOnce(&Listed) -> T) -> T {
        let mut tools = self.tools.lock_recovering("tools");
        let listed = tools.get_or_insert_with(|| {
            Listed::of(
                &mcp::Client::new(&self.upstream, &self.token)
                    .list_tools()
                    .unwrap_or_default(),
            )
        });
        read(listed)
    }

    fn decl(&self, tool: &str) -> ToolDecl {
        self.listed(|l| l.decls.get(tool).cloned().unwrap_or_default())
    }

    /// The served tools by AIR operation id.
    fn operations(&self) -> HashMap<String, String> {
        self.listed(|l| l.operations.clone())
    }

    /// Whether the grant lets this connector change anything.
    fn writes(&self, connector: &str) -> bool {
        self.grant
            .iter()
            .any(|g| g.connector == connector && g.mode == GrantMode::Write)
    }
}

/// What `tools/list` declared: each tool's contract by wire name, and the
/// wire name of each AIR operation (`anvil/operation_id`).
#[derive(Default)]
struct Listed {
    decls: HashMap<String, ToolDecl>,
    operations: HashMap<String, String>,
}

impl Listed {
    fn of(tools: &[Value]) -> Listed {
        let mut listed = Listed::default();
        for tool in tools {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            let decl = ToolDecl::read(tool);
            if let Some(operation) = &decl.operation {
                listed
                    .operations
                    .entry(operation.clone())
                    .or_insert_with(|| name.to_owned());
            }
            listed.decls.insert(name.to_owned(), decl);
        }
        listed
    }
}

/// What starts a turn's proxy.
pub(crate) struct ProxySpec {
    pub upstream: String,
    pub token: String,
    pub deadline_ms: Option<u64>,
    pub preset: Option<crate::PolicyPreset>,
}

/// A running proxy; stopped when dropped.
pub struct EffectProxy {
    state: Arc<ProxyState>,
    address: SocketAddr,
    in_flight: Arc<(Mutex<usize>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl EffectProxy {
    /// Bind `listen` and serve the turn of `record`.
    pub(crate) fn start(
        yard: &Yard,
        record: &crate::state::Record,
        listen: std::net::IpAddr,
        spec: ProxySpec,
    ) -> io::Result<EffectProxy> {
        let listener = TcpListener::bind((listen, 0))?;
        let address = listener.local_addr()?;
        let settings = yard.approval_settings();
        let subject = record
            .actor
            .as_ref()
            .map(|a| a.subject.clone())
            .or_else(|| yard.connectors().map(|g| g.subject.clone()))
            .unwrap_or_default();
        let provision = record.provision.as_ref();
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(ProxyState {
            yard: yard.clone(),
            upstream: spec.upstream,
            token: spec.token,
            branch: record.info.name.clone(),
            task: task_of(yard, record),
            turn: record.info.turns + 1,
            subject: subject.clone(),
            grant: provision.map(|p| p.connectors.clone()).unwrap_or_default(),
            admin: settings.admin.clone(),
            seat: provision.and_then(|p| p.approvals.clone()),
            person: settings.person_for(&subject).cloned(),
            preset: spec.preset.map(|p| p.approvals()),
            deadline_ms: spec.deadline_ms,
            tools: Mutex::new(None),
            stop: stop.clone(),
        });
        let in_flight = Arc::new((Mutex::new(0usize), Condvar::new()));
        let thread = {
            let (state, in_flight) = (state.clone(), in_flight.clone());
            std::thread::Builder::new()
                .name("by-effects".into())
                .spawn(move || accept(listener, state, in_flight))?
        };
        Ok(EffectProxy {
            state,
            address,
            in_flight,
            thread: Some(thread),
        })
    }

    pub fn port(&self) -> u16 {
        self.address.port()
    }

    fn shut(&mut self) {
        if self.state.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut wake = self.address;
        if wake.ip().is_unspecified() {
            wake.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
        }
        best_effort(
            "connect to wake the accept loop",
            TcpStream::connect_timeout(&wake, Duration::from_secs(1)),
        );
        if let Some(thread) = self.thread.take() {
            branchyard_support::join_reporting("effect proxy accept", thread);
        }
        let (count, idle) = &*self.in_flight;
        let deadline = std::time::Instant::now() + DRAIN;
        let mut active = count.lock_recovering("count");
        while *active > 0 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            active = idle.wait_timeout_recovering(active, left, "idle").0;
        }
    }
}

impl Drop for EffectProxy {
    fn drop(&mut self) {
        self.shut();
    }
}

/// The task a branch serves: the root of its delegation tree.
pub(crate) fn task_of(yard: &Yard, record: &crate::state::Record) -> String {
    let store = yard.store();
    let mut name = record.info.name.clone();
    let mut parent = record.info.parent.clone();
    let mut hops = 0;
    while let Some(p) = parent {
        hops += 1;
        if hops > 64 {
            break;
        }
        name = p.clone();
        parent = store.read(&p).ok().and_then(|r| r.info.parent);
    }
    name
}

fn accept(listener: TcpListener, state: Arc<ProxyState>, in_flight: Arc<(Mutex<usize>, Condvar)>) {
    static THREADS: AtomicUsize = AtomicUsize::new(0);
    for stream in listener.incoming() {
        if state.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let (state, in_flight) = (state.clone(), in_flight.clone());
        *in_flight.0.lock_recovering("in-flight count") += 1;
        let n = THREADS.fetch_add(1, Ordering::Relaxed);
        let spawned = std::thread::Builder::new()
            .name(format!("by-effects-{n}"))
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

/// Hop-by-hop headers, and those the proxy sets itself.
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
            | "accept-encoding"
            | "idempotency-key"
    ) || name.starts_with("proxy-")
}

/// The upstream path for the proxy's `target`: `/mcp...` maps onto the
/// gateway's URL, anything else goes to the gateway's host as it is.
fn upstream_path(upstream: &Target, target: &str) -> (Target, String) {
    let root = Target {
        prefix: String::new(),
        ..upstream.clone()
    };
    match target.strip_prefix("/mcp") {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '?']) => {
            (root, format!("{}{rest}", upstream.prefix))
        }
        _ => (root, target.to_owned()),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        415 => "Unsupported Media Type",
        502 => "Bad Gateway",
        _ => "Error",
    }
}

#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard
fn respond(out: &mut TcpStream, status: u16, headers: &[(String, String)], body: &[u8]) {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "connection" | "keep-alive" | "transfer-encoding" | "content-length" | "te" | "trailer"
        ) {
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let _ = out.write_all(head.as_bytes());
    let _ = out.write_all(body);
    let _ = out.flush();
}

/// Answer a JSON-RPC request without forwarding it: a tool result,
/// marked an error or not, with `meta` under `_meta.branchyard`.
fn tool_answer(out: &mut TcpStream, id: &Value, error: bool, text: &str, meta: Value) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": text}],
            "isError": error,
            "_meta": {"branchyard": meta},
        }
    });
    respond(
        out,
        200,
        &[("Content-Type".into(), "application/json".into())],
        &serde_json::to_vec(&body).unwrap_or_default(),
    );
}

fn rpc_error(out: &mut TcpStream, status: u16, id: &Value, message: &str) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": -32000, "message": message},
    });
    respond(
        out,
        status,
        &[("Content-Type".into(), "application/json".into())],
        &serde_json::to_vec(&body).unwrap_or_default(),
    );
}

/// The gateway's answer, read whole.
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    response_headers: upstream::Response,
}

/// Send `request` (with `body` instead of its own) to the gateway and read
/// the answer whole.
fn forward(
    state: &ProxyState,
    request: &Request,
    body: &[u8],
    extra: &[(String, String)],
) -> Result<Answer, Failure> {
    let target = Target::parse(&state.upstream).map_err(Failure::Connect)?;
    let (root, path) = upstream_path(&target, &request.target);
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|(n, _)| !dropped(n))
        .cloned()
        .collect();
    headers.push(("Accept-Encoding".into(), "identity".into()));
    headers.extend(extra.iter().cloned());
    let mut response = upstream::send(&root, &request.method, &path, &headers, body)?;
    let mut bytes = Vec::new();
    (&mut response.body)
        .take(mcp::MAX_ANSWER as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| Failure::Exchange(e.to_string()))?;
    Ok(Answer {
        status: response.status,
        headers: response.headers.clone(),
        body: bytes,
        response_headers: response,
    })
}

/// Pass the request through as it is.
fn pass(state: &ProxyState, request: &Request, out: &mut TcpStream) {
    match forward(state, request, &request.body, &[]) {
        Ok(answer) => respond(out, answer.status, &answer.headers, &answer.body),
        Err(why) => respond(
            out,
            502,
            &[("Content-Type".into(), "application/json".into())],
            json!({"error": {"code": "gateway_unreachable", "message": format!("Branchyard's effect proxy: {why}")}})
                .to_string()
                .as_bytes(),
        ),
    }
}

/// How a call arrived: MCP's `tools/call`, or Anvil's REST route.
#[derive(Clone, Debug, PartialEq)]
enum Wire {
    Mcp { id: Value },
    Rest,
}

impl Wire {
    /// A refusal from the proxy itself.
    fn error(&self, out: &mut TcpStream, status: u16, code: &str, message: &str) {
        match self {
            Wire::Mcp { id } => rpc_error(out, status, id, message),
            Wire::Rest => rest_answer(
                out,
                status,
                &json!({"error": {"code": code, "message": message}}),
            ),
        }
    }

    /// The approval policy's refusal, with what Branchyard decided.
    fn refused(&self, out: &mut TcpStream, code: &str, message: &str, meta: Value) {
        match self {
            Wire::Mcp { id } => tool_answer(
                out,
                id,
                true,
                &json!({"error": {"code": code, "message": message}}).to_string(),
                meta,
            ),
            Wire::Rest => rest_answer(
                out,
                403,
                &json!({"error": {"code": code, "message": message, "branchyard": meta}}),
            ),
        }
    }

    /// A call held in the outbox: not an error, and not done.
    fn held(&self, out: &mut TcpStream, text: &str, meta: Value) {
        match self {
            Wire::Mcp { id } => tool_answer(out, id, false, text, meta),
            Wire::Rest => rest_answer(
                out,
                202,
                &json!({"staged": true, "message": text, "branchyard": meta}),
            ),
        }
    }

    /// The forwarded body: the key (and the draft flag) where this wire
    /// carries them. The `Idempotency-Key` header is sent on both.
    fn body(&self, request: &Request, key: &str, stage: bool) -> Vec<u8> {
        match self {
            Wire::Mcp { .. } => {
                let mut meta = json!({"idempotency_key": key});
                if stage {
                    meta["stage"] = json!(true);
                }
                with_meta(request, &meta)
            }
            Wire::Rest => {
                let mut body: Value = serde_json::from_slice(&request.body).unwrap_or(json!({}));
                if stage {
                    body["stage"] = json!(true);
                }
                serde_json::to_vec(&body).unwrap_or_default()
            }
        }
    }

    /// The gateway's answer, as the ledger reads it.
    fn called(&self, answer: &Answer) -> mcp::Called {
        match self {
            Wire::Mcp { id } => mcp::called(&answer.response_headers, &answer.body, id),
            Wire::Rest => mcp::rest_called(&answer.response_headers, &answer.body),
        }
    }
}

fn rest_answer(out: &mut TcpStream, status: u16, body: &Value) {
    respond(
        out,
        status,
        &[("Content-Type".into(), "application/json".into())],
        &serde_json::to_vec(body).unwrap_or_default(),
    );
}

/// The tool a REST call names: `POST /call/<tool>`.
fn rest_tool(request: &Request) -> Option<String> {
    if request.method != "POST" {
        return None;
    }
    let path = request.target.split(['?', '#']).next().unwrap_or("");
    let tool = path.strip_prefix("/call/")?;
    // Strict: a malformed escape is not a tool name, so it is not a REST call.
    let well_formed = tool.split('%').skip(1).all(|rest| {
        rest.as_bytes()
            .get(..2)
            .is_some_and(|pair| pair.iter().all(u8::is_ascii_hexdigit))
    });
    if !well_formed {
        return None;
    }
    percent_encoding::percent_decode_str(tool)
        .decode_utf8()
        .ok()
        .map(std::borrow::Cow::into_owned)
}

fn handle(stream: TcpStream, state: &ProxyState) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    best_effort(
        "restore the stream's read timeout",
        stream.set_read_timeout(Some(Duration::from_secs(300))),
    );
    let mut reader = BufReader::new(stream);
    let request = match read_request(&mut reader, &mut out) {
        Ok(Some(request)) => request,
        Ok(None) => return,
        Err(why) => {
            respond(&mut out, 400, &[], why.as_bytes());
            return;
        }
    };
    // A body the proxy cannot read could hold a call it must ledger: an
    // encoded body is refused, never forwarded.
    let encoded = request
        .header("content-encoding")
        .is_some_and(|e| !e.trim().eq_ignore_ascii_case("identity"));
    if request.method == "POST" && encoded {
        respond(
            &mut out,
            415,
            &[],
            b"Branchyard's effect proxy takes requests without a Content-Encoding",
        );
        return;
    }
    let json: Option<Value> = match request.method == "POST" && !request.body.is_empty() {
        true => serde_json::from_slice(&request.body).ok(),
        false => None,
    };
    let (wire, name, arguments, staging) = match rest_tool(&request) {
        // Anvil's REST route: `{"arguments": {...}, "stage": true?}`.
        Some(name) => {
            let Some(body) = json.as_ref().filter(|j| j.is_object()) else {
                return Wire::Rest.error(
                    &mut out,
                    400,
                    "validation_error",
                    "Branchyard's effect proxy: a REST call takes a JSON object body",
                );
            };
            (
                Wire::Rest,
                name,
                body.get("arguments").cloned().unwrap_or(json!({})),
                body.get("stage") == Some(&Value::Bool(true)),
            )
        }
        None => {
            let is_call = |j: &Value| j.get("method").and_then(Value::as_str) == Some("tools/call");
            // A batch holding a call is refused: each call is ledgered on
            // its own.
            if let Some(Value::Array(items)) = &json {
                if items.iter().any(is_call) {
                    rpc_error(
                        &mut out,
                        400,
                        &Value::Null,
                        "Branchyard's effect proxy takes one tools/call per request, not a batch",
                    );
                    return;
                }
            }
            let Some(call) = json.as_ref().filter(|j| is_call(j)) else {
                return pass(state, &request, &mut out);
            };
            let params = &call["params"];
            (
                Wire::Mcp {
                    id: call.get("id").cloned().unwrap_or(Value::Null),
                },
                params["name"].as_str().unwrap_or("").to_owned(),
                params.get("arguments").cloned().unwrap_or(json!({})),
                params["_meta"]["stage"] == Value::Bool(true),
            )
        }
    };
    // Only this turn's token: what is ledgered is this turn's.
    if !presented_is(state, &request) {
        return wire.error(
            &mut out,
            401,
            "auth_required",
            "Branchyard's effect proxy: the token is not this turn's",
        );
    }
    let (connector, operation) = match name.split_once("__") {
        Some((c, o)) => (c.to_owned(), o.to_owned()),
        None => (String::new(), name.clone()),
    };
    // A connector the grant only reads cannot change anything: the
    // gateway refuses its writes itself. A name without a connector is
    // ledgered as the worst case.
    if !connector.is_empty() && !state.writes(&connector) {
        return pass(state, &request, &mut out);
    }
    let decl = state.decl(&name);
    let class = decl.class();
    if class == EffectClass::Read {
        return pass(state, &request, &mut out);
    }
    let deletion =
        is_deletion_name(&operation) || decl.operation.as_deref().is_some_and(is_deletion_name);
    let subject = Subject::Operation {
        connector: &connector,
        operation: &operation,
        alias: decl.operation.as_deref(),
        class,
        deletion,
    };
    let resolved = super::resolve(&state.layers(), &subject).unwrap_or(Resolved {
        approval: class.default_approval(),
        layer: super::Layer::Default,
        rule: None,
    });
    let call = Call {
        wire,
        name,
        connector,
        operation,
        arguments,
        staging,
        class,
        deletion,
        decl,
        resolved: resolved.clone(),
    };
    match resolved.approval {
        Approval::Block => {
            ask::note(
                &state.yard,
                &state.branch,
                EffectActivity::Blocked {
                    connector: call.connector.clone(),
                    operation: call.operation.clone(),
                    resolved: resolved.clone(),
                },
            );
            call.wire.refused(
                &mut out,
                "approval_blocked",
                &format!(
                    "Branchyard's approval policy blocks {} {} ({})",
                    call.connector,
                    call.operation,
                    resolved.describe()
                ),
                json!({"approval": "block"}),
            );
        }
        Approval::Allow => {
            let approval = ApprovalRecord {
                by: "policy".into(),
                at_ms: now_ms(),
                surface: "policy".into(),
                allowed: true,
                reason: Some(resolved.describe()),
            };
            perform(state, &request, &call, approval, &mut out);
        }
        Approval::Ask => {
            let asked = ask::open(
                &state.yard,
                AskSpec {
                    branch: state.branch.clone(),
                    turn: state.turn,
                    subject: state.subject.clone(),
                    about: AskAbout::Operation {
                        connector: call.connector.clone(),
                        operation: call.operation.clone(),
                        class: call.class,
                        deletion: call.deletion,
                    },
                    effect: None,
                    resolved: resolved.clone(),
                    request: Some(call.arguments.clone()),
                    deadline_ms: state.deadline_ms,
                },
            );
            let answer = asked.and_then(|asked| {
                let stop = || state.stop.load(Ordering::SeqCst);
                ask::wait(&state.yard, &asked, &stop)
            });
            match answer {
                Ok(answer) if answer.allow => {
                    perform(state, &request, &call, answer.record(), &mut out)
                }
                Ok(answer) => call.wire.refused(
                    &mut out,
                    "approval_denied",
                    &format!(
                        "{} {} was not approved: denied by {} ({}){}",
                        call.connector,
                        call.operation,
                        answer.by,
                        answer.surface,
                        answer.reason.map(|r| format!(": {r}")).unwrap_or_default()
                    ),
                    json!({"approval": "denied"}),
                ),
                Err(why) => call.wire.error(
                    &mut out,
                    502,
                    "approval_unavailable",
                    &format!("Branchyard's effect proxy could not ask: {why}"),
                ),
            }
        }
        Approval::Stage => stage(state, &request, &call, &mut out),
    }
}

fn presented_is(state: &ProxyState, request: &Request) -> bool {
    presented(request).is_some_and(|token| same(token, &state.token))
}

/// One call, as the proxy sees it.
struct Call {
    wire: Wire,
    name: String,
    connector: String,
    operation: String,
    arguments: Value,
    /// The harness asked for the draft form itself.
    staging: bool,
    class: EffectClass,
    deletion: bool,
    decl: ToolDecl,
    resolved: Resolved,
}

impl Call {
    fn entry(&self, state: &ProxyState, id: &str, entry_state: EffectState) -> EffectEntry {
        let now = now_ms();
        EffectEntry {
            id: id.to_owned(),
            task: state.task.clone(),
            branch: state.branch.clone(),
            turn: state.turn,
            subject: state.subject.clone(),
            connector: self.connector.clone(),
            operation: self.operation.clone(),
            operation_id: self.decl.operation.clone(),
            account: state
                .grant
                .iter()
                .find(|g| g.connector == self.connector)
                .and_then(|g| g.account.clone()),
            class: self.class,
            state: entry_state,
            request_digest: request_digest(&self.connector, &self.operation, &self.arguments),
            undo: None,
            compensate: None,
            undo_unavailable: None,
            approval: None,
            decided: Some(self.resolved.clone()),
            undo_approval: None,
            deletion: self.deletion,
            staged: None,
            // The declared lookup, resolved from the request and the key
            // before the call: what settles an answer lost entirely.
            lookup: self.decl.lookup.as_ref().and_then(|declared| {
                mcp::resolve_lookup(declared, &self.arguments, id, &state.operations())
            }),
            declared: false,
            detail: None,
            upstream_key: None,
            created_ms: now,
            updated_ms: now,
        }
    }
}

/// The request with `_meta` merged into its params.
fn with_meta(request: &Request, meta: &Value) -> Vec<u8> {
    let mut body: Value = serde_json::from_slice(&request.body).unwrap_or(json!({}));
    let params = &mut body["params"];
    if !params.is_object() {
        *params = json!({});
    }
    let existing = params
        .get("_meta")
        .cloned()
        .filter(Value::is_object)
        .unwrap_or(json!({}));
    let mut merged = existing;
    if let (Some(m), Some(add)) = (merged.as_object_mut(), meta.as_object()) {
        for (k, v) in add {
            m.insert(k.clone(), v.clone());
        }
    }
    params["_meta"] = merged;
    serde_json::to_vec(&body).unwrap_or_default()
}

/// What a finished call moves its entry to.
pub(crate) fn finish_move(called: &mcp::Called) -> EffectMove {
    if !called.ok() {
        // A failed call still reports its class, key and lookup.
        let mut change = EffectMove::to(EffectState::Failed).detail(called.answer());
        if let Some(meta) = &called.meta {
            change.lookup = meta.lookup.clone();
            change.upstream_key = meta.idempotency_key.clone();
            change.operation_id = meta.operation.clone();
        }
        return change;
    }
    match &called.meta {
        Some(meta) => described(meta, EffectState::Confirmed).detail(called.answer()),
        None => EffectMove {
            state: Some(EffectState::Confirmed),
            class: Some(EffectClass::Irreversible),
            no_undo: true,
            declared: Some(false),
            detail: Some(format!(
                "{}; the gateway described no effect, so it is irreversible",
                called.answer()
            )),
            ..EffectMove::default()
        },
    }
}

/// A move to `state` carrying what `meta` says.
pub(crate) fn described(meta: &EffectMeta, state: EffectState) -> EffectMove {
    let mut change = EffectMove::to(state);
    change.declared = Some(true);
    change.class = meta.class;
    change.operation_id = meta.operation.clone();
    match &meta.undo {
        Some(Some(undo)) => change.undo = Some(undo.clone()),
        Some(None) => change.no_undo = true,
        None => {}
    }
    change.compensate = meta.compensate.clone();
    change.undo_unavailable = meta.undo_unavailable.clone();
    change.lookup = meta.lookup.clone();
    change.upstream_key = meta.idempotency_key.clone();
    change.draft = meta.staged.clone();
    change
}

/// Write the entry `begun`, forward the call with its id as the
/// idempotency key, finish the entry from the answer, and give the answer
/// to the harness.
fn perform(
    state: &ProxyState,
    request: &Request,
    call: &Call,
    approval: ApprovalRecord,
    out: &mut TcpStream,
) {
    let ledger = state.yard.store();
    let id = match ulid(now_ms()) {
        Ok(id) => id,
        Err(why) => {
            return call
                .wire
                .error(out, 502, "ledger_unavailable", &why.to_string())
        }
    };
    let mut entry = call.entry(state, &id, EffectState::Begun);
    entry.approval = Some(approval);
    // Begun before the call: a ledger that cannot be written refuses it.
    if let Err(why) = ledger.effects().open_effect(&entry) {
        return call.wire.error(
            out,
            502,
            "ledger_unavailable",
            &format!(
                "Branchyard's effect ledger could not record the call, so it was not made: {why}"
            ),
        );
    }
    ask::note(&state.yard, &state.branch, EffectActivity::of(&entry));
    let body = call.wire.body(request, &id, call.staging);
    let extra = [("Idempotency-Key".to_owned(), id.clone())];
    let (change, reply) = match forward(state, request, &body, &extra) {
        Err(Failure::Connect(why)) => (
            EffectMove::to(EffectState::Failed).detail(format!("not sent: {why}")),
            Err(format!("could not reach the gateway: {why}")),
        ),
        Err(Failure::Exchange(why)) => (
            EffectMove::to(EffectState::Unknown)
                .detail(format!("the gateway's answer was lost: {why}")),
            Err(format!("the gateway's answer was lost: {why}")),
        ),
        Ok(answer) => {
            let called = call.wire.called(&answer);
            let change = match (called.answered, answer.status) {
                // No answer from a failing gateway: the upstream may have
                // been called.
                (false, status) if status >= 500 => {
                    EffectMove::to(EffectState::Unknown).detail(called.answer())
                }
                _ => finish_move(&called),
            };
            (change, Ok(answer))
        }
    };
    if let Ok(Some(done)) =
        ledger
            .effects()
            .move_effect(&id, &[EffectState::Begun], &change, now_ms())
    {
        ask::note(&state.yard, &state.branch, EffectActivity::of(&done));
    }
    match reply {
        Ok(answer) => respond(out, answer.status, &answer.headers, &answer.body),
        Err(why) => call.wire.error(
            out,
            502,
            "gateway_unreachable",
            &format!("Branchyard's effect proxy: {why}"),
        ),
    }
}

/// Stage the call: its draft form when it declares one, else the outbox.
fn stage(state: &ProxyState, request: &Request, call: &Call, out: &mut TcpStream) {
    let ledger = state.yard.store();
    let id = match ulid(now_ms()) {
        Ok(id) => id,
        Err(why) => {
            return call
                .wire
                .error(out, 502, "ledger_unavailable", &why.to_string())
        }
    };
    // The ask that holds it, with what is performed when it is approved.
    let asked = ask::open(
        &state.yard,
        AskSpec {
            branch: state.branch.clone(),
            turn: state.turn,
            subject: state.subject.clone(),
            about: AskAbout::Promote {
                connector: call.connector.clone(),
                operation: call.operation.clone(),
                class: call.class,
            },
            effect: Some(id.clone()),
            resolved: call.resolved.clone(),
            request: Some(json!({"name": call.name, "arguments": call.arguments})),
            deadline_ms: None,
        },
    );
    let asked = match asked {
        Ok(asked) => asked,
        Err(why) => {
            return call.wire.error(
                out,
                502,
                "ledger_unavailable",
                &format!("Branchyard could not stage the call, so it was not made: {why}"),
            )
        }
    };
    let mut entry = call.entry(state, &id, EffectState::Staged);
    entry.staged = Some(Staged {
        draft: None,
        ask: asked.id.clone(),
    });
    if let Err(why) = ledger.effects().open_effect(&entry) {
        return call.wire.error(
            out,
            502,
            "ledger_unavailable",
            &format!(
                "Branchyard's effect ledger could not record the call, so it was not made: {why}"
            ),
        );
    }
    ask::note(&state.yard, &state.branch, EffectActivity::of(&entry));
    if !call.decl.draft {
        let text = format!(
            "Branchyard staged this call: {} {} is held until a person approves it (effect {}, \
             approval {}). It has not happened.",
            call.connector, call.operation, id, asked.id
        );
        return call.wire.held(
            out,
            &text,
            json!({"effect": id, "state": "staged", "approval": asked.id}),
        );
    }
    // The draft form: the call with `stage: true`, under the same key.
    let body = call.wire.body(request, &id, true);
    let extra = [("Idempotency-Key".to_owned(), id.clone())];
    match forward(state, request, &body, &extra) {
        Ok(answer) => {
            let called = call.wire.called(&answer);
            let change = match (
                called.ok(),
                called.meta.as_ref().and_then(|m| m.staged.clone()),
            ) {
                (true, Some(draft)) => EffectMove {
                    detail: Some(match (&draft.promote, &draft.unavailable) {
                        (Some(_), _) => {
                            "a draft was made; the real effect waits for approval".to_owned()
                        }
                        (None, Some(why)) => {
                            format!("a draft was made, but cannot be promoted: {why}")
                        }
                        (None, None) => "a draft was made, but cannot be promoted".to_owned(),
                    }),
                    lookup: called.meta.as_ref().and_then(|m| m.lookup.clone()),
                    draft: Some(draft),
                    ..EffectMove::default()
                },
                (true, None) => {
                    EffectMove::default().detail("the gateway staged it but described no draft")
                }
                (false, _) => EffectMove::to(EffectState::Failed)
                    .detail(format!("the draft failed: {}", called.answer())),
            };
            if let Ok(Some(done)) =
                ledger
                    .effects()
                    .move_effect(&id, &[EffectState::Staged], &change, now_ms())
            {
                ask::note(&state.yard, &state.branch, EffectActivity::of(&done));
            }
            respond(out, answer.status, &answer.headers, &answer.body)
        }
        Err(why) => {
            let lost = matches!(why, Failure::Exchange(_));
            let change = EffectMove::to(match lost {
                true => EffectState::Unknown,
                false => EffectState::Failed,
            })
            .detail(format!("the draft call: {why}"));
            best_effort(
                "move the effect in the ledger",
                ledger
                    .effects()
                    .move_effect(&id, &[EffectState::Staged], &change, now_ms()),
            );
            call.wire.error(
                out,
                502,
                "gateway_unreachable",
                &format!("Branchyard's effect proxy: {why}"),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_paths_map_onto_the_gateway() {
        let upstream = Target::parse("http://127.0.0.1:8931/mcp").unwrap();
        let (root, path) = upstream_path(&upstream, "/mcp");
        assert_eq!((root.prefix.as_str(), path.as_str()), ("", "/mcp"));
        assert_eq!(upstream_path(&upstream, "/mcp?x=1").1, "/mcp?x=1");
        assert_eq!(
            upstream_path(&upstream, "/connect/start").1,
            "/connect/start"
        );
        assert_eq!(upstream_path(&upstream, "/mcpx").1, "/mcpx");
        assert_eq!(upstream_path(&upstream, "/call/g__x").1, "/call/g__x");
    }

    fn request(method: &str, target: &str, body: &[u8]) -> Request {
        Request {
            method: method.into(),
            target: target.into(),
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }

    #[test]
    fn meta_is_merged_into_the_call() {
        let request = request(
            "POST",
            "/mcp",
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"g__x","_meta":{"progressToken":3}}}"#,
        );
        let body: Value =
            serde_json::from_slice(&with_meta(&request, &json!({"idempotency_key": "K"}))).unwrap();
        assert_eq!(
            body["params"]["_meta"],
            json!({"progressToken": 3, "idempotency_key": "K"})
        );
        assert_eq!(body["params"]["name"], "g__x");
        let staged: Value =
            serde_json::from_slice(&Wire::Mcp { id: json!(1) }.body(&request, "K", true)).unwrap();
        assert_eq!(staged["params"]["_meta"]["stage"], true);
    }

    #[test]
    fn rest_calls_name_their_tool_and_stage_in_the_body() {
        let rest = request(
            "POST",
            "/call/github__create%5Fcomment?x=1",
            br#"{"arguments":{}}"#,
        );
        assert_eq!(rest_tool(&rest).as_deref(), Some("github__create_comment"));
        assert_eq!(rest_tool(&request("GET", "/call/g__x", b"")), None);
        assert_eq!(rest_tool(&request("POST", "/calls/g__x", b"")), None);
        assert_eq!(rest_tool(&request("POST", "/call/g__%4", b"")), None);
        assert_eq!(rest_tool(&request("POST", "/call/g__%zz", b"")), None);
        // An escape at the very end of the name decodes.
        assert_eq!(
            rest_tool(&request("POST", "/call/g__x%5F", b"")).as_deref(),
            Some("g__x_")
        );
        let body: Value = serde_json::from_slice(&Wire::Rest.body(&rest, "K", true)).unwrap();
        assert_eq!(body, json!({"arguments": {}, "stage": true}));
    }
}
