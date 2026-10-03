//! A mock connector gateway on loopback for the effect ledger's tests,
//! speaking Anvil's wire (ADR-0030, Anvil's `docs/branchyard.md`, "Effects
//! and undo"): MCP Streamable HTTP (JSON, and one tool answered as an event
//! stream) and the REST route `POST /call/<tool>`; `tools/list` publishing
//! `anvil/operation_id`, `anvil/effect_class` and `anvil/effect_contract`;
//! each call's effect report (`_meta.effect`, or `X-Anvil-Effect` over
//! REST) with follow-ups named by `tool`; idempotency keys honoured (a key
//! seen before replays its answer without doing anything again); drafts
//! (`_meta.stage` / `"stage": true`) with promote and discard calls; lookups
//! answering `not_found`; and every call recorded. Shared by the SDK, CLI
//! and server tests through `#[path]`.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};

use serde_json::{json, Value};

/// One call the gateway received.
#[derive(Clone, Debug)]
pub struct Call {
    pub tool: String,
    pub arguments: Value,
    /// `_meta` over MCP; `{"stage": ...}` from the body over REST.
    pub meta: Value,
    /// The `Idempotency-Key` header.
    pub key: Option<String>,
    /// Whether it came over the REST route.
    pub rest: bool,
    /// Whether it replayed an earlier answer for the same key.
    pub replayed: bool,
    pub token: String,
    /// What the hook saw when the call arrived.
    pub seen: Option<String>,
}

impl Call {
    /// Whether it asked for the draft form.
    pub fn staged(&self) -> bool {
        self.meta["stage"] == json!(true)
    }
}

type Hook = Box<dyn Fn(&Call) -> Option<String> + Send>;

#[derive(Default)]
struct State {
    calls: Vec<Call>,
    /// Answers by tool and idempotency key.
    performed: HashMap<String, Outcome>,
    /// Keys whose effect happened, for lookups.
    happened: HashSet<String>,
    next: u64,
    deadline_ms: u64,
    fail_inverse: bool,
    no_compensation: bool,
    /// Tools held until released, and whether they are held now.
    hold: HashSet<String>,
    holding: usize,
    hook: Option<Hook>,
}

/// The gateway; stops when dropped.
pub struct MockGateway {
    pub url: String,
    pub port: u16,
    state: Arc<(Mutex<State>, Condvar)>,
}

fn tool(name: &str, operation: &str, contract: Value) -> Value {
    let mut meta = json!({"anvil/operation_id": operation});
    if let Some(class) = contract.get("class") {
        meta["anvil/effect_class"] = class.clone();
        meta["anvil/effect_contract"] = contract.clone();
    }
    let mut tool = json!({"name": name, "_meta": meta,
        "inputSchema": {"type": "object", "properties": {}}});
    if contract.get("class") == Some(&json!("read")) {
        tool["annotations"] = json!({"readOnlyHint": true});
    }
    tool
}

