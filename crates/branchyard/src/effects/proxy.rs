//! A turn's effect-ledger proxy: an HTTP/1.1 reverse proxy in front of the
//! connector gateway, on `std` threads like the model gateway, for as long
//! as the turn runs. The harness is given it as `ANVIL_GATEWAY_URL`.
//!
//! Everything but a `tools/call` passes through as it is (initialize,
//! `tools/list`, session deletes, Anvil's REST shapes). A `tools/call`:
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
//!    and `_meta.idempotency_key`; a ledger that cannot be written refuses
//!    the call;
//! 4. is finished from the answer: `confirmed` with the gateway's
//!    `_meta.effect` (class, undo, deadline), or irreversible with no undo
//!    when the gateway described nothing; `failed` on a refusal or a tool
//!    error; `unknown` when the answer was lost after the call was sent.

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
use crate::state::now_ms;
use crate::Yard;

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
    /// What `tools/list` declared, by wire name; listed once.
    tools: Mutex<Option<HashMap<String, ToolDecl>>>,
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

    fn decl(&self, tool: &str) -> ToolDecl {
        let mut tools = self.tools.lock().unwrap_or_else(|e| e.into_inner());
        if tools.is_none() {
            let listed = mcp::Client::new(&self.upstream, &self.token)
                .list_tools()
                .unwrap_or_default();
            *tools = Some(
                listed
                    .iter()
                    .filter_map(|t| Some((t.get("name")?.as_str()?.to_owned(), ToolDecl::read(t))))
                    .collect(),
            );
        }
        tools
            .as_ref()
            .and_then(|t| t.get(tool).cloned())
            .unwrap_or_default()
    }

    /// Whether the grant lets this connector change anything.
    fn writes(&self, connector: &str) -> bool {
        self.grant
            .iter()
            .any(|g| g.connector == connector && g.mode == GrantMode::Write)
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
        let _ = TcpStream::connect_timeout(&wake, Duration::from_secs(1));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let (count, idle) = &*self.in_flight;
        let deadline = std::time::Instant::now() + DRAIN;
        let mut active = count.lock().unwrap_or_else(|e| e.into_inner());
        while *active > 0 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
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
        *in_flight.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        let n = THREADS.fetch_add(1, Ordering::Relaxed);
        let spawned = std::thread::Builder::new()
            .name(format!("by-effects-{n}"))
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
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Error",
    }
}

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

