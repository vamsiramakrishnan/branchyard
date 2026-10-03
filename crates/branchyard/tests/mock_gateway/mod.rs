//! A mock connector gateway on loopback for the effect ledger's tests: MCP
//! Streamable HTTP (JSON, and one tool answered as an event stream), the
//! effect metadata of `docs/effects.md#the-wire` (`_meta.effect` with
//! class, undo, deadline and summary), idempotency keys honoured (a key
//! seen before replays its answer without doing anything again), drafts
//! (`_meta.stage`) and promotion (`_meta.promote`), lookups, and every
//! call recorded. Shared by the SDK, CLI and server tests through
//! `#[path]`.

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
    pub meta: Value,
    /// The `Idempotency-Key` header.
    pub key: Option<String>,
    /// Whether it replayed an earlier answer for the same key.
    pub replayed: bool,
    pub token: String,
    /// What the hook saw when the call arrived.
    pub seen: Option<String>,
}

type Hook = Box<dyn Fn(&Call) -> Option<String> + Send>;

#[derive(Default)]
struct State {
    calls: Vec<Call>,
    /// Answers by idempotency key.
    performed: HashMap<String, (u16, Value)>,
    /// Keys whose effect happened, for lookups.
    happened: HashSet<String>,
    next: u64,
    deadline_ms: u64,
    fail_inverse: bool,
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

/// The tools it lists, as Anvil would declare them.
pub fn tools() -> Value {
    json!([
        {"name": "slack__chat_post", "_meta": {"effect": {"class": "reversible", "operation": "slack.chat.post"}}},
        {"name": "slack__chat_delete", "_meta": {"effect": {"class": "reversible", "deletion": true}}},
        {"name": "github__issues_create", "_meta": {"effect": {"class": "compensable"}}},
        {"name": "github__issues_close", "_meta": {"effect": {"class": "compensable"}}},
        {"name": "github__issues_list", "annotations": {"readOnlyHint": true}},
        {"name": "gmail__send", "_meta": {"effect": {"class": "irreversible", "draft": true,
            "lookup": {"operation": "sent_lookup"}}}},
        {"name": "gmail__sent_lookup", "annotations": {"readOnlyHint": true}},
        {"name": "webhook__fire", "_meta": {"effect": {"class": "irreversible"}}},
        {"name": "legacy__do"},
        {"name": "flaky__charge", "_meta": {"effect": {"class": "compensable",
            "lookup": {"operation": "charge_lookup", "arguments": {"kind": "charge"}}}}},
        {"name": "flaky__charge_lookup", "annotations": {"readOnlyHint": true}},
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

    /// The deadline every undo it describes carries.
    pub fn set_deadline(&self, ms: u64) {
        self.lock().deadline_ms = ms;
    }

    /// Make every inverse (`chat_delete`, `issues_close`) fail.
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
    let method = line.split_whitespace().next()?.to_owned();
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

fn tool_result(text: &str, error: bool, effect: Option<Value>) -> Value {
    let mut result = json!({"content": [{"type": "text", "text": text}], "isError": error});
    if let Some(effect) = effect {
        result["_meta"] = json!({"effect": effect});
    }
    result
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
        "tools/call" => call(stream, shared, &request, &body, token),
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

fn call(
    stream: TcpStream,
    shared: &Arc<(Mutex<State>, Condvar)>,
    request: &Request,
    body: &Value,
    token: String,
) {
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let params = &body["params"];
    let tool = params["name"].as_str().unwrap_or("").to_owned();
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let meta = params.get("_meta").cloned().unwrap_or(json!({}));
    let key = request.header("idempotency-key").map(str::to_owned);
    let meta_key = meta["idempotency_key"].as_str().map(str::to_owned);
    let (lock, changed) = &**shared;
    let mut state = lock.lock().unwrap();
    let mut record = Call {
        tool: tool.clone(),
        arguments: arguments.clone(),
        meta: meta.clone(),
        key: key.clone(),
        replayed: false,
        token,
        seen: None,
    };
    if let Some(hook) = &state.hook {
        record.seen = hook(&record);
    }
    let dedupe = key
        .clone()
        .or(meta_key.clone())
        .map(|k| format!("{tool}:{k}"));
    if let Some(previous) = dedupe
        .as_ref()
        .and_then(|k| state.performed.get(k))
        .cloned()
    {
        record.replayed = true;
        state.calls.push(record);
        drop(state);
        return write(
            &stream,
            previous.0,
            &[("Content-Type", "application/json".into())],
            json!({"jsonrpc": "2.0", "id": id, "result": previous.1})
                .to_string()
                .as_bytes(),
        );
    }
    state.next += 1;
    let n = state.next;
    let deadline = state.deadline_ms;
    let effect_key = key.clone().or(meta_key.clone()).unwrap_or_default();
    let lookup = |state: &State| {
        let asked = arguments["idempotency_key"].as_str().unwrap_or("");
        let found = state.happened.contains(asked);
        tool_result(
            if found { "found" } else { "not found" },
            false,
            Some(
                json!({"class": "read", "lookup": match (found, tool.as_str()) {
                    (true, "flaky__charge_lookup") => json!({"found": true, "class": "compensable",
                        "undo": {"operation": "refund", "arguments": {"key": asked}}}),
                    _ => json!({"found": found}),
                }}),
            ),
        )
    };
    let result = match tool.as_str() {
        "slack__chat_post" => tool_result(
            &format!("posted ts={n}"),
            false,
            Some(
                json!({"class": "reversible", "deadline_ms": deadline, "idempotency_key": effect_key,
                "summary": format!("message in #{}", arguments["channel"].as_str().unwrap_or("general")),
                "undo": {"operation": "chat_delete", "arguments": {"ts": n.to_string()},
                         "summary": format!("deletable until the deadline")}}),
            ),
        ),
        "slack__chat_delete" | "github__issues_close" if state.fail_inverse => tool_result(
            "{\"error\": {\"code\": \"not_found\", \"message\": \"it is already gone\"}}",
            true,
            None,
        ),
        "slack__chat_delete" => tool_result(
            "deleted",
            false,
            Some(json!({"class": "reversible", "undo": null})),
        ),
        "github__issues_create" => tool_result(
            &format!("issue #{n}"),
            false,
            Some(
                json!({"class": "compensable", "summary": format!("issue #{n}"),
                "undo": {"operation": "issues_close", "arguments": {"number": n}, "kind": "compensate",
                         "summary": format!("issue #{n} will be closed, not deleted")}}),
            ),
        ),
        "github__issues_close" => tool_result(
            "closed",
            false,
            Some(json!({"class": "compensable", "undo": null})),
        ),
        "github__issues_list" => tool_result("[]", false, None),
        "gmail__send" if meta["stage"] == json!(true) => tool_result(
            &format!("draft d-{n}"),
            false,
            Some(
                json!({"class": "irreversible", "staged": {"handle": format!("d-{n}")}, "undo": null}),
            ),
        ),
        "gmail__send" => tool_result(
            "sent",
            false,
            Some(
                json!({"class": "irreversible", "undo": null, "summary": "email to finance@",
                        "idempotency_key": effect_key}),
            ),
        ),
        "gmail__sent_lookup" | "flaky__charge_lookup" => lookup(&state),
        "webhook__fire" => tool_result(
            "fired",
            false,
            Some(json!({"class": "irreversible", "undo": null})),
        ),
        "legacy__do" => tool_result("done", false, None),
        "flaky__charge" => tool_result(
            "charged",
            false,
            Some(json!({"class": "compensable",
            "undo": {"operation": "refund", "arguments": {"charge": n}}})),
        ),
        _ => tool_result("no such tool", true, None),
    };
    let ok = result["isError"] != json!(true);
    let lookup_tool = tool.ends_with("_lookup");
    if ok && !lookup_tool && !effect_key.is_empty() && meta["stage"] != json!(true) {
        state.happened.insert(effect_key.clone());
    }
    if let Some(k) = dedupe {
        if ok && !lookup_tool {
            state.performed.insert(k, (200, result.clone()));
        }
    }
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
    let message = json!({"jsonrpc": "2.0", "id": id, "result": result});
    if tool == "slack__chat_post" {
        let sse = format!(
            "event: message\ndata: {}\n\nevent: message\ndata: {}\n\n",
            json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progress": 1}}),
            message
        );
        return write(
            &stream,
            200,
            &[("Content-Type", "text/event-stream".into())],
            sse.as_bytes(),
        );
    }
    write(
        &stream,
        200,
        &[("Content-Type", "application/json".into())],
        message.to_string().as_bytes(),
    );
}

/// What a harness runs to call the gateway through Anvil's wire, as the
/// packaged SDKs do: `python3 call.py TOOL 'JSON ARGUMENTS'`. Prints
/// `call TOOL: TEXT` or `call TOOL error: TEXT`.
pub const CALL_PY: &str = r#"import json, os, sys, urllib.request
url = os.environ["ANVIL_GATEWAY_URL"]
token = open(os.environ["ANVIL_GATEWAY_TOKEN_FILE"]).read().strip()
headers = {"Authorization": "Bearer " + token, "Content-Type": "application/json",
           "Accept": "application/json, text/event-stream"}
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
tool, arguments = sys.argv[1], json.loads(sys.argv[2] if len(sys.argv) > 2 else "{}")
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