/// The tools it lists, as Anvil declares them.
pub fn tools() -> Value {
    json!([
        tool("slack__chat_post", "slack.chat.post", json!({"class": "reversible",
            "inverse": {"operation": "slack.chat.delete", "arguments": {"ts": "response.ts"},
                        "deadline": {"within_ms": 86_400_000}},
            "compensate": {"operation": "slack.chat.update",
                           "arguments": {"ts": "response.ts", "text": {"const": "(retracted)"}}}})),
        tool("slack__chat_delete", "slack.chat.delete", json!({"class": "irreversible"})),
        tool("slack__chat_update", "slack.chat.update", json!({})),
        tool("github__issues_create", "github.issues.create", json!({"class": "compensable",
            "compensate": {"operation": "github.issues.update",
                           "arguments": {"number": "response.number", "state": {"const": "closed"}}}})),
        {
            // The compensation needs confirmation, as Anvil's schema says.
            "name": "github__issues_close",
            "_meta": {"anvil/operation_id": "github.issues.update"},
            "inputSchema": {"type": "object", "properties": {
                "number": {"type": "integer"}, "state": {"type": "string"},
                "confirm": {"type": "boolean", "const": true}},
                "required": ["number", "confirm"]},
        },
        tool("github__issues_list", "github.issues.list", json!({"class": "read"})),
        tool("gmail__send", "gmail.send", json!({"class": "irreversible",
            "lookup": {"operation": "gmail.sent.lookup", "by": "key",
                       "arguments": {"key": "idempotency_key"}},
            "draft": {"operation": "gmail.drafts.create", "arguments": {"to": "request.to"},
                      "handle": "response.id",
                      "promote": {"operation": "gmail.drafts.send", "arguments": {"id": "response.id"}},
                      "discard": {"operation": "gmail.drafts.delete", "arguments": {"id": "response.id"}}}})),
        tool("gmail__drafts_send", "gmail.drafts.send", json!({"class": "irreversible"})),
        tool("gmail__drafts_delete", "gmail.drafts.delete", json!({"class": "irreversible"})),
        tool("gmail__sent_lookup", "gmail.sent.lookup", json!({"class": "read"})),
        tool("blog__publish", "blog.posts.publish", json!({"class": "irreversible",
            "draft": {"operation": "blog.posts.create", "arguments": {"draft": {"const": true}},
                      "handle": "response.id",
                      "promote": {"operation": "blog.posts.update",
                                  "arguments": {"id": "response.id", "draft": {"const": false}}}}})),
        tool("blog__posts_update", "blog.posts.update", json!({"class": "irreversible"})),
        tool("webhook__fire", "webhook.fire", json!({"class": "irreversible"})),
        {"name": "legacy__do"},
        tool("flaky__charge", "flaky.charges.create", json!({"class": "compensable",
            "compensate": {"operation": "flaky.refunds.create", "arguments": {"charge": "response.id"}},
            "lookup": {"operation": "flaky.charges.lookup", "by": "key",
                       "arguments": {"key": "idempotency_key", "kind": {"const": "charge"}}}})),
        tool("flaky__charge_lookup", "flaky.charges.lookup", json!({"class": "read"})),
        tool("flaky__refund", "flaky.refunds.create", json!({"class": "irreversible"})),
    ])
}

impl MockGateway {
    pub fn start() -> MockGateway {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: Arc<(Mutex<State>, Condvar)> = Arc::default();
        state.0.lock().unwrap().deadline_ms = u64::MAX / 2;
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let shared = shared.clone();
                std::thread::spawn(move || serve(stream, &shared));
            }
        });
        MockGateway {
            url: format!("http://127.0.0.1:{port}/mcp"),
            port,
            state,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.0.lock().unwrap()
    }

    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// The calls to `tool` (its wire name).
    pub fn calls_to(&self, tool: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| c.tool == tool)
            .collect()
    }

    /// The deadline every inverse it reports carries.
    pub fn set_deadline(&self, ms: u64) {
        self.lock().deadline_ms = ms;
    }

    /// Report no compensation beside an inverse.
    pub fn without_compensation(&self) {
        self.lock().no_compensation = true;
    }

    /// Make every inverse and compensation (`chat_delete`, `issues_close`)
    /// fail.
    pub fn fail_inverses(&self) {
        self.lock().fail_inverse = true;
    }

    /// Hold calls to `tool` once they have done their effect, until
    /// [`MockGateway::release`]: an engine killed meanwhile never sees the
    /// answer.
    pub fn hold(&self, tool: &str) {
        self.lock().hold.insert(tool.to_owned());
    }

    /// Wait until a held call arrived.
    pub fn wait_held(&self) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().unwrap();
        while state.holding == 0 {
            state = changed.wait(state).unwrap();
        }
    }

    pub fn release(&self) {
        let (lock, changed) = &*self.state;
        lock.lock().unwrap().hold.clear();
        changed.notify_all();
    }

    /// Run `hook` on each call as it arrives; what it returns is kept as
    /// the call's `seen`.
    pub fn on_call(&self, hook: impl Fn(&Call) -> Option<String> + Send + 'static) {
        self.lock().hook = Some(Box::new(hook));
    }
}

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