fn handle(stream: TcpStream, state: &ProxyState) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let mut reader = BufReader::new(stream);
    let request = match read_request(&mut reader, &mut out) {
        Ok(Some(request)) => request,
        Ok(None) => return,
        Err(why) => {
            respond(&mut out, 400, &[], why.as_bytes());
            return;
        }
    };
    let json: Option<Value> = match request.method == "POST" && !request.body.is_empty() {
        true => serde_json::from_slice(&request.body).ok(),
        false => None,
    };
    let call = json
        .as_ref()
        .filter(|j| j.get("method").and_then(Value::as_str) == Some("tools/call"));
    let Some(call) = call else {
        return pass(state, &request, &mut out);
    };
    let id = call.get("id").cloned().unwrap_or(Value::Null);
    // Only this turn's token: what is ledgered is this turn's.
    if !presented_is(state, &request) {
        rpc_error(
            &mut out,
            401,
            &id,
            "Branchyard's effect proxy: the token is not this turn's",
        );
        return;
    }
    let name = call["params"]["name"].as_str().unwrap_or("").to_owned();
    let arguments = call["params"]
        .get("arguments")
        .cloned()
        .unwrap_or(json!({}));
    let (connector, operation) = match name.split_once("__") {
        Some((c, o)) => (c.to_owned(), o.to_owned()),
        None => (String::new(), name.clone()),
    };
    // A connector the grant only reads cannot change anything: the
    // gateway refuses its writes itself.
    if !state.writes(&connector) {
        return pass(state, &request, &mut out);
    }
    let decl = state.decl(&name);
    let class = decl.class();
    if class == EffectClass::Read {
        return pass(state, &request, &mut out);
    }
    let deletion = decl.deletion
        || is_deletion_name(&operation)
        || decl.operation.as_deref().is_some_and(is_deletion_name);
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
        id,
        name,
        connector,
        operation,
        arguments,
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
            tool_answer(
                &mut out,
                &call.id,
                true,
                &json!({"error": {"code": "approval_blocked", "message": format!(
                    "Branchyard's approval policy blocks {} {} ({})",
                    call.connector, call.operation, resolved.describe())}})
                .to_string(),
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
                Ok(answer) => tool_answer(
                    &mut out,
                    &call.id,
                    true,
                    &json!({"error": {"code": "approval_denied", "message": format!(
                        "{} {} was not approved: denied by {} ({}){}",
                        call.connector, call.operation, answer.by, answer.surface,
                        answer.reason.map(|r| format!(": {r}")).unwrap_or_default())}})
                    .to_string(),
                    json!({"approval": "denied"}),
                ),
                Err(why) => rpc_error(
                    &mut out,
                    502,
                    &call.id,
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

/// One `tools/call`, as the proxy sees it.
struct Call {
    id: Value,
    name: String,
    connector: String,
    operation: String,
    arguments: Value,
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
            account: state
                .grant
                .iter()
                .find(|g| g.connector == self.connector)
                .and_then(|g| g.account.clone()),
            class: self.class,
            state: entry_state,
            request_digest: request_digest(&self.connector, &self.operation, &self.arguments),
            undo: None,
            approval: None,
            decided: Some(self.resolved.clone()),
            undo_approval: None,
            deletion: self.deletion,
            staged: None,
            lookup: self.decl.lookup.clone(),
            declared: false,
            summary: None,
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
        return EffectMove::to(EffectState::Failed).detail(called.answer());
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
    match &meta.undo {
        Some(Some(undo)) => change.undo = Some(undo.clone()),
        Some(None) => change.no_undo = true,
        None => {}
    }
    change.summary = meta.summary.clone();
    change.upstream_key = meta.idempotency_key.clone();
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
        Err(why) => return rpc_error(out, 502, &call.id, &why.to_string()),
    };
    let mut entry = call.entry(state, &id, EffectState::Begun);
    entry.approval = Some(approval);
    // Begun before the call: a ledger that cannot be written refuses it.
    if let Err(why) = ledger.effects().open_effect(&entry) {
        return rpc_error(
            out,
            502,
            &call.id,
            &format!(
                "Branchyard's effect ledger could not record the call, so it was not made: {why}"
            ),
        );
    }
    ask::note(&state.yard, &state.branch, EffectActivity::of(&entry));
    let body = with_meta(request, &json!({"idempotency_key": id}));
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
            let called = mcp::called(&answer.response_headers, &answer.body, &call.id);
            let change = match (called.answered, answer.status) {
                // No JSON-RPC answer from a failing gateway: the upstream
                // may have been called.
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
        Err(why) => rpc_error(
            out,
            502,
            &call.id,
            &format!("Branchyard's effect proxy: {why}"),
        ),
    }
}

/// Stage the call: its draft form when it declares one, else the outbox.
fn stage(state: &ProxyState, request: &Request, call: &Call, out: &mut TcpStream) {
    let ledger = state.yard.store();
    let id = match ulid(now_ms()) {
        Ok(id) => id,
        Err(why) => return rpc_error(out, 502, &call.id, &why.to_string()),
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
            return rpc_error(
                out,
                502,
                &call.id,
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
        return rpc_error(
            out,
            502,
            &call.id,
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
        return tool_answer(
            out,
            &call.id,
            false,
            &text,
            json!({"effect": id, "state": "staged", "approval": asked.id}),
        );
    }
    // The draft form: the call with `stage: true`, under the same key.
    let body = with_meta(request, &json!({"idempotency_key": id, "stage": true}));
    let extra = [("Idempotency-Key".to_owned(), id.clone())];
    match forward(state, request, &body, &extra) {
        Ok(answer) => {
            let called = mcp::called(&answer.response_headers, &answer.body, &call.id);
            let change = match (
                called.ok(),
                called.meta.as_ref().and_then(|m| m.draft.clone()),
            ) {
                (true, Some(draft)) => EffectMove {
                    draft: Some(draft),
                    detail: Some("a draft was made; the real effect waits for approval".into()),
                    ..EffectMove::default()
                },
                (true, None) => {
                    EffectMove::default().detail("the gateway staged it but named no draft handle")
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
            let _ = ledger
                .effects()
                .move_effect(&id, &[EffectState::Staged], &change, now_ms());
            rpc_error(
                out,
                502,
                &call.id,
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
    }

    #[test]
    fn meta_is_merged_into_the_call() {
        let request = Request {
            method: "POST".into(),
            target: "/mcp".into(),
            headers: Vec::new(),
            body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"g__x","_meta":{"progressToken":3}}}"#.to_vec(),
        };
        let body: Value =
            serde_json::from_slice(&with_meta(&request, &json!({"idempotency_key": "K"}))).unwrap();
        assert_eq!(
            body["params"]["_meta"],
            json!({"progressToken": 3, "idempotency_key": "K"})
        );
        assert_eq!(body["params"]["name"], "g__x");
    }
}