fn read(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut words = line.split_whitespace();
    let method = words.next()?.to_owned();
    let target = words.next()?.to_owned();
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).ok()?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        let (name, value) = trimmed.split_once(':')?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let length: usize = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        target,
        headers,
        body,
    })
}

fn write(mut stream: &TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
    let mut head = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// What one call did: its data or its error, and its effect report.
#[derive(Clone, Debug)]
struct Outcome {
    data: Value,
    /// `(code, message)` for a refusal or a failure.
    error: Option<(String, String)>,
    effect: Option<Value>,
}

impl Outcome {
    fn ok(data: Value, effect: Value) -> Outcome {
        Outcome {
            data,
            error: None,
            effect: Some(effect),
        }
    }

    fn error(code: &str, message: &str, effect: Option<Value>) -> Outcome {
        Outcome {
            data: Value::Null,
            error: Some((code.to_owned(), message.to_owned())),
            effect,
        }
    }

    fn text(&self) -> String {
        match (&self.error, &self.data) {
            (Some((code, message)), _) => {
                json!({"error": {"code": code, "message": message}}).to_string()
            }
            (None, Value::String(text)) => text.clone(),
            (None, data) => data.to_string(),
        }
    }

    /// As an MCP `tools/call` result.
    fn mcp(&self) -> Value {
        let mut result = json!({"content": [{"type": "text", "text": self.text()}],
                                "isError": self.error.is_some()});
        if let Some(effect) = &self.effect {
            result["_meta"] = json!({"effect": effect});
        }
        result
    }

    /// As a REST answer: status, headers, body.
    fn rest(&self) -> (u16, Vec<(&'static str, String)>, Vec<u8>) {
        let mut headers = vec![("Content-Type", "application/json".to_owned())];
        if let Some(effect) = &self.effect {
            headers.push(("X-Anvil-Effect", effect.to_string()));
        }
        match &self.error {
            Some((code, message)) => {
                let status = match code.as_str() {
                    "not_found" => 404,
                    "unsupported_operation" => 422,
                    _ => 502,
                };
                let body = json!({"error": {"code": code, "message": message}});
                (status, headers, body.to_string().into_bytes())
            }
            None => (200, headers, self.data.to_string().into_bytes()),
        }
    }
}

fn serve(stream: TcpStream, shared: &Arc<(Mutex<State>, Condvar)>) {
    let Some(request) = read(&stream) else {
        return;
    };
    if request.method == "DELETE" {
        return write(&stream, 200, &[], b"");
    }
    let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
        return write(&stream, 400, &[], b"not json");
    };
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let token = request
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_owned();
    if token.is_empty() {
        return write(&stream, 401, &[], b"no token");
    }
    if let Some(tool) = request.target.strip_prefix("/call/") {
        let meta = json!({"stage": body.get("stage").cloned().unwrap_or(Value::Null)});
        let arguments = body.get("arguments").cloned().unwrap_or(json!({}));
        return call(stream, shared, &request, tool, arguments, meta, token, None);
    }
    let answer = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
    let json_type = ("Content-Type", "application/json".to_owned());
    match body["method"].as_str().unwrap_or("") {
        "initialize" => write(
            &stream,
            200,
            &[json_type, ("Mcp-Session-Id", "s-1".into())],
            answer(
                json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                          "serverInfo": {"name": "mock", "version": "1"}}),
            )
            .to_string()
            .as_bytes(),
        ),
        "notifications/initialized" => write(&stream, 202, &[], b""),
        "tools/list" => write(
            &stream,
            200,
            &[json_type],
            answer(json!({"tools": tools()})).to_string().as_bytes(),
        ),
        "tools/call" => {
            let params = &body["params"];
            let tool = params["name"].as_str().unwrap_or("").to_owned();
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let meta = params.get("_meta").cloned().unwrap_or(json!({}));
            call(
                stream,
                shared,
                &request,
                &tool,
                arguments,
                meta,
                token,
                Some(id),
            )
        }
        other => write(
            &stream,
            200,
            &[json_type],
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": other}})
                .to_string()
                .as_bytes(),
        ),
    }
}

fn follow(kind: Option<&str>, operation: &str, tool: &str, arguments: Value) -> Value {
    let mut call = json!({"operation": operation, "tool": tool, "arguments": arguments});
    if let Some(kind) = kind {
        call["kind"] = json!(kind);
    }
    call
}

/// What `tool` does with `arguments`, its effect report carrying `key`.
fn perform(
    state: &State,
    tool: &str,
    arguments: &Value,
    staged: bool,
    key: &str,
    n: u64,
) -> Outcome {
    let key_or_null = match key {
        "" => Value::Null,
        key => json!(key),
    };
    let report = |class: &str, operation: &str| {
        json!({"class": class, "operation": operation, "idempotency_key": key_or_null,
               "undo": null, "deadline_ms": null, "lookup": null})
    };
    let lookup = |operation: &str, tool: &str, extra: Value| {
        let mut arguments = json!({"key": key});
        if let (Some(a), Some(b)) = (arguments.as_object_mut(), extra.as_object()) {
            a.extend(b.clone());
        }
        json!({"by": "key", "operation": operation, "tool": tool, "arguments": arguments})
    };
    match tool {
        "slack__chat_post" => {
            let ts = n.to_string();
            let mut effect = report("reversible", "slack.chat.post");
            effect["undo"] = follow(
                Some("inverse"),
                "slack.chat.delete",
                "slack__chat_delete",
                json!({"ts": ts}),
            );
            effect["deadline_ms"] = json!(state.deadline_ms);
            if !state.no_compensation {
                effect["compensate"] = follow(
                    Some("compensate"),
                    "slack.chat.update",
                    "slack__chat_update",
                    json!({"ts": ts, "text": "(retracted)"}),
                );
                effect["compensate"]["deadline_ms"] = Value::Null;
            }
            Outcome::ok(json!(format!("posted ts={n}")), effect)
        }
        "slack__chat_delete" | "slack__chat_update" | "github__issues_close"
            if state.fail_inverse =>
        {
            Outcome::error("not_found", "it is already gone", None)
        }
        "slack__chat_delete" => Outcome::ok(
            json!("deleted"),
            report("irreversible", "slack.chat.delete"),
        ),
        "slack__chat_update" => Outcome::ok(
            json!("retracted"),
            report("irreversible", "slack.chat.update"),
        ),
        "github__issues_create" => {
            let mut effect = report("compensable", "github.issues.create");
            effect["undo"] = follow(
                Some("compensate"),
                "github.issues.update",
                "github__issues_close",
                json!({"number": n, "state": "closed"}),
            );
            Outcome::ok(json!(format!("issue #{n}")), effect)
        }
        "github__issues_close" if arguments["confirm"] != json!(true) => Outcome::error(
            "confirmation_required",
            "github.issues.update needs confirm: true",
            None,
        ),
        "github__issues_close" => Outcome::ok(
            json!("closed"),
            report("irreversible", "github.issues.update"),
        ),
        "github__issues_list" => Outcome::ok(json!([]), report("read", "github.issues.list")),
        "gmail__send" if staged => {
            let handle = format!("d-{n}");
            let mut effect = report("irreversible", "gmail.send");
            effect["staged"] = json!({"draft_operation": "gmail.drafts.create", "handle": handle,
                "promote": follow(None, "gmail.drafts.send", "gmail__drafts_send", json!({"id": handle})),
                "discard": follow(None, "gmail.drafts.delete", "gmail__drafts_delete", json!({"id": handle}))});
            Outcome::ok(json!(format!("draft {handle}")), effect)
        }
        "gmail__send" => {
            let mut effect = report("irreversible", "gmail.send");
            effect["lookup"] = lookup("gmail.sent.lookup", "gmail__sent_lookup", json!({}));
            Outcome::ok(json!("sent"), effect)
        }
        "gmail__drafts_send" => Outcome::ok(
            json!(format!("sent {}", arguments["id"].as_str().unwrap_or(""))),
            report("irreversible", "gmail.drafts.send"),
        ),
        "gmail__drafts_delete" => Outcome::ok(
            json!(format!(
                "deleted {}",
                arguments["id"].as_str().unwrap_or("")
            )),
            report("irreversible", "gmail.drafts.delete"),
        ),
        "blog__publish" if staged => {
            let handle = n;
            let mut effect = report("irreversible", "blog.posts.publish");
            effect["staged"] = json!({"draft_operation": "blog.posts.create", "handle": handle,
                "promote": follow(None, "blog.posts.update", "blog__posts_update",
                                  json!({"id": handle, "draft": false})),
                "discard": null});
            Outcome::ok(json!({"id": handle, "draft": true}), effect)
        }
        "blog__publish" => Outcome::ok(
            json!({"id": n}),
            report("irreversible", "blog.posts.publish"),
        ),
        "blog__posts_update" => Outcome::ok(
            json!({"id": arguments["id"], "draft": false}),
            report("irreversible", "blog.posts.update"),
        ),
        "webhook__fire" if staged => Outcome::error(
            "unsupported_operation",
            "webhook.fire has no draft form (effect/no_draft_form)",
            None,
        ),
        "webhook__fire" => Outcome::ok(json!("fired"), report("irreversible", "webhook.fire")),
        "legacy__do" => Outcome {
            data: json!("done"),
            error: None,
            effect: None,
        },
        "flaky__charge" => {
            let mut effect = report("compensable", "flaky.charges.create");
            effect["undo"] = follow(
                Some("compensate"),
                "flaky.refunds.create",
                "flaky__refund",
                json!({"charge": n}),
            );
            effect["lookup"] = lookup(
                "flaky.charges.lookup",
                "flaky__charge_lookup",
                json!({"kind": "charge"}),
            );
            Outcome::ok(json!("charged"), effect)
        }
        "gmail__sent_lookup" | "flaky__charge_lookup" => {
            let asked = arguments["key"].as_str().unwrap_or("");
            match state.happened.contains(asked) {
                true => Outcome::ok(json!({"key": asked}), report("read", "lookup")),
                false => {
                    Outcome::error("not_found", "no such call", Some(report("read", "lookup")))
                }
            }
        }
        _ => Outcome::error("not_found", "no such tool", None),
    }
}

#[allow(clippy::too_many_arguments)]
fn call(
    stream: TcpStream,
    shared: &Arc<(Mutex<State>, Condvar)>,
    request: &Request,
    tool: &str,
    arguments: Value,
    meta: Value,
    token: String,
    // The JSON-RPC id over MCP; `None` over REST.
    rpc: Option<Value>,
) {
    let tool = tool.to_owned();
    let key = request.header("idempotency-key").map(str::to_owned);
    let meta_key = meta["idempotency_key"].as_str().map(str::to_owned);
    let (lock, changed) = &**shared;
    let mut state = lock.lock().unwrap();
    let mut record = Call {
        tool: tool.clone(),
        arguments: arguments.clone(),
        meta: meta.clone(),
        key: key.clone(),
        rest: rpc.is_none(),
        replayed: false,
        token,
        seen: None,
    };
    if let Some(hook) = &state.hook {
        record.seen = hook(&record);
    }
    let staged = record.staged();
    let dedupe = key
        .clone()
        .or(meta_key.clone())
        .map(|k| format!("{tool}:{k}"));
    let replay = dedupe
        .as_ref()
        .and_then(|k| state.performed.get(k))
        .cloned();
    let outcome = match replay {
        Some(previous) => {
            record.replayed = true;
            state.calls.push(record);
            drop(state);
            return answer(&stream, rpc, &tool, &previous);
        }
        None => {
            state.next += 1;
            let effect_key = key.clone().or(meta_key.clone()).unwrap_or_default();
            let outcome = perform(&state, &tool, &arguments, staged, &effect_key, state.next);
            let lookup_tool = tool.ends_with("_lookup");
            if outcome.error.is_none() && !lookup_tool {
                if !effect_key.is_empty() && !staged {
                    state.happened.insert(effect_key);
                }
                if let Some(k) = dedupe {
                    state.performed.insert(k, outcome.clone());
                }
            }
            outcome
        }
    };
    let held = state.hold.contains(&tool);
    state.calls.push(record);
    changed.notify_all();
    if held {
        // Done upstream; the answer waits, and then is never sent.
        state.holding += 1;
        changed.notify_all();
        while state.hold.contains(&tool) {
            state = changed.wait(state).unwrap();
        }
        state.holding -= 1;
        return;
    }
    drop(state);
    if tool == "flaky__charge" {
        // The effect happened; the answer is lost.
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return;
    }
    answer(&stream, rpc, &tool, &outcome)
}

fn answer(stream: &TcpStream, rpc: Option<Value>, tool: &str, outcome: &Outcome) {
    let Some(id) = rpc else {
        let (status, headers, body) = outcome.rest();
        return write(stream, status, &headers, &body);
    };
    let message = json!({"jsonrpc": "2.0", "id": id, "result": outcome.mcp()});
    if tool == "slack__chat_post" {
        let sse = format!(
            "event: message\ndata: {}\n\nevent: message\ndata: {}\n\n",
            json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progress": 1}}),
            message
        );
        return write(
            stream,
            200,
            &[("Content-Type", "text/event-stream".into())],
            sse.as_bytes(),
        );
    }
    write(
        stream,
        200,
        &[("Content-Type", "application/json".into())],
        message.to_string().as_bytes(),
    );
}

/// What a harness runs to call the gateway through Anvil's wire, as the
/// packaged SDKs do: `python3 call.py TOOL 'JSON ARGUMENTS'` over MCP, or
/// `python3 call.py --rest TOOL 'JSON ARGUMENTS'` over the REST route.
/// Prints `call TOOL: TEXT` or `call TOOL error: TEXT`.
pub const CALL_PY: &str = r#"import json, os, sys, urllib.error, urllib.request
url = os.environ["ANVIL_GATEWAY_URL"]
token = open(os.environ["ANVIL_GATEWAY_TOKEN_FILE"]).read().strip()
headers = {"Authorization": "Bearer " + token, "Content-Type": "application/json",
           "Accept": "application/json, text/event-stream"}
args = sys.argv[1:]
rest = args[:1] == ["--rest"]
if rest:
    args = args[1:]
tool, arguments = args[0], json.loads(args[1] if len(args) > 1 else "{}")
if rest:
    base = url[:-len("/mcp")] if url.endswith("/mcp") else url
    request = urllib.request.Request(base + "/call/" + tool, method="POST",
                                     data=json.dumps({"arguments": arguments}).encode(), headers=headers)
    try:
        response = urllib.request.urlopen(request, timeout=600)
        print("call %s: %s" % (tool, response.read().decode()))
    except urllib.error.HTTPError as e:
        print("call %s error %d: %s" % (tool, e.code, e.read().decode()))
    except Exception as e:
        print("call %s failed: %s" % (tool, e))
    sys.exit(0)
def rpc(body, session=None):
    h = dict(headers)
    if session:
        h["Mcp-Session-Id"] = session
    request = urllib.request.Request(url, method="POST", data=json.dumps(body).encode(), headers=h)
    response = urllib.request.urlopen(request, timeout=600)
    text = response.read().decode()
    if response.headers.get("Content-Type", "").startswith("text/event-stream"):
        found = None
        for block in text.split("\n\n"):
            data = "\n".join(l[5:].lstrip() for l in block.splitlines() if l.startswith("data:"))
            if data:
                message = json.loads(data)
                if message.get("id") == body.get("id"):
                    found = message
        return found, response.headers.get("Mcp-Session-Id")
    return (json.loads(text) if text else None), response.headers.get("Mcp-Session-Id")
init, session = rpc({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}})
rpc({"jsonrpc": "2.0", "method": "notifications/initialized"}, session)
try:
    answer, _ = rpc({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                     "params": {"name": tool, "arguments": arguments}}, session)
except Exception as e:
    print("call %s failed: %s" % (tool, e))
    sys.exit(0)
if "error" in answer:
    print("call %s rpc error: %s" % (tool, answer["error"]["message"]))
else:
    result = answer["result"]
    text = "".join(c.get("text", "") for c in result.get("content", []))
    print("call %s%s: %s" % (tool, " error" if result.get("isError") else "", text))
"#;
